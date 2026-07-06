//! The request pipeline. See spec §5.
//!
//! One `Pipeline::run()` call processes one chat completion request: it resolves
//! the combo into concrete (provider, model, account) targets, expands account
//! rotation, executes the first eligible target with bounded timeouts, and
//! records a usage row.

use crate::circuit_breaker::CircuitBreakerRegistry;
use crate::combos::{Combo, SelectionRegistry};
use crate::compression::stats::CompressionStats;
use crate::config::{RacingConfig, RetriesConfig};
use crate::error::CoreError;
use crate::ids::{ApiKeyId, ComboId, RequestId, TraceId};
use crate::secrets::MasterKey;
use crate::timeouts::Timeouts;
use crate::translation::OpenAIResponse;
use crate::upstream::UpstreamClient;
use parking_lot::RwLock;
use rusqlite::Connection;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

pub mod repository;
pub mod service;
pub mod context;
pub mod stage;
pub mod quotas;
pub mod racing;
pub mod stages;
pub mod worker;
mod execution;
mod streaming;

#[cfg(test)]
pub mod test_utils;
#[cfg(test)]
mod tests;

pub const SSE_DONE_BYTES: bytes::Bytes = bytes::Bytes::from_static(b"data: [DONE]\n\n");

/// Per-call knobs the pipeline reads from the surrounding `AppConfig`.
#[derive(Clone)]
pub struct PipelineConfig {
    pub defaults: Timeouts,
    pub racing: RacingConfig,
    pub retries: RetriesConfig,
    pub max_attempts: u8,
    pub master_key: Arc<MasterKey>,
    pub adapters: Arc<Vec<Arc<dyn crate::adapters::ProviderAdapter>>>,
    pub http_client: reqwest::Client,
    pub cooldown_secs: u64,
    pub cooldown_max_secs: u64,
    pub cooldown_factor: u32,
    pub upstream_client: Arc<UpstreamClient>,
    pub oauth_provider_registry: Option<Arc<crate::oauth::OAuthProviderRegistry>>,
    pub compression_mode: crate::compression::CompressionMode,
    pub idle_chunk_retryable: bool,
    pub quota_protection: crate::config::QuotaProtectionConfig,
    pub background_tx: tokio::sync::mpsc::Sender<crate::pipeline::worker::BackgroundJob>,
}

/// All the input needed to process a single chat completion.
#[derive(Debug, Clone)]
pub struct PipelineRequest {
    pub request_id: RequestId,
    pub trace_id: TraceId,
    pub combo_id: ComboId,
    pub openai_request: Arc<crate::translation::OpenAIRequest>,
    pub client_disconnected: tokio::sync::watch::Receiver<bool>,
    pub stream_sink: Option<crate::race_sink::StreamSink>,
    pub api_key_id: Option<ApiKeyId>,
    pub race_cancel: Option<crate::upstream::CancellationToken>,
    pub combo_override: Option<Combo>,
    pub targets_override: Option<Vec<crate::combos::ComboTarget>>,
    pub request_headers: std::collections::BTreeMap<String, String>,
    pub request_body_json: Option<Arc<serde_json::Value>>,
    pub race_cancelled: bool,
    pub endpoint_kind: crate::endpoint::EndpointKind,
}

/// Outcome of a single `Pipeline::run()` call.
#[derive(Debug)]
pub struct PipelineResult {
    pub status_code: u16,
    pub error: Option<CoreError>,
    pub final_response: Option<OpenAIResponse>,
    pub attempts: u8,
    pub usage_tuple: Option<(String, u8, crate::ids::ComboTargetId)>,
}

/// Bundle of "what kind of failure" inputs for [`Pipeline::record_and_fail`]
/// and [`Pipeline::record_and_fail_with_trace_id`].
pub struct FailureContext<'a> {
    pub attempt: u8,
    pub race_size: u8,
    pub err: &'a CoreError,
    pub started: std::time::Instant,
    pub model: Option<&'a crate::models::Model>,
    pub connect_ms: Option<u64>,
    pub ttft_ms: Option<u64>,
    pub status_code: u16,
}

