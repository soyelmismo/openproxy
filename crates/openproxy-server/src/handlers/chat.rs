//! `POST /v1/chat/completions` — the public entry point.
//!
//! Spec §2.1 contract: parse the JSON as an [`OpenAIRequest`], resolve the
//! routing plan from `model` via [`openproxy_core::routing::resolve`] (a
//! `models` row routes direct through a synthetic single-target combo, a
//! `combo:<name>` matches a combo, anything else is 404), honour the legacy
//! `x-openproxy-combo` override, run the [`Pipeline`] (dispatch, retries,
//! timeouts, usage writes), and translate the [`PipelineResult`] into an
//! OpenAI-shaped JSON response or a structured error.

use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use bytes::{BufMut, Bytes};
use futures::stream::Stream;
use openproxy_pipeline::ResponseExt;
use openproxy_pipeline::{Pipeline, PipelineRequest};
use openproxy_types::TargetFormat;
use serde_json::json;
use std::convert::Infallible;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use std::time::Instant;
use tokio_stream::wrappers::ReceiverStream;

use crate::{
    error::ApiError, middleware::auth::ParsedChatRequest, services::PipelineRunner, state::AppState,
};

pub fn router(state: &AppState) -> axum::Router<AppState> {
    axum::Router::new().route(
        "/completions",
        super::chat_endpoint(state, chat_completions),
    )
}

/// SSE keepalive interval: emits `: keep-alive\n\n` while the upstream generates,
/// so nginx/Cloudflare and client HTTP libraries do not drop the connection on
/// inactivity (large prompts, reasoning models with slow first tokens) and report
/// a false "client disconnected".
///
/// CRITICAL: the first keepalive is DELAYED by this interval, never sent
/// immediately. `tokio::time::interval` fires on its first tick, which put
/// `: keep-alive\n\n` ahead of any `data: {...}` frame; some SSE clients (notably
/// the OpenAI Python library's httpx-sse parser) mishandle a leading comment and
/// close the connection. `interval_at` gives the upstream time to send real data.
const SSE_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);

/// A stream yielding pre-formatted SSE frames (`Bytes`) from an mpsc channel,
/// interleaved with keepalive comments. Unlike `axum::response::Sse` it writes raw
/// `Bytes` with no extra wrapping: the pipeline already emits `data: {payload}\n\n`.
struct SseBytesStream {
    inner: futures::stream::SelectAll<ReceiverStream<Bytes>>,
    keepalive: tokio::time::Interval,
}

impl Stream for SseBytesStream {
    type Item = Result<Bytes, Infallible>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // All fields are `Unpin`, so `get_mut()` is safe.
        let this = self.get_mut();

        // Biased poll: check the keepalive first, emitting a comment adds no data.
        if this.keepalive.poll_tick(cx).is_ready() {
            return Poll::Ready(Some(Ok(Bytes::from_static(b": keep-alive\n\n"))));
        }

        // Poll the merged channel, wrapping each item in Ok.
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
    crate::extractors::ValidatedToken(auth_token): crate::extractors::ValidatedToken,
    axum::Extension(resolved_route): axum::Extension<crate::middleware::routing::ResolvedRoute>,
) -> Result<axum::response::Response, ApiError> {
    let (pipeline, prepared) = PipelineRunner::prepare_from_handler(
        crate::services::pipeline_runner::PrepareFromHandlerParams {
            state: &state,
            headers: &headers,
            cancel_watch,
            auth_token,
            resolved_route,
            raw_request_body: parsed_req.bytes,
            endpoint_kind: openproxy_types::EndpointKind::Chat,
        },
    );

    if prepared.req.openai_request.stream {
        return Ok(handle_streaming_response(
            pipeline,
            prepared.req,
            prepared.done_tx,
            prepared.stream_rx,
        ));
    }

    handle_sync_response(pipeline, prepared.req, prepared.done_tx).await
}

