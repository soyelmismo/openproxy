//! Dispatch no-streaming: ciclo de vida de una respuesta unary completa.

use super::types::{DispatchContext, DispatchParams, NonStreamingSuccessArgs};
use super::{Dispatcher, UpstreamDispatcher};
use crate::PipelineResult;
use crate::inline_tools::extract_inline_tools_from_response;
use crate::think_extractor::extract_think_from_response;
use crate::translation::OpenAIResponse;
use openproxy_adapters::upstream::{UpstreamError, UpstreamRequest};
use openproxy_types::error::CoreError;
use std::time::Instant;

/// Inserta los pares (k, v) que parseen como `HeaderName`/`HeaderValue`.
/// Los malformados se descartan.
pub(crate) fn populate_upstream_headers(
    upstream_request: &mut UpstreamRequest,
    headers: &[(String, String)],
) {
    upstream_request.headers.reserve(headers.len());
    for (k, v) in headers {
        if k.to_ascii_lowercase().starts_with("x-openproxy-") {
            continue;
        }
        if let (Ok(name), Ok(mut value)) = (
            http::HeaderName::from_bytes(k.as_bytes()),
            http::HeaderValue::from_str(v),
        ) {
            // `HeaderValue::from_str` crea un valor no sensible; marcar el
            // bearer de nuevo para que Debug/redaction no filtren el token.
            if name == http::header::AUTHORIZATION {
                value.set_sensitive(true);
            }
            upstream_request.headers.insert(name, value);
        }
    }
}

/// Anuncia `Accept: text/event-stream` en requests de inferencia streaming
/// cuando el adapter no declaró su propio Accept. Algunos upstreams (p.ej. el
/// endpoint mavis de MiniMax) condicionan el flush incremental de chunks a
/// este header: sin él bufferizan el cuerpo completo aunque el JSON diga
/// `stream:true`. Los adapters que ya fijan Accept (p.ej. atomesus) se
/// respetan. En no-streaming no se toca.
pub(crate) fn ensure_streaming_accept(upstream_request: &mut UpstreamRequest) {
    if upstream_request.is_streaming && !upstream_request.headers.contains_key(http::header::ACCEPT)
    {
        upstream_request.headers.insert(
            http::header::ACCEPT,
            http::HeaderValue::from_static("text/event-stream"),
        );
    }
}

pub(crate) use super::translation::{ensure_usage_populated, translate_non_streaming_body};

/// `true` cuando la respuesta 200 no trae contenido útil. Se trata como
/// error para forzar el retry.
pub(crate) fn is_empty_response(resp: &OpenAIResponse) -> bool {
    resp.choices.first().is_some_and(|c| {
        let msg = &c.message;
        let content_empty = msg
            .content
            .as_ref()
            .is_none_or(|v| v.as_str().is_none_or(str::is_empty));
        let no_tool_calls = msg.tool_calls.as_ref().is_none_or(std::vec::Vec::is_empty);
        let no_reasoning = !msg.extra.contains_key("reasoning_content");
        let no_finish = c
            .finish_reason
            .as_ref()
            .is_none_or(|f| f == "null" || f.is_empty());
        content_empty && no_tool_calls && no_reasoning && no_finish
    })
}

