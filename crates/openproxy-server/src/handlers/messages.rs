use axum::{
    extract::State,
    http::HeaderMap,
    response::IntoResponse,
};
use bytes::Bytes;
use futures::stream::Stream;
use openproxy_pipeline::redact::redact_sensitive_headers;
use openproxy_pipeline::{Pipeline, PipelineRequest};
use openproxy_types::ids::{ApiKeyId, RequestId, TraceId};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio_stream::wrappers::ReceiverStream;
use std::sync::Arc;

use crate::{
    disconnect::CancelWatch,
    error::ApiError,
    middleware::auth::{ParsedChatRequest, ValidatedApiToken},
    state::AppState,
};
use openproxy_pipeline::translation::{AnthropicRequest, anthropic_request_to_openai, openai_response_to_anthropic};

pub async fn anthropic_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    cancel_watch: Option<axum::Extension<CancelWatch>>,
    axum::Extension(parsed_req): axum::Extension<ParsedChatRequest>,
    auth_token: Option<axum::Extension<ValidatedApiToken>>,
    axum::Extension(mut resolved_route): axum::Extension<crate::middleware::routing::ResolvedRoute>,
) -> Result<axum::response::Response, ApiError> {
    let anthropic_req: AnthropicRequest = serde_json::from_slice(&parsed_req.bytes)
        .map_err(|e| ApiError(openproxy_types::error::CoreError::Validation(format!("Invalid Anthropic Request: {e}"))))?;
    
    let openai_req = Arc::new(anthropic_request_to_openai(anthropic_req));
    resolved_route.openai_req = openai_req.clone();
    
    let cancel = cancel_watch.map(|axum::Extension(cw)| cw).unwrap_or_default();
    let token_inner = auth_token.map(|axum::Extension(t)| t);
    
    let api_key_id: Option<ApiKeyId> = token_inner.as_ref().map(|r| r.key_id);
    let combo_id = resolved_route.combo_id;
    let combo_override = resolved_route.combo_override;
    let targets_override = resolved_route.targets_override;

    let config = openproxy_pipeline::PipelineConfig {
        defaults: openproxy_pipeline::timeouts::Timeouts::from_config(&state.timeouts()),
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
    let pipeline = Pipeline::with_selection_registry(
        state.db_pool().writer_arc(),
        config,
        state.record_bodies_and_flags(),
        state.selection_registry(),
        state.circuit_breaker(),
    );

    let request_id = RequestId::new();
    let trace_id = TraceId::new();

    let client_deadline_ms: Option<u64> = headers
        .get("x-request-deadline-ms")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0);
    let total_ms = state.timeouts().total_ms;
    let watchdog_budget_ms = client_deadline_ms.filter(|&ms| ms < total_ms).unwrap_or(total_ms);

    let (tx, rx) = tokio::sync::mpsc::channel(64);
    let CancelWatch { tx: watchdog_tx, rx: client_disconnected } = cancel;
    
    let stream_sink = if openai_req.stream {
        Some(openproxy_pipeline::StreamSink::Direct(tx))
    } else {
        Some(openproxy_pipeline::StreamSink::Discard)
    };

    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        tokio::select! {
            _ = done_rx => {}
            _ = tokio::time::sleep(std::time::Duration::from_millis(watchdog_budget_ms)) => {
                let _ = watchdog_tx.send(true);
            }
        }
    });

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
        request_body_json: Some(parsed_req.bytes),
        race_cancelled: false,
        race_cancel: None,
        endpoint_kind: openproxy_types::EndpointKind::Chat,
        compressed_messages: Arc::new(std::sync::OnceLock::new()),
    };

    if openai_req.stream {
        let (error_tx, error_rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(1);
        tokio::spawn(async move {
            let result = pipeline.run(req).await;
            let _ = done_tx.send(());
            if let Some(err) = result.error {
                let raw_err = err.to_string();
                let redacted = openproxy_core::cost::redact_error_msg(&raw_err);
                let message = crate::error::truncate_error_message(&redacted.0);
                let error_json = serde_json::json!({
                    "type": "error",
                    "error": {
                        "type": err.code(),
                        "message": message
                    }
                });
                let error_str = serde_json::to_string(&error_json).unwrap_or_default();
                let mut frame = bytes::BytesMut::with_capacity(error_str.len() + 16);
                frame.extend_from_slice(b"event: error\ndata: ");
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

        let sse_stream = OpenAIToAnthropicSseStream {
            inner: merged,
            has_started: false,
            has_finished: false,
            message_id: format!("msg_{}", request_id),
            model: openai_req.model.clone(),
        };
        
        let body = axum::body::Body::from_stream(sse_stream);
        Ok((
            [(
                axum::http::header::CONTENT_TYPE,
                "text/event-stream; charset=utf-8",
            )],
            body,
        ).into_response())
    } else {
        let result = pipeline.run(req).await;
        let _ = done_tx.send(());
        if let Some(err) = result.error {
            return Err(ApiError(err));
        }
        let body_value = match result.final_response {
            Some(resp) => {
                let anthropic_resp = openai_response_to_anthropic(resp);
                serde_json::to_value(&anthropic_resp).unwrap_or_else(|e| serde_json::json!({"error": {"message": e.to_string()}}))
            },
            None => serde_json::json!({"error": {"message": "no response"}}),
        };
        Ok(axum::Json(body_value).into_response())
    }
}

struct OpenAIToAnthropicSseStream<S> {
    inner: S,
    has_started: bool,
    has_finished: bool,
    message_id: String,
    model: String,
}

impl<S: Stream<Item = Bytes> + Unpin> Stream for OpenAIToAnthropicSseStream<S> {
    type Item = Result<Bytes, std::convert::Infallible>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        
        loop {
            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Ready(Some(chunk)) => {
                    let s = std::str::from_utf8(&chunk).unwrap_or("");
                    if s.starts_with("data: ") && !s.contains("[DONE]") {
                        let json_str = s.trim_start_matches("data: ").trim();
                        if let Ok(v) = serde_json::from_str::<OpenAISseProbe>(json_str) {
                            let mut out = bytes::BytesMut::new();
                            
                            if !this.has_started {
                                this.has_started = true;
                                let start_event = serde_json::json!({
                                    "type": "message_start",
                                    "message": {
                                        "id": this.message_id,
                                        "type": "message",
                                        "role": "assistant",
                                        "model": this.model,
                                        "content": [],
                                        "stop_reason": null,
                                        "stop_sequence": null,
                                        "usage": {"input_tokens": 0, "output_tokens": 0}
                                    }
                                });
                                out.extend_from_slice(b"event: message_start\ndata: ");
                                out.extend_from_slice(serde_json::to_string(&start_event).unwrap().as_bytes());
                                out.extend_from_slice(b"\n\n");
                                
                                let block_start = serde_json::json!({
                                    "type": "content_block_start",
                                    "index": 0,
                                    "content_block": {"type": "text", "text": ""}
                                });
                                out.extend_from_slice(b"event: content_block_start\ndata: ");
                                out.extend_from_slice(serde_json::to_string(&block_start).unwrap().as_bytes());
                                out.extend_from_slice(b"\n\n");
                            }
                            
                            if let Some(choices) = v.choices {
                                if let Some(first) = choices.first() {
                                    if let Some(delta) = &first.delta {
                                        if let Some(content) = &delta.content {
                                            if !content.is_empty() {
                                                let block_delta = serde_json::json!({
                                                    "type": "content_block_delta",
                                                    "index": 0,
                                                    "delta": {"type": "text_delta", "text": content}
                                                });
                                                out.extend_from_slice(b"event: content_block_delta\ndata: ");
                                                out.extend_from_slice(serde_json::to_string(&block_delta).unwrap().as_bytes());
                                                out.extend_from_slice(b"\n\n");
                                            }
                                        }
                                    }
                                    
                                    if let Some(finish_reason) = &first.finish_reason {
                                        if !this.has_finished {
                                            this.has_finished = true;
                                            
                                            let stop = serde_json::json!({
                                                "type": "content_block_stop",
                                                "index": 0
                                            });
                                            out.extend_from_slice(b"event: content_block_stop\ndata: ");
                                            out.extend_from_slice(serde_json::to_string(&stop).unwrap().as_bytes());
                                            out.extend_from_slice(b"\n\n");
                                            
                                            let anthropic_stop = match finish_reason.as_str() {
                                                "length" => "max_tokens",
                                                "tool_calls" | "function_call" => "tool_use",
                                                "content_filter" => "stop_sequence",
                                                _ => "end_turn",
                                            };

                                            let msg_delta = serde_json::json!({
                                                "type": "message_delta",
                                                "delta": {"stop_reason": anthropic_stop},
                                                "usage": {"output_tokens": 0}
                                            });
                                            out.extend_from_slice(b"event: message_delta\ndata: ");
                                            out.extend_from_slice(serde_json::to_string(&msg_delta).unwrap().as_bytes());
                                            out.extend_from_slice(b"\n\n");
                                            
                                            out.extend_from_slice(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
                                        }
                                    }
                                }
                            }
                            
                            return Poll::Ready(Some(Ok(out.freeze())));
                        }
                    }
                    
                    if s.starts_with("event: error") || s.starts_with(": keep-alive") {
                        return Poll::Ready(Some(Ok(chunk)));
                    }
                    
                    continue; // Skip chunk and poll next
                },
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[derive(serde::Deserialize)]
struct OpenAISseProbe {
    choices: Option<Vec<OpenAIChoiceProbe>>,
}

#[derive(serde::Deserialize)]
struct OpenAIChoiceProbe {
    delta: Option<OpenAIDeltaProbe>,
    finish_reason: Option<String>,
}

#[derive(serde::Deserialize)]
struct OpenAIDeltaProbe {
    content: Option<String>,
}
