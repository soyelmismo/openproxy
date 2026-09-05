//! Dispatch no-streaming: ciclo de vida de una respuesta unary completa.
//! Contiene 4 funciones libres (poblado de headers, traducción de body,
//! chequeo de respuesta vacía) más 3 métodos (`collect_non_streaming_body`,
//! `record_non_streaming_success`, `dispatch_upstream_non_streaming`).

use super::types::{DispatchContext, DispatchParams, NonStreamingSuccessArgs};
use super::{Dispatcher, UpstreamDispatcher};
use crate::PipelineResult;
use crate::think_extractor::extract_think_from_response;
use crate::translation::OpenAIResponse;
use openproxy_adapters::ProviderAdapter;
use openproxy_adapters::upstream::{UpstreamError, UpstreamRequest};
use openproxy_types::error::CoreError;
use std::time::Instant;

/// Inserta en `upstream_request.headers` los pares (k, v) que parseen
/// correctamente como `HeaderName`/`HeaderValue`. Los pares malformados
/// se descartan silenciosamente.
pub(super) fn populate_upstream_headers(
    upstream_request: &mut UpstreamRequest,
    headers: &[(String, String)],
) {
    upstream_request.headers.reserve(headers.len());
    for (k, v) in headers {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(k.as_bytes()),
            http::HeaderValue::from_str(v),
        ) {
            upstream_request.headers.insert(name, value);
        }
    }
}

/// Construye un `OpenAIResponse` minimalista extrayendo `choices[0].message.content`
/// del body crudo (formato simple usado por adapters que aún no producen
/// OpenAI nativo).
pub(super) fn translate_simple_text_response(
    response_body_raw: &serde_json::Value,
    model_name: String,
) -> OpenAIResponse {
    let text = response_body_raw
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|s| s.as_str())
        .or_else(|| response_body_raw.get("content").and_then(|s| s.as_str()))
        .unwrap_or("");

    let mut id = String::with_capacity(48);
    use std::fmt::Write;
    let _ = write!(&mut id, "chatcmpl_{}", uuid::Uuid::new_v4());

    OpenAIResponse {
        id,
        object: "chat.completion".to_string(),
        created: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
        model: model_name,
        choices: vec![openproxy_types::OpenAIChoice {
            index: 0,
            message: openproxy_types::OpenAIMessage {
                role: "assistant".to_string(),
                content: Some(serde_json::Value::String(text.to_string())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: Default::default(),
            },
            finish_reason: Some("stop".to_string()),
        }],
        usage: None,
    }
}

/// Despacha `response_body_raw` al `OpenAIResponse` correcto según el
/// `target_format`. Los adapters Gemini delega a su propia traducción;
/// Atomesus/Fx caen al fallback simple.
pub(super) fn translate_non_streaming_body(
    target_format: openproxy_types::TargetFormat,
    response_body_raw: &serde_json::Value,
    req: &crate::PipelineRequest,
) -> Result<OpenAIResponse, CoreError> {
    match target_format {
        openproxy_types::TargetFormat::Responses => {
            unreachable!("Responses format is handled natively before dispatcher")
        }
        openproxy_types::TargetFormat::Openai => {
            <OpenAIResponse as serde::Deserialize>::deserialize(response_body_raw)
                .map_err(|e| CoreError::Parse(format!("parse openai response: {e}")))
        }
        openproxy_types::TargetFormat::Anthropic => {
            let anthropic_resp: crate::translation::AnthropicResponse =
                <crate::translation::AnthropicResponse as serde::Deserialize>::deserialize(
                    response_body_raw,
                )
                .map_err(|e| CoreError::Parse(format!("parse anthropic response: {e}")))?;
            Ok(crate::translation::anthropic_to_openai(&anthropic_resp))
        }
        openproxy_types::TargetFormat::Gemini => {
            let adapter = openproxy_adapters::GeminiAdapter::new();
            adapter.translate_non_streaming_response(target_format, response_body_raw.clone())
        }
        openproxy_types::TargetFormat::Atomesus | openproxy_types::TargetFormat::Fx => Ok(
            translate_simple_text_response(response_body_raw, req.openai_request.model.clone()),
        ),
    }
}

