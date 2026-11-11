//! `dispatch_upstream_non_streaming`: the single upstream send for unary
//! responses, including retry preconditions and empty-response handling.
//! Split from `dispatcher/unary.rs` to keep each file under the LOC invariant.

use super::super::types::{DispatchContext, DispatchParams, NonStreamingSuccessArgs};
use super::super::{Dispatcher, UpstreamDispatcher};
use crate::PipelineResult;
use crate::inline_tools::extract_inline_tools_from_response;
use crate::think_extractor::extract_think_from_response;
use openproxy_adapters::upstream::UpstreamRequest;
use openproxy_types::error::CoreError;
use std::time::Instant;

use super::super::translation::ensure_usage_populated;
use super::super::translation::translate_non_streaming_body;
use super::is_empty_response;
use super::sink::emit_unary_response_to_stream_sink;

impl UpstreamDispatcher {
    pub(crate) async fn dispatch_upstream_non_streaming(
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
            session_hash: crate::session_affinity::SessionAffinityRegistry::extract_session_hash(&params.req),
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