/// Wrap an SSE byte stream in [`crate::services::sse_coalesce::CoalescedSse`] and
/// attach the standard `text/event-stream; charset=utf-8` content-type header.
pub(crate) fn into_coalesced_sse_response<S>(sse_stream: S) -> axum::response::Response
where
    S: Stream<Item = Result<Bytes, Infallible>> + Send + Unpin + 'static,
{
    let body = axum::body::Body::from_stream(crate::services::sse_coalesce::CoalescedSse::new(
        sse_stream,
        crate::services::sse_coalesce::configured_window(),
    ));
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/event-stream; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

/// Run a non-streaming pipeline request, signal `done_tx`, and map any pipeline
/// error into [`ApiError`].
pub(crate) async fn run_sync_pipeline(
    pipeline: Pipeline,
    req: PipelineRequest,
    done_tx: tokio::sync::oneshot::Sender<()>,
    caller: &'static str,
) -> Result<(Option<openproxy_types::OpenAIResponse>, u32, u128), ApiError> {
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
            response_error = %err,
            "{caller}: masked error returned to the client"
        );
        return Err(ApiError(err));
    }

    Ok((
        result.final_response,
        u32::from(result.attempts),
        elapsed_ms,
    ))
}

pub(crate) fn handle_streaming_response(
    pipeline: Pipeline,
    req: PipelineRequest,
    done_tx: tokio::sync::oneshot::Sender<()>,
    rx: tokio::sync::mpsc::Receiver<Bytes>,
) -> axum::response::Response {
    let merged =
        PipelineRunner::spawn_streaming_bridge(pipeline, req, done_tx, rx, TargetFormat::Openai);

    let sse_stream = SseBytesStream {
        inner: merged,
        keepalive: tokio::time::interval_at(
            tokio::time::Instant::now() + SSE_KEEPALIVE_INTERVAL,
            SSE_KEEPALIVE_INTERVAL,
        ),
    };

    into_coalesced_sse_response(sse_stream)
}

pub(crate) async fn handle_sync_response(
    pipeline: Pipeline,
    req: PipelineRequest,
    done_tx: tokio::sync::oneshot::Sender<()>,
) -> Result<axum::response::Response, ApiError> {
    let (final_response, attempts, elapsed_ms) =
        run_sync_pipeline(pipeline, req, done_tx, "chat_completions").await?;

    match final_response {
        Some(resp) => {
            let mut buf = bytes::BytesMut::with_capacity(1024).writer();
            match serde_json::to_writer(&mut buf, &resp) {
                Ok(()) => Ok((
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    buf.into_inner().freeze(),
                )
                    .into_response()),
                Err(e) => {
                    let err = ApiError(openproxy_types::CoreError::Internal(format!(
                        "serialize response: {e}"
                    )));
                    Ok(Json(json!({
                        "error": {
                            "code": "internal",
                            "message": err.sanitized_message(),
                        }
                    }))
                    .into_response())
                }
            }
        }
        None => {
            tracing::warn!(
                attempts,
                elapsed_ms,
                "chat_completions: pipeline returned neither error nor response"
            );
            Ok(Json(json!({
                "error": {"code": "internal", "message": "no response from pipeline"}
            }))
            .into_response())
        }
    }
}

pub(crate) async fn handle_sync_response_responses(
    pipeline: Pipeline,
    req: PipelineRequest,
    done_tx: tokio::sync::oneshot::Sender<()>,
) -> Result<axum::response::Response, ApiError> {
    let (final_response, attempts, elapsed_ms) =
        run_sync_pipeline(pipeline, req, done_tx, "responses_completions").await?;

    let body_value = match final_response {
        Some(resp) => resp.to_responses_envelope(),
        None => {
            tracing::warn!(
                attempts,
                elapsed_ms,
                "responses_completions: pipeline returned neither error nor response"
            );
            json!({
                "object": "response",
                "error": { "code": "internal", "message": "no response from pipeline" }
            })
        }
    };

    Ok(Json(body_value).into_response())
}