impl UpstreamDispatcher {
    /// Lee el body con deadline total desde `params.started`. Un status
    /// non-2xx acota el read a 5s para no penalizar los retries.
    pub(super) async fn collect_non_streaming_body(
        &self,
        response: openproxy_adapters::upstream::UpstreamResponse,
        status_code: u16,
        params: &DispatchParams<'_>,
        dctx: &DispatchContext<'_>,
        connect_and_send_ms: u64,
        ttft_ms: u64,
    ) -> Result<bytes::Bytes, Box<PipelineResult>> {
        let non_streaming_body_deadline = params.started
            + std::time::Duration::from_millis(params.resolved_timeouts.total.as_millis() as u64);
        let mut remaining = non_streaming_body_deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(std::time::Duration::ZERO);

        if !(200..300).contains(&status_code) {
            remaining = std::cmp::min(remaining, std::time::Duration::from_secs(5));
        }

        match tokio::time::timeout(remaining, response.collect()).await {
            Ok(Ok(b)) => Ok(b),
            Ok(Err(UpstreamError::Cancel)) => {
                tracing::warn!(
                    combo_id = params.combo.id.0,
                    target_id = params.target.id.0,
                    provider = %params.target.provider_id,
                    elapsed_ms = params.started.elapsed().as_millis() as u64,
                    "client cancelled during upstream body read; aborting attempt"
                );
                let err = CoreError::Cancelled(openproxy_types::CancelReason::ClientDisconnected);
                Err(Box::new(
                    self.record_and_fail(
                        params.req.clone(),
                        params.combo,
                        params.target,
                        dctx.fail_ctx_code(
                            &err,
                            Some(connect_and_send_ms),
                            Some(ttft_ms),
                            err.http_status(),
                        ),
                    )
                    .await,
                ))
            }
            Ok(Err(UpstreamError::Timeout(phase))) => {
                let err = CoreError::UpstreamTimeout {
                    phase: phase.as_str().to_string(),
                    ms: params.started.elapsed().as_millis() as u64,
                };
                Err(Box::new(
                    self.record_and_fail(
                        params.req.clone(),
                        params.combo,
                        params.target,
                        dctx.fail_ctx_code(
                            &err,
                            Some(connect_and_send_ms),
                            Some(ttft_ms),
                            err.http_status(),
                        ),
                    )
                    .await,
                ))
            }
            Ok(Err(e)) => {
                self.check_and_trigger_proxy_rotation(
                    &params.target.provider_id,
                    params.target.account_id,
                    params
                        .req
                        .proxy_override
                        .as_ref()
                        .map(|(pid, _)| pid.as_str()),
                    super::rotation::ProxyRotationTrigger::ConnectError,
                    None,
                )
                .await;
                let err = CoreError::UpstreamConnection(format!("read upstream body: {e}"));
                Err(Box::new(
                    self.record_and_fail(
                        params.req.clone(),
                        params.combo,
                        params.target,
                        dctx.fail_ctx_code(
                            &err,
                            Some(connect_and_send_ms),
                            Some(ttft_ms),
                            err.http_status(),
                        ),
                    )
                    .await,
                ))
            }
            Err(_elapsed) => {
                self.check_and_trigger_proxy_rotation(
                    &params.target.provider_id,
                    params.target.account_id,
                    params
                        .req
                        .proxy_override
                        .as_ref()
                        .map(|(pid, _)| pid.as_str()),
                    super::rotation::ProxyRotationTrigger::ConnectError,
                    None,
                )
                .await;
                let elapsed = params.started.elapsed().as_millis() as u64;
                let err = CoreError::UpstreamTimeout {
                    phase: "total (config: total_ms)".to_string(),
                    ms: elapsed,
                };
                tracing::warn!(
                    combo_id = params.combo.id.0,
                    target_id = params.target.id.0,
                    provider = %params.target.provider_id,
                    elapsed_ms = elapsed,
                    "non-streaming body read exceeded total_ms; aborting attempt"
                );
                Err(Box::new(
                    self.record_and_fail(
                        params.req.clone(),
                        params.combo,
                        params.target,
                        dctx.fail_ctx_code(
                            &err,
                            Some(connect_and_send_ms),
                            Some(ttft_ms),
                            err.http_status(),
                        ),
                    )
                    .await,
                ))
            }
        }
    }

