use crate::circuit_breaker::CircuitBreakerRegistry;
use crate::timeouts::Timeouts;
use crate::translation::OpenAIResponse;
use chrono;
use openproxy_adapters::upstream::UpstreamClient;
use openproxy_compression::stats::CompressionStats;
use openproxy_db::secrets::MasterKey;
use openproxy_types::SelectionRegistry;
use openproxy_types::combos::{Combo, ComboTarget};
use openproxy_types::config::{RacingConfig, RetriesConfig};
use openproxy_types::error::CoreError;
use openproxy_types::ids::{ApiKeyId, ComboId, RequestId, TraceId};
use parking_lot::RwLock;
use rusqlite::Connection;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

pub const SSE_DONE_BYTES: bytes::Bytes = bytes::Bytes::from_static(b"data: [DONE]\n\n");

#[derive(Clone)]
pub struct PipelineConfig {
    pub defaults: Timeouts,
    pub racing: RacingConfig,
    pub retries: RetriesConfig,
    pub max_attempts: u8,
    pub master_key: Arc<MasterKey>,
    pub adapters: Arc<Vec<openproxy_adapters::adapters::ProviderAdapterEnum>>,
    pub cooldown_secs: u64,
    pub cooldown_max_secs: u64,
    pub cooldown_factor: u32,
    pub upstream_client: Arc<UpstreamClient>,
    pub oauth_provider_registry: Option<Arc<dyn crate::oauth::PipelineOAuthRegistry>>,
    pub compression_mode: openproxy_compression::CompressionMode,
    pub idle_chunk_retryable: bool,
    pub quota_protection: openproxy_types::config::QuotaProtectionConfig,
    pub background_tx: tokio::sync::mpsc::Sender<crate::worker::BackgroundJob>,
}

#[derive(Debug, Clone)]
pub struct PipelineRequest {
    pub request_id: RequestId,
    pub trace_id: TraceId,
    pub combo_id: ComboId,
    pub openai_request: Arc<openproxy_types::OpenAIRequest>,
    pub client_disconnected: tokio::sync::watch::Receiver<Option<openproxy_types::CancelReason>>,
    pub stream_sink: Option<crate::race_sink::StreamSink>,
    pub api_key_id: Option<ApiKeyId>,
    pub race_cancel: Option<openproxy_adapters::upstream::CancellationToken>,
    pub combo_override: Option<Combo>,
    pub targets_override: Option<Vec<openproxy_types::combos::ComboTarget>>,
    pub request_headers: std::collections::BTreeMap<String, String>,
    pub request_body_json: Option<bytes::Bytes>,
    pub race_cancelled: bool,
    pub endpoint_kind: openproxy_types::endpoint::EndpointKind,
    pub compressed_messages: Arc<std::sync::OnceLock<Option<Vec<openproxy_types::OpenAIMessage>>>>,
    pub proxy_override: Option<(String, String)>,
}

#[derive(Debug)]
pub struct PipelineResult {
    pub status_code: u16,
    pub error: Option<CoreError>,
    pub final_response: Option<OpenAIResponse>,
    pub attempts: u8,
    pub usage_tuple: Option<(String, u8, openproxy_types::ids::ComboTargetId)>,
}

pub struct FailureContext<'a> {
    pub attempt: u8,
    pub race_size: u8,
    pub err: &'a CoreError,
    pub started: std::time::Instant,
    pub model: Option<&'a openproxy_types::models::Model>,
    pub connect_ms: Option<u64>,
    pub ttft_ms: Option<u64>,
    pub status_code: u16,
    pub proxy_url: Option<String>,
    pub proxy_status: Option<String>,
}

pub struct SingleExecutionParams<'a> {
    pub req: PipelineRequest,
    pub combo: &'a Combo,
    pub resolved_target: &'a crate::context::ResolvedTarget,
    pub attempt: u8,
    pub race_size: u8,
    pub total_targets: u8,
    pub race_cancel: &'a openproxy_adapters::upstream::CancellationToken,
}

pub struct PartialFailureParams<'a> {
    pub req: PipelineRequest,
    pub combo: &'a Combo,
    pub target: &'a ComboTarget,
    pub ctx: FailureContext<'a>,
    pub trace_id: String,
    pub acc: Option<&'a crate::sse_accumulator::ResponseAccumulator>,
    pub chunk_id: Option<&'a str>,
    pub created: u64,
    pub model_name: &'a str,
}

#[derive(Clone)]
pub struct Pipeline {
    pub(crate) conn: Arc<parking_lot::Mutex<Connection>>,
    pub(crate) config: PipelineConfig,
    pub(crate) circuit_breaker: CircuitBreakerRegistry,
    pub(crate) rr_counters: Arc<dashmap::DashMap<ComboId, std::sync::atomic::AtomicU64>>,
    pub(crate) selection_registry: Arc<SelectionRegistry>,
    pub(crate) record_bodies_and_headers: Arc<AtomicBool>,
    pub(crate) compression_stats_cell: Arc<RwLock<Option<CompressionStats>>>,
    pub predictive_limiter: Arc<crate::predictive_rate_limit::PredictiveRateLimiter>,
    pub tracker: crate::usage_tracker::UsageTracker,
    pub dispatcher: crate::upstream_dispatcher::UpstreamDispatcher,
    pub(crate) repo: Arc<dyn crate::repository::PipelineRepository>,
}

