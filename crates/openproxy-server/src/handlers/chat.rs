//! `POST /v1/chat/completions` — the public entry point.
//!
//! Spec §2.1 describes the contract:
//! 1. Parse the incoming JSON as an [`OpenAIRequest`].
//! 2. Resolve the routing plan from the `model` field via
//!    [`openproxy_core::routing::resolve`]. A model that matches a
//!    row in the `models` table goes direct (via a synthetic
//!    single-target combo); a `combo:<name>` matches a combo; anything
//!    else is 404.
//! 3. The optional `x-openproxy-combo` header is a legacy override
//!    that forces a specific combo, bypassing model resolution.
//! 4. Hand the resolved plan + the parsed request to the
//!    [`Pipeline`] which dispatches it to the configured upstream,
//!    with retries, timeouts, and usage writes.
//! 5. Translate the pipeline's [`PipelineResult`] back into either
//!    an OpenAI-shaped JSON response or a structured error.
//!
//! Streaming (`stream: true` in the request body) is intentionally
//! not wired up: the MVP is non-streaming and the pipeline's SSE
//! plumbing is a follow-up.

use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use bytes::Bytes;
use futures::stream::Stream;
use openproxy_core::{
    ids::{ApiKeyId, RequestId, TraceId},
    pipeline::{Pipeline, PipelineConfig, PipelineRequest},
    redact::redact_sensitive_headers,
};
use serde_json::json;
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use std::time::Instant;
use tokio_stream::wrappers::ReceiverStream;

use crate::{
    disconnect::CancelWatch,
    error::ApiError,
    middleware::auth::{ParsedChatRequest, ValidatedApiToken},
    state::AppState,
};

/// SSE keepalive interval. Sends `: keep-alive\n\n` (an SSE comment)
/// periodically to keep the connection alive while the upstream is
/// generating. This is critical for streaming requests where the
/// upstream takes a long time to produce the first token (e.g. large
/// prompts, reasoning models). Without frequent keepalives,
/// intermediate proxies (nginx, cloudflare) and client HTTP
/// libraries may close the connection due to inactivity, causing
/// false-positive "client disconnected" errors.
///
/// CRITICAL: the first keepalive is DELAYED by this interval (not
/// sent immediately). The previous code used `tokio::time::interval`
/// which fires on the FIRST tick (immediately), sending `: keep-alive\n\n`
/// as the VERY FIRST bytes of the response body — before any `data: {...}`
/// frame. Some SSE clients (notably the OpenAI Python library's httpx-sse
/// parser) may not handle a leading SSE comment correctly and close the
/// connection. Using `interval_at` with a delayed start ensures the first
/// keepalive only fires after `SSE_KEEPALIVE_INTERVAL` of inactivity,
/// giving the upstream time to send the first real data frame.
const SSE_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);

/// A stream that yields pre-formatted SSE frames (`Bytes`) from an
/// mpsc channel, interleaved with periodic SSE keepalive comments.
/// Unlike `axum::response::Sse`, this writes raw `Bytes` directly to
/// the HTTP body with zero additional wrapping — the pipeline already
/// formats each chunk as `data: {payload}\n\n`.
struct SseBytesStream {
    inner: futures::stream::SelectAll<ReceiverStream<Bytes>>,
    keepalive: tokio::time::Interval,
}

impl Stream for SseBytesStream {
    type Item = Result<Bytes, Infallible>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // All fields are `Unpin`, so `get_mut()` is safe.
        let this = self.get_mut();

        // Check keepalive first (biased): if the keepalive timer
        // has elapsed, emit a comment to keep the connection alive
        // without adding data to the stream.
        if this.keepalive.poll_tick(cx).is_ready() {
            return Poll::Ready(Some(Ok(Bytes::from_static(b": keep-alive\n\n"))));
        }

