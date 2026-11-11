//! Unary response lifecycle: body collection with deadline and the
//! success accounting (`record_non_streaming_success`). Split from
//! `dispatcher/unary.rs` to keep each file under the LOC invariant.

use super::super::UpstreamDispatcher;
use super::super::types::{DispatchContext, DispatchParams, NonStreamingSuccessArgs};
use crate::PipelineResult;
use openproxy_adapters::upstream::UpstreamError;
use openproxy_types::error::CoreError;
use std::time::Instant;

impl UpstreamDispatcher {
    /// Lee el body con deadline total desde `params.started`. Un status
    /// non-2xx acota el read a 5s para no penalizar los retries.
    pub(crate) async fn collect_non_streaming_body(
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
                    super::super::rotation::ProxyRotationTrigger::ConnectError,
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
                    super::super::rotation::ProxyRotationTrigger::ConnectError,
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
    pub(crate) async fn record_non_streaming_success(
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
}