impl Pipeline {
    pub fn repo(&self) -> Arc<dyn crate::repository::PipelineRepository> {
        Arc::clone(&self.repo)
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
            CircuitBreakerRegistry::new(&openproxy_types::config::CircuitBreakerConfig {
                failure_threshold: 5,
                unhealthy_duration_ms: 60_000,
            }),
            Arc::new(crate::predictive_rate_limit::PredictiveRateLimiter::new()),
        )
    }

    pub fn with_selection_registry(
        conn: Arc<parking_lot::Mutex<Connection>>,
        config: PipelineConfig,
        record_bodies_and_headers: Arc<AtomicBool>,
        selection_registry: Arc<SelectionRegistry>,
        circuit_breaker: CircuitBreakerRegistry,
        predictive_limiter: Arc<crate::predictive_rate_limit::PredictiveRateLimiter>,
    ) -> Self {
        let compression_stats_cell = Arc::new(RwLock::new(None));
        let repo = Arc::new(crate::repository::SqlitePipelineRepository::new(
            Arc::clone(&conn),
        ));
        let tracker = crate::usage_tracker::UsageTracker {
            conn: Arc::clone(&conn),
            background_tx: config.background_tx.clone(),
            record_bodies_and_headers: Arc::clone(&record_bodies_and_headers),
            compression_stats_cell: Arc::clone(&compression_stats_cell),
            selection_registry: Arc::clone(&selection_registry),
            cooldown_secs: config.cooldown_secs,
            cooldown_max_secs: config.cooldown_max_secs,
            cooldown_factor: config.cooldown_factor,
            repo: Arc::clone(&repo) as Arc<dyn crate::repository::PipelineRepository>,
        };
        let dispatcher = crate::upstream_dispatcher::UpstreamDispatcher::new(
            Arc::clone(&conn),
            config.clone(),
            tracker.clone(),
            Arc::clone(&record_bodies_and_headers),
        );
        Self {
            conn,
            config,
            circuit_breaker,
            rr_counters: Arc::new(dashmap::DashMap::new()),
            selection_registry,
            record_bodies_and_headers,
            compression_stats_cell,
            predictive_limiter,
            tracker,
            dispatcher,
            repo,
        }
    }

    pub fn selection_registry(&self) -> &Arc<SelectionRegistry> {
        &self.selection_registry
    }

    pub fn is_recording(&self) -> bool {
        self.record_bodies_and_headers
            .load(std::sync::atomic::Ordering::Relaxed)
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
        write!(f, "{s}")
    }
}

pub fn is_upstream_health_issue(err: &CoreError) -> bool {
    match err {
        CoreError::UpstreamTimeout { phase, .. } => phase != "idle_chunk",
        CoreError::UpstreamConnection(_) => true,
        CoreError::RateLimited { .. } => true,
        CoreError::UpstreamError { status, .. } => *status >= 500 || *status == 429,
        _ => false,
    }
}

fn parse_retry_after_seconds(trimmed: &str) -> Option<u64> {
    let secs = trimmed.parse::<f64>().ok()?;
    (secs.is_finite() && secs >= 0.0).then_some((secs * 1000.0) as u64)
}

fn parse_retry_after_date(trimmed: &str) -> Option<u64> {
    let parsed = chrono::DateTime::parse_from_rfc2822(trimmed).ok()?;
    let now = chrono::Utc::now();
    let target = parsed.with_timezone(&chrono::Utc);
    let diff_ms = (target - now).num_milliseconds().max(0) as u64;
    Some(diff_ms)
}

pub fn parse_retry_after_ms(val: &str) -> Option<u64> {
    let trimmed = val.trim();
    if trimmed.is_empty() {
        return None;
    }
    parse_retry_after_seconds(trimmed).or_else(|| parse_retry_after_date(trimmed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_retry_after_ms() {
        // Empty string
        assert_eq!(parse_retry_after_ms(""), None);
        assert_eq!(parse_retry_after_ms("   "), None);

        // Float / Int seconds
        assert_eq!(parse_retry_after_ms("10"), Some(10000));
        assert_eq!(parse_retry_after_ms("1.5"), Some(1500));
        assert_eq!(parse_retry_after_ms("0"), Some(0));

        // Invalid strings
        assert_eq!(parse_retry_after_ms("abc"), None);
        assert_eq!(parse_retry_after_ms("-1"), None);

        // Date parsing is hard to test deterministically for future without mocking time,
        // but we can test past dates returning 0
        let past_date = "Fri, 31 Dec 1999 23:59:59 GMT";
        assert_eq!(parse_retry_after_ms(past_date), Some(0));

        // test valid future date parsing logic roughly
        let future_date = (chrono::Utc::now() + chrono::Duration::seconds(10)).to_rfc2822();
        let res = parse_retry_after_ms(&future_date).unwrap();
        // Since some time passes between now and parsing, it's roughly 10000
        assert!(res > 8000 && res <= 10000);
    }
}