    /// El builder de usage es no-fatal: un fallo se loguea como warn y
    /// devuelve `usage_tuple=None`.
    pub(super) async fn record_non_streaming_success(
        &self,
        params: DispatchParams<'_>,
        dctx: &DispatchContext<'_>,
        args: NonStreamingSuccessArgs,
    ) -> PipelineResult {
        let prompt_tokens = args.openai_response.usage.as_ref().map(|u| u.prompt_tokens);
        let completion_tokens = args
            .openai_response
            .usage
            .as_ref()
            .map(|u| u.completion_tokens);
        let cached_tokens = args
            .openai_response
            .usage
            .as_ref()
            .and_then(|u| u.prompt_tokens_details.as_ref())
            .and_then(|d| d.cached_tokens);

        let total_ms_now = params.started.elapsed().as_millis() as u64;

        if let Some(registry) = self.message_threads.as_ref()
            && params.target.provider_id.as_str() == "claude-code"
        {
            let canonical = crate::stages::target_headers::session::resolve_canonical_session(
                &params.req.request_headers,
                &params.req.openai_request,
            );
            registry.record_success(
                &canonical,
                Some(params.target.id),
                &params.req.openai_request.messages,
                &args.openai_response.id,
            );
        }

        let request_headers_btm = if self.tracker.is_recording() {
            Some(crate::redact::redact_btreemap_sensitive(
                params
                    .headers
                    .iter()
                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
                    .collect::<std::collections::BTreeMap<String, String>>(),
            ))
        } else {
            None
        };
        let usage_tuple = match crate::usage_tracker::UsageRecordBuilder::new(
            &self.tracker,
            params.req,
            params.combo,
            params.target,
        )
        .proxy_url(dctx.proxy_url.clone())
        .proxy_status(dctx.proxy_status.clone())
        .model_opt(Some(params.model))
        .err_opt(None)
        .connect_ms_opt(Some(args.connect_and_send_ms))
        .ttft_ms_opt(Some(args.ttft_ms))
        .total_ms(total_ms_now)
        .status_code(args.status_code)
        .attempt(params.attempt)
        .race_size(params.race_size)
        .trace_id(params.trace_id)
        .prompt_tokens_opt(prompt_tokens)
        .completion_tokens_opt(completion_tokens)
        .cached_tokens(cached_tokens)
        .response_body_json(Some(args.response_body_raw))
        .redact_logs(self.config.pii_config.pii_redact_logs)
        .request_headers(request_headers_btm)
        .response_headers(args.response_headers)
        .is_streaming(false)
        .stream_complete(true)
        .client_response(true)
        .stop_reason(None)
        .record()
        .await
        {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(error = %e, "UsageRecordBuilder failed; non-fatal");
                None
            }
        };

