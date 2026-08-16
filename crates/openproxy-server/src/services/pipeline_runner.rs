//! Pipeline runner service helper.
//!
//! Encapsulates:
//! 1. Constructing a [`Pipeline`] instance from [`AppState`].
//! 2. Preparing a [`PipelineRequest`] along with watchdog timers, cancellation
//!    channels, and streaming channels.

use axum::http::HeaderMap;
use bytes::Bytes;
use openproxy_pipeline::redact::redact_sensitive_headers;
use openproxy_pipeline::{Pipeline, PipelineConfig, PipelineRequest, StreamSink};
use openproxy_types::combos::{Combo, ComboTarget};
use openproxy_types::ids::{ApiKeyId, ComboId, RequestId, TraceId};
use openproxy_types::{CancelReason, EndpointKind, OpenAIRequest};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, watch};

use crate::{disconnect::CancelWatch, state::AppState};

pub struct PreparedPipelineRequest {
    pub req: PipelineRequest,
    pub done_tx: oneshot::Sender<()>,
    pub stream_rx: mpsc::Receiver<Bytes>,
}

pub struct PrepareRequestParams<'a> {
    pub state: &'a AppState,
    pub headers: &'a HeaderMap,
    pub cancel: CancelWatch,
    pub openai_req: Arc<OpenAIRequest>,
    pub raw_request_body: Bytes,
    pub api_key_id: Option<ApiKeyId>,
    pub combo_id: ComboId,
    pub combo_override: Option<Combo>,
    pub targets_override: Option<Vec<ComboTarget>>,
    pub endpoint_kind: EndpointKind,
}

pub struct PipelineRunner;

impl PipelineRunner {
    /// Build a configured [`Pipeline`] from the current [`AppState`].
    pub fn build_pipeline(state: &AppState) -> Pipeline {
        let config = PipelineConfig {
            defaults: openproxy_pipeline::timeouts::Timeouts::from_config(&state.timeouts()),
            racing: state.config().racing.clone(),
            retries: state.config().retries,
            max_attempts: state.config().retries.max_attempts,
            master_key: Arc::clone(state.master_key()),
            adapters: state.adapters(),
            cooldown_secs: state.config().cooldown.cooldown_secs,
            cooldown_max_secs: state.config().cooldown.max_secs,
            cooldown_factor: state.config().cooldown.factor,
            upstream_client: Arc::clone(state.upstream_client()),
            oauth_provider_registry: Some(state.oauth_provider_registry()),
            compression_mode: state.compression_mode(),
            idle_chunk_retryable: state.idle_chunk_retryable(),
            quota_protection: state.quota_protection(),
            background_tx: state.background_tx(),
        };
        Pipeline::with_selection_registry(
            state.db_pool().writer_arc(),
            config,
            state.record_bodies_and_flags(),
            state.selection_registry(),
            state.circuit_breaker(),
        )
    }

    /// Calculate watchdog budget in milliseconds respecting `x-request-deadline-ms` header.
    pub fn calculate_watchdog_budget(state: &AppState, headers: &HeaderMap) -> u64 {
        let client_deadline_ms: Option<u64> = headers
            .get("x-request-deadline-ms")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|ms| *ms > 0);
        let total_ms = state.timeouts().total_ms;
        match client_deadline_ms {
            Some(client_ms) if client_ms < total_ms => {
                tracing::debug!(
                    client_ms,
                    total_ms,
                    "client requested shorter cancellation deadline than upstream total"
                );
                client_ms
            }
            _ => total_ms,
        }
    }

    /// Spawn a background task that sends a watchdog timeout cancel reason if
    /// `done_rx` is not notified before `budget_ms` elapses.
    pub fn spawn_watchdog(
        done_rx: oneshot::Receiver<()>,
        watchdog_tx: watch::Sender<Option<CancelReason>>,
        budget_ms: u64,
    ) {
        tokio::spawn(async move {
            tokio::select! {
                _ = done_rx => {}
                () = tokio::time::sleep(std::time::Duration::from_millis(budget_ms)) => {
                    tracing::warn!(
                        budget_ms,
                        "watchdog timer fired — cancelling pipeline (this is a total-budget timeout, NOT a client disconnect)"
                    );
                    let _ = watchdog_tx.send(Some(CancelReason::WatchdogTimeout));
                }
            }
        });
    }

    /// Prepare a [`PipelineRequest`] and wire watchdog + cancellation + streaming sinks.
    pub fn prepare_request(params: PrepareRequestParams<'_>) -> PreparedPipelineRequest {
        let PrepareRequestParams {
            state,
            headers,
            cancel,
            openai_req,
            raw_request_body,
            api_key_id,
            combo_id,
            combo_override,
            targets_override,
            endpoint_kind,
        } = params;
        let request_id = RequestId::new();
        let trace_id = TraceId::new();

        let watchdog_budget_ms = Self::calculate_watchdog_budget(state, headers);
        let (tx, rx) = mpsc::channel(64);
        let CancelWatch {
            tx: watchdog_tx,
            rx: client_disconnected,
        } = cancel;

        let stream_sink = if openai_req.stream {
            Some(StreamSink::Direct(tx))
        } else {
            Some(StreamSink::Discard)
        };

        let (done_tx, done_rx) = oneshot::channel::<()>();
        Self::spawn_watchdog(done_rx, watchdog_tx, watchdog_budget_ms);

        let req = PipelineRequest {
            request_id,
            trace_id,
            combo_id,
            openai_request: openai_req,
            client_disconnected,
            stream_sink,
            api_key_id,
            combo_override,
            targets_override,
            request_headers: redact_sensitive_headers(headers),
            request_body_json: Some(raw_request_body),
            race_cancelled: false,
            race_cancel: None,
            endpoint_kind,
            compressed_messages: Arc::new(std::sync::OnceLock::new()),
            proxy_override: None,
        };

        PreparedPipelineRequest {
            req,
            done_tx,
            stream_rx: rx,
        }
    }
}