        // Poll the merged channel stream and wrap each item in Ok.
        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Ready(Some(chunk)) => Poll::Ready(Some(Ok(chunk))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

pub async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    cancel_watch: Option<axum::Extension<crate::disconnect::CancelWatch>>,
    axum::Extension(parsed_req): axum::Extension<ParsedChatRequest>,
    auth_token: Option<axum::Extension<ValidatedApiToken>>,
    axum::Extension(resolved_route): axum::Extension<crate::middleware::routing::ResolvedRoute>,
) -> Result<axum::response::Response, ApiError> {
    let cancel = cancel_watch
        .map(|axum::Extension(cw)| cw)
        .unwrap_or_default();
    let token_inner = auth_token.map(|axum::Extension(t)| t);
    run_pipeline(
        state,
        cancel,
        headers,
        parsed_req.bytes,
        token_inner,
        resolved_route,
    )
    .await
}

/// Drive one chat-completion request through the pipeline.
///
/// The body mirrors what a synchronous-style pipeline call would look
/// like: parse → authenticate → resolve routing → build config → run
/// pipeline → shape the response.
///
/// `cancel` is the per-request watch receiver produced by the
/// [`crate::disconnect::client_disconnect_middleware`]. It flips to
/// `true` on a real TCP-level client disconnect; the pipeline's
/// dispatch loop, the `UpstreamClient::call()` `tokio::select!`, and the
/// SSE `stream.next()` `tokio::select!` all observe it.
async fn run_pipeline(
    state: AppState,
    cancel: CancelWatch,
    headers: HeaderMap,
    raw_request_body: bytes::Bytes,
    auth_result: Option<ValidatedApiToken>,
    resolved_route: crate::middleware::routing::ResolvedRoute,
) -> Result<axum::response::Response, ApiError> {
    let api_key_id: Option<ApiKeyId> = auth_result.as_ref().map(|r| r.key_id);

    let combo_id = resolved_route.combo_id;
    let combo_override = resolved_route.combo_override;
    let targets_override = resolved_route.targets_override;
    let openai_req = resolved_route.openai_req;

    // 5. Build pipeline.
    let pipeline = build_pipeline(&state);

    // 6. Per-request IDs.
    let request_id = RequestId::new();
    let trace_id = TraceId::new();

    // 7. Watchdog handling.
    let watchdog_budget_ms = calculate_watchdog_budget(&state, &headers);
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    let CancelWatch {
        tx: watchdog_tx,
        rx: client_disconnected,
    } = cancel;
    let stream_sink = if openai_req.stream {
        Some(openproxy_core::race_sink::StreamSink::Direct(tx))
    } else {
        Some(openproxy_core::race_sink::StreamSink::Discard)
    };

    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
    spawn_watchdog(done_rx, watchdog_tx, watchdog_budget_ms);

    // 8. Build request and run.
    let req = PipelineRequest {
        request_id,
        trace_id,
        combo_id,
        openai_request: openai_req.clone(),
        client_disconnected,
        stream_sink,
        api_key_id,
        combo_override,
        targets_override,
        request_headers: redact_sensitive_headers(&headers),
        request_body_json: Some(raw_request_body),
        race_cancelled: false,
        race_cancel: None,
        endpoint_kind: openproxy_core::endpoint::EndpointKind::Chat,
        compressed_messages: Arc::new(std::sync::OnceLock::new()),
    };

    if openai_req.stream {
        return handle_streaming_response(pipeline, req, done_tx, rx).await;
    }

    handle_sync_response(pipeline, req, done_tx).await
}

fn build_pipeline(state: &AppState) -> Pipeline {
    let config = PipelineConfig {
        defaults: openproxy_core::timeouts::Timeouts::from_config(&state.timeouts()),
        racing: state.config().racing.clone(),
        retries: state.config().retries,
        max_attempts: state.config().retries.max_attempts,
        master_key: state.master_key().clone(),
        adapters: Arc::new(state.adapters()),
        cooldown_secs: state.config().cooldown.cooldown_secs,
        cooldown_max_secs: state.config().cooldown.max_secs,
        cooldown_factor: state.config().cooldown.factor,
        upstream_client: state.upstream_client().clone(),
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

fn calculate_watchdog_budget(state: &AppState, headers: &HeaderMap) -> u64 {
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

fn spawn_watchdog(
    done_rx: tokio::sync::oneshot::Receiver<()>,
    watchdog_tx: tokio::sync::watch::Sender<bool>,
    budget_ms: u64,
) {
    tokio::spawn(async move {
        tokio::select! {
            _ = done_rx => {}
            _ = tokio::time::sleep(std::time::Duration::from_millis(budget_ms)) => {
                tracing::warn!(
                    budget_ms,
                    "watchdog timer fired — cancelling pipeline (this is a total-budget timeout, NOT a client disconnect)"
                );
                let _ = watchdog_tx.send(true);
            }
        }
    });
}

async fn handle_streaming_response(
    pipeline: Pipeline,
    req: PipelineRequest,
    done_tx: tokio::sync::oneshot::Sender<()>,
    rx: tokio::sync::mpsc::Receiver<Bytes>,
) -> Result<axum::response::Response, ApiError> {
    let (error_tx, error_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(1);

    tokio::spawn(async move {
        let result = pipeline.run(req).await;
        let _ = done_tx.send(());

        if let Some(err) = result.error {
            let raw_err = err.to_string();
            let redacted = openproxy_core::cost::redact_error_msg(&raw_err);
            let message = crate::error::truncate_error_message(&redacted.0);

            let error_json = serde_json::json!({
                "error": {
                    "message": message,
                    "type": err.code(),
                    "code": err.http_status(),
                }
            });
            let error_str = serde_json::to_string(&error_json).unwrap_or_default();
            let mut frame = bytes::BytesMut::with_capacity(error_str.len() + 16);
            frame.extend_from_slice(b"data: ");
            frame.extend_from_slice(error_str.as_bytes());
            frame.extend_from_slice(b"\n\n");
            let _ = error_tx.send(frame.freeze()).await;
        }
    });

    let main_stream = ReceiverStream::new(rx);
    let error_stream = ReceiverStream::new(error_rx);
    let mut merged = futures::stream::SelectAll::new();
    merged.push(main_stream);
    merged.push(error_stream);

    let sse_stream = SseBytesStream {
        inner: merged,
        keepalive: tokio::time::interval_at(
            tokio::time::Instant::now() + SSE_KEEPALIVE_INTERVAL,
            SSE_KEEPALIVE_INTERVAL,
        ),
    };

    let body = axum::body::Body::from_stream(sse_stream);
    Ok((
        [(
            axum::http::header::CONTENT_TYPE,
            "text/event-stream; charset=utf-8",
        )],
        body,
    )
        .into_response())
}

async fn handle_sync_response(
    pipeline: Pipeline,
    req: PipelineRequest,
    done_tx: tokio::sync::oneshot::Sender<()>,
) -> Result<axum::response::Response, ApiError> {
    let started = Instant::now();
    let result = pipeline.run(req).await;
    let _ = done_tx.send(());
    let elapsed_ms = started.elapsed().as_millis();

    if let Some(err) = result.error {
        let status =
            StatusCode::from_u16(err.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        tracing::debug!(
            status = status.as_u16(),
            attempts = result.attempts,
            elapsed_ms,
            error = %err,
            "chat_completions: error from pipeline"
        );
        return Err(ApiError(err));
    }

    let body_value = match result.final_response {
        Some(resp) => serde_json::to_value(&resp).unwrap_or_else(|e| {
            json!({
                "error": {
                    "code": "internal",
                    "message": format!("serialize response: {e}"),
                }
            })
        }),
        None => {
            tracing::warn!(
                attempts = result.attempts,
                elapsed_ms,
                "chat_completions: pipeline returned neither error nor response"
            );
            json!({
                "error": {"code": "internal", "message": "no response from pipeline"}
            })
        }
    };

    Ok(Json(body_value).into_response())
}