        PipelineResult {
            status_code: args.status_code,
            error: None,
            final_response: Some(args.openai_response),
            attempts: params.attempt,
            usage_tuple,
        }
    }

    pub(super) async fn dispatch_upstream_non_streaming(
        &self,
        params: DispatchParams<'_>,
        dctx: DispatchContext<'_>,
        upstream_request: UpstreamRequest,
    ) -> PipelineResult {
        let send_start = Instant::now();
        let reason = *params.req.client_disconnected.borrow();
        if let Some(reason) = reason {
            let elapsed = send_start.elapsed().as_millis() as u64;
            tracing::warn!(
                combo_id = params.combo.id.0,
                target_id = params.target.id.0,
                provider = %params.target.provider_id,
                elapsed_ms = elapsed,
                "client disconnected before upstream send; aborting attempt"
            );
            return self
                .record_and_fail(
                    params.req,
                    params.combo,
                    params.target,
                    dctx.fail_ctx_code(
                        &CoreError::Cancelled(reason),
                        Some(elapsed),
                        None,
                        CoreError::Cancelled(reason).http_status(),
                    ),
                )
                .await;
        }

        let cancel_token = openproxy_adapters::upstream::CancellationToken::from_watch(
            tokio::sync::watch::Receiver::clone(&params.req.client_disconnected),
        );
        let result = self
            .config
            .upstream_client
            .call(
                upstream_request,
                openproxy_adapters::upstream::TimeoutProfile::Custom(
                    params.resolved_timeouts.as_resolved(),
                ),
                cancel_token,
            )
            .await;
        let connect_and_send_ms = send_start.elapsed().as_millis() as u64;

        let response = match result {
            Ok(r) => r,
            Err(err) => {
                return self
                    .handle_upstream_error(
                        err,
                        params.req,
                        params.combo,
                        params.target,
                        &dctx,
                        connect_and_send_ms,
                    )
                    .await;
            }
        };

        let status_code = response.status.as_u16();
        let response_headers = self.is_recording().then(|| {
            // Security (OP-17): upstream responses can carry credentials back
            // (`set-cookie` from the provider, `proxy-authorization`, ...).
            // The recorded row is operator-visible via /admin/api/usage/detail,
            // so it must go through the same redaction as request headers —
            // this is the invariant documented in crate::redact.
            crate::redact::redact_btreemap_sensitive(
                response
                    .headers
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.as_str().to_string(),
                            v.to_str().unwrap_or_default().to_string(),
                        )
                    })
                    .collect::<std::collections::BTreeMap<String, String>>(),
            )
        });

        let ttft_ms = params.started.elapsed().as_millis() as u64;
        if (200..300).contains(&status_code) {
            openproxy_types::emit_stage_event!(
                request_id: params.req.request_id,
                trace_id: params.trace_id,
                stage: "waiting_ttft",
                elapsed_ms: ttft_ms,
                connect_ms: connect_and_send_ms,
                status_code: status_code,
            );
        }

        let body_bytes = match self
            .collect_non_streaming_body(
                response,
                status_code,
                &params,
                &dctx,
                connect_and_send_ms,
                ttft_ms,
            )
            .await
        {
            Ok(b) => b,
            Err(fail_res) => return *fail_res,
        };

        if !(200..300).contains(&status_code) {
            let retry_after_header = response_headers
                .as_ref()
                .and_then(|h: &std::collections::BTreeMap<String, String>| {
                    h.get("retry-after").or_else(|| h.get("Retry-After"))
                })
                .map(String::as_str);
            let body_str = String::from_utf8_lossy(&body_bytes).to_string();
            return self
                .handle_non_2xx_response(
                    status_code,
                    retry_after_header,
                    body_str,
                    params.req,
                    params.combo,
                    params.target,
                    params.model,
                    &dctx,
                    connect_and_send_ms,
                    Some(ttft_ms),
                )
                .await;
        }

        // Devin and Kiro endpoints emit stream envelopes (Connect-RPC protobuf
        // or AWS EventStream binary). A unary client folds the whole body into
        // an OpenAI response instead of parsing as JSON.
        let is_kiro = params.target.provider_id.as_str() == "kiro";
        let (response_body_raw, mut openai_response) = if is_kiro {
            match openproxy_adapters::adapters::kiro::fold_kiro_body_to_openai_response(
                &body_bytes,
                &params.req.openai_request.model,
            ) {
                Ok(resp) => {
                    let raw = serde_json::to_value(&resp).unwrap_or_default();
                    (raw, resp)
                }
                Err(err) => {
                    return self
                        .record_and_fail(
                            params.req,
                            params.combo,
                            params.target,
                            dctx.fail_ctx_code(
                                &err,
                                Some(connect_and_send_ms),
                                Some(ttft_ms),
                                err.http_status(),
                            ),
                        )
                        .await;
                }
            }
        } else {
            match params.target_format {
                openproxy_types::TargetFormat::Devin => {
                    match crate::sse::devin::fold_devin_body_to_openai_response(
                        &body_bytes,
                        &params.req.openai_request.model,
                    ) {
                        Ok(resp) => {
                            let raw = serde_json::to_value(&resp).unwrap_or_default();
                            (raw, resp)
                        }
                        Err(err) => {
                            return self
                                .record_and_fail(
                                    params.req,
                                    params.combo,
                                    params.target,
                                    dctx.fail_ctx_code(
                                        &err,
                                        Some(connect_and_send_ms),
                                        Some(ttft_ms),
                                        err.http_status(),
                                    ),
                                )
                                .await;
                        }
                    }
                }
                _ => match serde_json::from_slice::<serde_json::Value>(&body_bytes) {
                    Ok(raw) => {
                        let resp = match translate_non_streaming_body(
                            params.target_format,
                            &raw,
                            &params.req,
                        ) {
                            Ok(r) => {
                                let think_extracted = extract_think_from_response(r);
                                extract_inline_tools_from_response(think_extracted)
                            }
                            Err(err) => {
                                return self
                                    .record_and_fail(
                                        params.req,
                                        params.combo,
                                        params.target,
                                        dctx.fail_ctx_code(
                                            &err,
                                            Some(connect_and_send_ms),
                                            Some(ttft_ms),
                                            err.http_status(),
                                        ),
                                    )
                                    .await;
                            }
                        };
                        (raw, resp)
                    }
                    Err(e) => {
                        let body_str = String::from_utf8_lossy(&body_bytes);
                        if params.target_format == openproxy_types::TargetFormat::CommandCodeGo
                            || body_str.starts_with("data:")
                            || body_str.starts_with("event:")
                            || body_str.contains("\ndata:")
                            || body_str.contains("\nevent:")
                            || (body_str.trim_start().starts_with('{') && body_str.contains('\n'))
                        {
                            match crate::sse::parse_sse_stream_to_openai_response(
                                params.target_format,
                                &body_str,
                                &params.req.openai_request.model,
                            ) {
                                Ok(resp) => {
                                    let raw = serde_json::to_value(&resp).unwrap_or_default();
                                    let think_extracted = extract_think_from_response(resp);
                                    (raw, extract_inline_tools_from_response(think_extracted))
                                }
                                Err(err) => {
                                    return self
                                        .record_and_fail(
                                            params.req,
                                            params.combo,
                                            params.target,
                                            dctx.fail_ctx_code(
                                                &err,
                                                Some(connect_and_send_ms),
                                                Some(ttft_ms),
                                                err.http_status(),
                                            ),
                                        )
                                        .await;
                                }
                            }
                        } else {
                            let err =
                                CoreError::Parse(format!("invalid json in upstream response: {e}"));
                            return self
                                .record_and_fail(
                                    params.req,
                                    params.combo,
                                    params.target,
                                    dctx.fail_ctx_code(
                                        &err,
                                        Some(connect_and_send_ms),
                                        Some(ttft_ms),
                                        err.http_status(),
                                    ),
                                )
                                .await;
                        }
                    }
                },
            }
        };

        for choice in &mut openai_response.choices {
            choice.message.extra.remove("raw_response_body");
        }

        // The Claude Code adapter renames tool names for the upstream; map them
        // back so the client sees the names it declared.
        if params.target.provider_id.as_str() == "claude-code"
            && let Some(map) = crate::claude_tool_names::build_restore_map(&params.req)
        {
            crate::claude_tool_names::restore_response_tool_names(&mut openai_response, &map);
        }

        if is_empty_response(&openai_response) {
            let err = CoreError::UpstreamConnection(
                "upstream returned 200 but response is empty (content=null, finish_reason=null, no tool_calls, no reasoning) — treating as error for retry".to_string(),
            );
            return self
                .record_and_fail(
                    params.req,
                    params.combo,
                    params.target,
                    dctx.fail_ctx_code(&err, Some(connect_and_send_ms), Some(ttft_ms), 502),
                )
                .await;
        }

        if self.config.pii_config.pii_enabled
            && self.config.pii_config.pii_reversible
            && let Some(ref session) = *params.req.pii_session.lock()
        {
            session.restore_openai_response(&mut openai_response);
        }

        // Guarantee robust BPE token estimation if upstream omitted usage or returned 0
        ensure_usage_populated(&mut openai_response, &params.req.openai_request.messages);

        // If a system prefix was injected dynamically into this target, discount its tokens
        // from usage.prompt_tokens so the downstream client observes exact, untampered billing.
        if let Some(ref prefix) = *params.req.active_system_prefix.lock() {
            let prefix_msg = [openproxy_types::OpenAIMessage {
                role: "system".to_string(),
                content: Some(serde_json::Value::String(prefix.clone())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: Default::default(),
            }];
            let prefix_tokens =
                openproxy_compression::token_estimate::estimate_prompt_tokens(&prefix_msg);
            let client_est = openproxy_compression::token_estimate::estimate_prompt_tokens(
                &params.req.openai_request.messages,
            )
            .max(1);

            if let Some(ref mut u) = openai_response.usage {
                u.prompt_tokens = u
                    .prompt_tokens
                    .saturating_sub(prefix_tokens)
                    .max(client_est);
                u.total_tokens = u.prompt_tokens + u.completion_tokens;
            }
        }

        if params.combo.sanitize_response.unwrap_or(false) {
            let model_override = params
                .combo
                .real_model_id
                .as_deref()
                .unwrap_or(&params.req.openai_request.model);
            crate::response_sanitizer::sanitize_unary_response(
                &mut openai_response,
                model_override,
            );
        }

        let recorded_response_body =
            if self.config.pii_config.pii_enabled && !self.config.pii_config.pii_redact_logs {
                serde_json::to_value(&openai_response).unwrap_or_else(|_| response_body_raw.clone())
            } else if !self.config.pii_config.pii_enabled {
                serde_json::to_value(&openai_response).unwrap_or(response_body_raw)
            } else {
                response_body_raw
            };

        if let Some(sink) = params.req.stream_sink.as_ref() {
            emit_unary_response_to_stream_sink(sink, &openai_response).await;
        }

        self.record_non_streaming_success(
            params,
            &dctx,
            NonStreamingSuccessArgs {
                status_code,
                connect_and_send_ms,
                ttft_ms,
                response_headers,
                response_body_raw: recorded_response_body,
                openai_response,
            },
        )
        .await
    }
}