/// `true` cuando la respuesta 200 no tiene contenido útil: `content=null`,
/// `finish_reason=null`/vacío, sin `tool_calls`, sin `reasoning_content`.
/// Tratamos esto como error para forzar retry.
pub(super) fn is_empty_response(resp: &OpenAIResponse) -> bool {
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
    /// Lee el body con un deadline total (calculado desde `params.started`
    /// sumado a `resolved_timeouts.total`). Si el status es non-2xx acota el
    /// read a 5s para no penalizar retries. Cualquier error se traduce a
    /// `Box<PipelineResult>` para cortar la cadena del caller.
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
                Err(Box::new(self.record_and_fail(
                    params.req.clone(),
                    params.combo,
                    params.target,
                    dctx.fail_ctx_code(
                        &err,
                        Some(connect_and_send_ms),
                        Some(ttft_ms),
                        err.http_status(),
                    ),
                )))
            }
            Ok(Err(UpstreamError::Timeout(phase))) => {
                let err = CoreError::UpstreamTimeout {
                    phase: phase.as_str().to_string(),
                    ms: params.started.elapsed().as_millis() as u64,
                };
                Err(Box::new(self.record_and_fail(
                    params.req.clone(),
                    params.combo,
                    params.target,
                    dctx.fail_ctx_code(
                        &err,
                        Some(connect_and_send_ms),
                        Some(ttft_ms),
                        err.http_status(),
                    ),
                )))
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
                Err(Box::new(self.record_and_fail(
                    params.req.clone(),
                    params.combo,
                    params.target,
                    dctx.fail_ctx_code(
                        &err,
                        Some(connect_and_send_ms),
                        Some(ttft_ms),
                        err.http_status(),
                    ),
                )))
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
                Err(Box::new(self.record_and_fail(
                    params.req.clone(),
                    params.combo,
                    params.target,
                    dctx.fail_ctx_code(
                        &err,
                        Some(connect_and_send_ms),
                        Some(ttft_ms),
                        err.http_status(),
                    ),
                )))
            }
        }
    }

    /// Persiste una respuesta 2xx en el `UsageTracker` y construye el
    /// `PipelineResult` final. El builder es no-fatal: cualquier error
    /// se loguea como warn y devuelve `usage_tuple=None`.
    pub(super) fn record_non_streaming_success(
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
        .request_headers(request_headers_btm)
        .response_headers(args.response_headers)
        .is_streaming(false)
        .stream_complete(true)
        .stop_reason(None)
        .record()
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

    /// Entry point no-streaming. Pasos:
    /// 1. Pre-flight disconnect check.
    /// 2. `upstream_client.call(...)`.
    /// 3. `handle_upstream_error` o `collect_non_streaming_body`.
    /// 4. Si non-2xx → `handle_non_2xx_response`.
    /// 5. Parse + translate + is_empty check.
    /// 6. `record_non_streaming_success`.
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
            return self.record_and_fail(
                params.req,
                params.combo,
                params.target,
                dctx.fail_ctx_code(
                    &CoreError::Cancelled(reason),
                    Some(elapsed),
                    None,
                    CoreError::Cancelled(reason).http_status(),
                ),
            );
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
            response
                .headers
                .iter()
                .map(|(k, v)| {
                    (
                        k.as_str().to_string(),
                        v.to_str().unwrap_or_default().to_string(),
                    )
                })
                .collect::<std::collections::BTreeMap<String, String>>()
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

        let response_body_raw: serde_json::Value = match serde_json::from_slice(&body_bytes) {
            Ok(v) => v,
            Err(e) => {
                let err = CoreError::Parse(format!("invalid json in upstream response: {e}"));
                return self.record_and_fail(
                    params.req,
                    params.combo,
                    params.target,
                    dctx.fail_ctx_code(
                        &err,
                        Some(connect_and_send_ms),
                        Some(ttft_ms),
                        err.http_status(),
                    ),
                );
            }
        };

        let openai_response = match translate_non_streaming_body(
            params.target_format,
            &response_body_raw,
            &params.req,
        ) {
            Ok(r) => extract_think_from_response(r),
            Err(err) => {
                return self.record_and_fail(
                    params.req,
                    params.combo,
                    params.target,
                    dctx.fail_ctx_code(
                        &err,
                        Some(connect_and_send_ms),
                        Some(ttft_ms),
                        err.http_status(),
                    ),
                );
            }
        };

        if is_empty_response(&openai_response) {
            let err = CoreError::UpstreamConnection(
                "upstream returned 200 but response is empty (content=null, finish_reason=null, no tool_calls, no reasoning) — treating as error for retry".to_string(),
            );
            return self.record_and_fail(
                params.req,
                params.combo,
                params.target,
                dctx.fail_ctx_code(&err, Some(connect_and_send_ms), Some(ttft_ms), 502),
            );
        }

        self.record_non_streaming_success(
            params,
            &dctx,
            NonStreamingSuccessArgs {
                status_code,
                connect_and_send_ms,
                ttft_ms,
                response_headers,
                response_body_raw,
                openai_response,
            },
        )
    }
}