/// Orchestrates a single request end-to-end.
#[derive(Clone)]
pub struct Pipeline {
    pub(crate) conn: Arc<parking_lot::Mutex<Connection>>,
    pub(crate) config: PipelineConfig,
    pub(crate) circuit_breaker: CircuitBreakerRegistry,
    pub(crate) rr_counters: Arc<parking_lot::Mutex<HashMap<ComboId, u64>>>,
    pub(crate) selection_registry: Arc<SelectionRegistry>,
    pub(crate) record_bodies_and_headers: Arc<AtomicBool>,
    pub(crate) compression_stats_cell: Arc<RwLock<Option<CompressionStats>>>,
}

impl Pipeline {
    pub fn repo(&self) -> repository::SqlitePipelineRepository {
        repository::SqlitePipelineRepository::new(self.conn.clone())
    }

    pub fn new(conn: Arc<parking_lot::Mutex<Connection>>, config: PipelineConfig) -> Self {
        Self::with_recording_flag(conn, config, Arc::new(AtomicBool::new(false)))
    }

    pub fn with_recording_flag(
        conn: Arc<parking_lot::Mutex<Connection>>,
        config: PipelineConfig,
        record_bodies_and_headers: Arc<AtomicBool>,
    ) -> Self {
        Self::with_selection_registry(
            conn,
            config,
            record_bodies_and_headers,
            Arc::new(SelectionRegistry::new()),
            CircuitBreakerRegistry::new(&crate::config::CircuitBreakerConfig {
                failure_threshold: 5,
                unhealthy_duration_ms: 60_000,
            }),
        )
    }

    pub fn with_selection_registry(
        conn: Arc<parking_lot::Mutex<Connection>>,
        config: PipelineConfig,
        record_bodies_and_headers: Arc<AtomicBool>,
        selection_registry: Arc<SelectionRegistry>,
        circuit_breaker: CircuitBreakerRegistry,
    ) -> Self {
        Self {
            conn,
            config,
            circuit_breaker,
            rr_counters: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            selection_registry,
            record_bodies_and_headers,
            compression_stats_cell: Arc::new(RwLock::new(None)),
        }
    }

    pub fn selection_registry(&self) -> &Arc<SelectionRegistry> {
        &self.selection_registry
    }

    pub fn is_recording(&self) -> bool {
        self.record_bodies_and_headers.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn prune_circuit_breaker_idle(&self, max_idle: std::time::Duration) -> usize {
        self.circuit_breaker.prune_idle(max_idle)
    }

    pub fn circuit_breaker_len(&self) -> usize {
        self.circuit_breaker.len()
    }

    pub fn set_recording(&self, enabled: bool) {
        self.record_bodies_and_headers
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Phase label for tracing/debug.
#[derive(Debug, Clone, Copy)]
pub enum ErrorPhase {
    Resolve,
    Route,
    Retry,
}

impl std::fmt::Display for ErrorPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ErrorPhase::Resolve => "resolve",
            ErrorPhase::Route => "route",
            ErrorPhase::Retry => "retry",
        };
        write!(f, "{}", s)
    }
}

pub(crate) fn is_upstream_health_issue(err: &CoreError) -> bool {
    match err {
        CoreError::UpstreamTimeout { phase, .. } => phase != "idle_chunk",
        CoreError::UpstreamConnection(_) => true,
        CoreError::RateLimited { .. } => true,
        CoreError::UpstreamError { status, .. } => *status >= 500,
        _ => false,
    }
}

pub(crate) fn parse_retry_after_ms(val: &str) -> Option<u64> {
    const MAX_RETRY_AFTER_MS: u64 = 5 * 60 * 1000; // 5 minutes
    let trimmed = val.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(secs) = trimmed.parse::<f64>() {
        if !secs.is_finite() || secs < 0.0 {
            return None;
        }
        let ms = (secs * 1000.0) as u64;
        return Some(ms.min(MAX_RETRY_AFTER_MS));
    }
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc2822(trimmed) {
        let now = chrono::Utc::now();
        if parsed.with_timezone(&chrono::Utc) <= now {
            return Some(0);
        }
        let delta = (parsed.with_timezone(&chrono::Utc) - now)
            .num_milliseconds()
            .max(0) as u64;
        return Some(delta.min(MAX_RETRY_AFTER_MS));
    }
    None
}