async fn emit_unary_response_to_stream_sink(
    sink: &crate::race_sink::StreamSink,
    resp: &OpenAIResponse,
) {
    if matches!(sink, crate::race_sink::StreamSink::Discard) {
        return;
    }
    use std::fmt::Write;
    let chunk_id = &resp.id;
    let created = resp.created;
    let model = &resp.model;

    for choice in &resp.choices {
        let mut delta = serde_json::Map::new();
        delta.insert("role".to_string(), serde_json::json!(choice.message.role));
        if let Some(ref content) = choice.message.content {
            delta.insert("content".to_string(), content.clone());
        }
        if let Some(ref tc) = choice.message.tool_calls {
            delta.insert("tool_calls".to_string(), serde_json::json!(tc));
        }
        for (k, v) in &choice.message.extra {
            delta.insert(k.clone(), v.clone());
        }

        let chunk = serde_json::json!({
            "id": chunk_id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [{
                "index": choice.index,
                "delta": delta,
                "finish_reason": null,
            }],
        });
        if let Ok(json_str) = serde_json::to_string(&chunk) {
            let mut buf = String::with_capacity(json_str.len() + 8);
            let _ = write!(&mut buf, "data: {json_str}\n\n");
            let _ = sink.send(bytes::Bytes::from(buf)).await;
        }

        let finish_chunk = serde_json::json!({
            "id": chunk_id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [{
                "index": choice.index,
                "delta": {},
                "finish_reason": choice.finish_reason.as_deref().unwrap_or("stop"),
            }],
            "usage": resp.usage,
        });
        if let Ok(json_str) = serde_json::to_string(&finish_chunk) {
            let mut buf = String::with_capacity(json_str.len() + 8);
            let _ = write!(&mut buf, "data: {json_str}\n\n");
            let _ = sink.send(bytes::Bytes::from(buf)).await;
        }
    }
    let _ = sink.send(crate::pipeline::SSE_DONE_BYTES).await;
}

