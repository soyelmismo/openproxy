//! `FormattingStage`: final body/format adaptation for the chosen target
//! (target format, streaming flags, stream-body enforcement) and the shared
//! stream-policy helpers used by both the unary and streaming dispatchers.

use crate::context::PipelineContext;
use crate::stage::{PipelineNext, PipelineStage};
use crate::{FailureContext, PipelineResult};
use openproxy_types::error::CoreError;

use super::prefix::{prepare_messages_for_formatting, resolve_system_prefix_for_target};
use super::resolve_target_format;

#[derive(Clone, Copy)]
pub struct FormattingStage;

impl PipelineStage for FormattingStage {
    async fn execute(
        &self,
        ctx: &mut PipelineContext,
        next: PipelineNext<'_>,
    ) -> Result<PipelineResult, CoreError> {
        let current = ctx.current_target.as_ref().ok_or_else(|| {
            CoreError::Internal("missing current_target in pipeline context".into())
        })?;
        let Some(adapter) = crate::context::pinned_adapter(
            &ctx.req,
            &ctx.pipeline.config.adapters,
            &current.target.provider_id,
        ) else {
            let err = CoreError::ProviderNotFound(current.target.provider_id.to_string());
            return fail_stage!(ctx, &current.target, &err, None);
        };

        let mut target_format = resolve_target_format(&adapter, current.model.target_format);
        if current.target.provider_id.as_str() == "zai"
            && let Some(acc_id) = current.target.account_id
        {
            let key = std::sync::Arc::clone(&ctx.pipeline.config.master_key);
            if let Ok(Some(acc)) = ctx
                .pipeline
                .async_repo()
                .run(move |repo| repo.get_account(acc_id, &key))
                .await
            {
                let now = openproxy_types::now_unix_secs_str()
                    .parse::<u64>()
                    .unwrap_or(0);
                let fallback = current
                    .custom_meta
                    .as_ref()
                    .map_or(current.api_key.as_str(), |m| m.access_token.as_str());
                if let Ok(route) = openproxy_adapters::adapters::zai::resolve_zai_inference_route(
                    &acc,
                    fallback,
                    current.model.model_id.as_str(),
                    now,
                    &adapter.config().base_url,
                ) && route.source == openproxy_types::quota::QuotaSource::AutoclawStarter
                {
                    target_format = openproxy_types::TargetFormat::Openai;
                }
            }
        }
        let model_streaming = current.model.capabilities().and_then(|c| c.streaming);
        let adapter_streaming = adapter.forced_streaming();
        let stream = resolve_upstream_streaming(model_streaming, adapter_streaming);

        // Resolve and inject system prefix for this target if configured
        // (Supports inheritance: if this target was flattened from a parent sub-combo target,
        // it checks its own prefixes first; if empty, inherits the parent target's prefixes).
        let prefix_text = resolve_system_prefix_for_target(
            ctx,
            current.target.id,
            current.target.inherited_target_id,
        )
        .await;
        if let Some(ref p) = prefix_text {
            *ctx.req.active_system_prefix.lock() = Some(p.clone());
        }

        let base_messages_ref = prepare_messages_for_formatting(ctx);
        let messages_storage: Vec<openproxy_types::OpenAIMessage>;
        let messages_ref: &[openproxy_types::OpenAIMessage] = if let Some(ref prefix) = prefix_text
        {
            let mut msgs = Vec::with_capacity(base_messages_ref.len() + 1);
            msgs.push(openproxy_types::OpenAIMessage {
                role: "system".to_string(),
                content: Some(serde_json::Value::String(prefix.clone())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: Default::default(),
            });
            msgs.extend(base_messages_ref.iter().cloned());
            messages_storage = msgs;
            &messages_storage
        } else {
            base_messages_ref
        };

        let policy = crate::policy::EffectiveTargetPolicy::resolve(
            ctx.combo.as_ref(),
            &current.target,
            &current.model,
        );
        let effective_model = policy.apply_to_model(&current.model);

        let mut req_override;
        let req_ref = if current.target.thinking_effort.is_some()
            || (ctx.req.openai_request.max_tokens.is_none()
                && !ctx
                    .req
                    .openai_request
                    .extra
                    .contains_key("max_completion_tokens")
                && policy.max_output_tokens.is_some())
        {
            req_override = ctx.req.clone();
            req_override.openai_request = policy.normalize_request(&ctx.req.openai_request);
            &req_override
        } else {
            &ctx.req
        };

        let formatter = crate::formatting::get_formatter(target_format);
        let (effective_messages_ref, synthetic_thread_header) =
            crate::message_threads::resolve_synthetic_thread_plan(
                ctx,
                target_format,
                adapter.id().as_str(),
                messages_ref,
            );

        let body_bytes = match formatter
            .format_request(
                req_ref,
                &effective_model,
                effective_messages_ref,
                stream,
                &adapter,
            )
            .and_then(|body| {
                if let Some(thread_obj) = synthetic_thread_header.as_ref() {
                    openproxy_adapters::adapters::traits::patch_json_request_body(body, |obj| {
                        obj.insert("thread".to_string(), thread_obj.clone());
                    })
                } else {
                    Ok(body)
                }
            })
            .and_then(|body| {
                adapter.wrap_request_body(body, target_format, &current.model.model_id, current)
            })
            .and_then(|body| {
                if let Some(profile) = current.client_profile()
                    && adapter.id().as_str() != "claude-code"
                {
                    crate::stages::profile_cloaking::wrap_profile_request_body(
                        body,
                        profile,
                        target_format,
                        current,
                    )
                } else {
                    Ok(body)
                }
            })
            .and_then(|body| {
                // Devin bodies are protobuf envelopes by this point — JSON
                // param stripping would fail the parse on binary input.
                if target_format == openproxy_types::TargetFormat::Devin {
                    return Ok(body);
                }
                crate::formatting::strip_provider_configured_params_from_body(
                    body,
                    &adapter.config().extra_headers,
                )
            })
            .and_then(|body| enforce_stream_body(body, target_format, stream))
        {
            Ok(b) => b,
            Err(e) => return fail_stage!(ctx, &current.target, &e, Some(&current.model)),
        };

        ctx.target_format = Some(target_format);
        ctx.body_bytes = Some(body_bytes);
        ctx.is_streaming = Some(stream);
        next.execute(ctx).await
    }
}

/// Unary at either scope is an explicit veto. Auto always streams upstream,
/// independently of the downstream response format requested by the client.
pub(super) fn resolve_upstream_streaming(model: Option<bool>, provider: Option<bool>) -> bool {
    model != Some(false) && provider != Some(false)
}

/// Provider wrappers and parameter stripping cannot silently override transport
/// policy. Apply it before request finalization/signing, in the native dialect.
pub(super) fn enforce_stream_body(
    body: bytes::Bytes,
    format: openproxy_types::TargetFormat,
    stream: bool,
) -> Result<bytes::Bytes, CoreError> {
    if matches!(
        format,
        openproxy_types::TargetFormat::Gemini | openproxy_types::TargetFormat::Devin
    ) {
        // Gemini selects streaming through the URL, not JSON. Devin's body is
        // a Connect-RPC protobuf envelope: JSON patching would corrupt it and
        // the RPC has no stream flag (the endpoint is always server-streaming).
        return Ok(body);
    }
    openproxy_adapters::adapters::traits::patch_json_request_body(body, |obj| {
        if format == openproxy_types::TargetFormat::CommandCodeGo {
            if let Some(params) = obj
                .get_mut("params")
                .and_then(serde_json::Value::as_object_mut)
            {
                params.insert("stream".into(), serde_json::Value::Bool(stream));
            }
        } else {
            obj.insert("stream".into(), serde_json::Value::Bool(stream));
            if !stream {
                obj.remove("stream_options");
            }
        }
    })
}

pub(super) fn stream_request_url(url: String, stream: bool) -> String {
    if stream || !url.contains(":streamGenerateContent") {
        return url;
    }
    // Both Gemini and Antigravity use the same RPC suffix. Unary must not
    // retain the SSE query parameter even when the adapter supplied the URL.
    let url = url.replace(":streamGenerateContent", ":generateContent");
    let Some((base, query)) = url.split_once('?') else {
        return url;
    };
    let query = query
        .split('&')
        .filter(|part| *part != "alt=sse")
        .collect::<Vec<_>>()
        .join("&");
    if query.is_empty() {
        base.to_owned()
    } else {
        format!("{base}?{query}")
    }
}