#[cfg(test)]
mod ensure_usage_tests {
    use super::*;
    use openproxy_types::{OpenAIChoice, OpenAIMessage, OpenAIResponse, OpenAIUsage};

    #[test]
    fn test_ensure_usage_populates_missing() {
        let messages = vec![OpenAIMessage {
            role: "user".to_string(),
            content: Some(serde_json::Value::String(
                "Hello world from unit test".to_string(),
            )),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        }];

        let mut resp = OpenAIResponse {
            id: "test-id".to_string(),
            object: "chat.completion".to_string(),
            created: 123456,
            model: "test-model".to_string(),
            choices: vec![OpenAIChoice {
                index: 0,
                message: OpenAIMessage {
                    role: "assistant".to_string(),
                    content: Some(serde_json::Value::String(
                        "Hi! This is a test response.".to_string(),
                    )),
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                    extra: Default::default(),
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: None,
        };

        ensure_usage_populated(&mut resp, &messages);

        assert!(resp.usage.is_some());
        let u = resp.usage.unwrap();
        assert!(
            u.prompt_tokens >= 4,
            "expected at least 4 prompt tokens, got {}",
            u.prompt_tokens
        );
        assert!(
            u.completion_tokens >= 4,
            "expected at least 4 completion tokens, got {}",
            u.completion_tokens
        );
        assert_eq!(u.total_tokens, u.prompt_tokens + u.completion_tokens);
    }

    #[test]
    fn test_ensure_usage_preserves_existing() {
        let messages = vec![];
        let mut resp = OpenAIResponse {
            id: "test-id".to_string(),
            object: "chat.completion".to_string(),
            created: 123456,
            model: "test-model".to_string(),
            choices: vec![],
            usage: Some(OpenAIUsage {
                prompt_tokens: 42,
                completion_tokens: 18,
                total_tokens: 60,
                prompt_tokens_details: None,
            }),
        };

        ensure_usage_populated(&mut resp, &messages);

        let u = resp.usage.unwrap();
        assert_eq!(u.prompt_tokens, 42);
        assert_eq!(u.completion_tokens, 18);
        assert_eq!(u.total_tokens, 60);
    }
}
