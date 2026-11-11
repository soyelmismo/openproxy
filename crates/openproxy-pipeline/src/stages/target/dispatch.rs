//! `DispatchStage` and `CustomAdapterStage`: the terminal stages that send
//! the request upstream (including the antigravity lazy project fetch) and
//! delegate to custom adapter overrides.

use crate::context::PipelineContext;
use crate::stage::{PipelineNext, PipelineStage};
use crate::{FailureContext, PipelineResult};
use openproxy_types::error::CoreError;

use super::formatting::stream_request_url;

#[derive(Clone, Copy)]
pub struct DispatchStage;

impl PipelineStage for DispatchStage {
    async fn execute(
        &self,
        ctx: &mut PipelineContext,
        _next: PipelineNext<'_>,
    ) -> Result<PipelineResult, CoreError> {
        let current = ctx.current_target.as_mut().ok_or_else(|| {
            CoreError::Internal("missing current_target in pipeline context".into())
        })?;
        let cloned_target = current.target.clone();
        let target = &cloned_target;
        let cloned_model = current.model.clone();
        let model = &cloned_model;
        let attempt = ctx.current_target_attempt;
        let race_size = ctx.race_size;
        let started = ctx.started.unwrap_or_else(std::time::Instant::now);
        let trace_id = ctx.trace_id.clone();
        let combo = ctx
            .combo
            .as_ref()
            .ok_or_else(|| CoreError::Internal("missing combo in pipeline context".into()))?;

        let Some(adapter) = crate::context::pinned_adapter(
            &ctx.req,
            &ctx.pipeline.config.adapters,
            &target.provider_id,
        ) else {
            let err = CoreError::ProviderNotFound(target.provider_id.to_string());
            return fail_stage!(ctx, target, &err, Some(model));
        };

        if ctx.race_cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
            return fail_stage!(
                with_trace;
                ctx,
                target,
                &CoreError::RaceLost,
                Some(model),
                CoreError::RaceLost.http_status()
            );
        }

        try_lazy_fetch_antigravity_project(&ctx.pipeline, current).await;

        let api_key = current
            .custom_meta
            .as_ref()
            .map_or(current.api_key.as_str(), |m| m.access_token.as_str());
        let account_label_str = current.api_key_label.as_deref().unwrap_or("");

        let target_format = ctx.target_format.ok_or_else(|| {
            CoreError::Internal("missing target_format in pipeline context".into())
        })?;
        let mut zai_route = if target.provider_id.as_str() == "zai" {
            match crate::stages::zai::prepare_zai_route(
                &ctx.pipeline,
                current,
                &adapter.config().base_url,
                ctx.req.proxy_override.as_ref(),
            )
            .await
            {
                Ok(route) => Some(route),
                Err(err) => return fail_stage!(ctx, target, &err, Some(model), err.http_status()),
            }
        } else {
            None
        };
        let url = zai_route.as_ref().map_or_else(
            || {
                adapter.build_chat_url_for_account(
                    target_format,
                    &model.model_id,
                    account_label_str,
                )
            },
            |prepared| prepared.route.url.clone(),
        );
        let url = stream_request_url(url, ctx.is_streaming.unwrap_or(false));
        let effective_key = zai_route
            .as_ref()
            .map_or(api_key, |prepared| prepared.route.credential.as_str());
        let mut headers = adapter.build_headers(effective_key, target_format, &model.model_id);
        let codex_ws = current
            .custom_meta
            .as_ref()
            .and_then(|m| m.codex_workspace_id.as_deref());
        crate::stages::target_headers::propagate_provider_target_headers_with_profile(
            &mut headers,
            target.provider_id.as_str(),
            adapter.id().as_str(),
            &ctx.req.request_headers,
            &ctx.req.openai_request,
            codex_ws,
            current.client_profile(),
        );

        openproxy_types::emit_stage_event!(
            request_id: ctx.req.request_id,
            trace_id: trace_id,
            stage: "connecting",
            elapsed_ms: started.elapsed().as_millis() as u64,
            session_hash: crate::session_affinity::SessionAffinityRegistry::extract_session_hash(&ctx.req),
            api_key_id: ctx.req.api_key_id,
        );

        let body_bytes = ctx
            .body_bytes
            .take()
            .ok_or_else(|| CoreError::Internal("missing body_bytes in pipeline context".into()))?;
        // Finalize only after formatting, wrapping, parameter stripping and
        // header propagation. Signatures and body-derived headers must describe
        // the exact bytes sent to the upstream, independently of the schema.
        let body_bytes = match adapter.finalize_request(
            body_bytes,
            &mut headers,
            target_format,
            &model.model_id,
            current,
        ) {
            Ok(body) => body,
            Err(error) => return fail_stage!(ctx, target, &error, Some(model)),
        };

        // If an explicit client profile is active (and provider isn't already the native one),
        // apply the client profile's finalization rules (e.g. CCH signature, billing headers).
        let body_bytes = if let Some(profile) = current.client_profile()
            && adapter.id().as_str() != "claude-code"
        {
            match crate::stages::profile_cloaking::finalize_profile_request(
                body_bytes,
                &mut headers,
                profile,
                target_format,
                current,
            ) {
                Ok(body) => body,
                Err(error) => return fail_stage!(ctx, target, &error, Some(model)),
            }
        } else {
            body_bytes
        };
        let resolved_timeouts = ctx.resolved_timeouts.ok_or_else(|| {
            CoreError::Internal("missing resolved_timeouts in pipeline context".into())
        })?;

        let mut result = ctx
            .pipeline
            .dispatcher
            .dispatch_upstream(crate::upstream_dispatcher::DispatchParams {
                target,
                combo,
                req: ctx.req.clone(),
                model,
                target_format,
                url: &url,
                headers: &headers,
                body_bytes: body_bytes.clone(),
                resolved_timeouts: &resolved_timeouts,
                started,
                attempt,
                race_size,
                trace_id,
                is_streaming: ctx.is_streaming.unwrap_or(false),
            })
            .await;

        let starter_depleted = zai_route.as_ref().is_some_and(|prepared| {
            prepared.route.source == openproxy_types::quota::QuotaSource::AutoclawStarter
                || prepared.route.source == openproxy_types::quota::QuotaSource::ZcodeStarter
        }) && result
            .error
            .as_ref()
            .is_some_and(openproxy_adapters::adapters::zai::is_zcode_entitlement_exhaustion);
        if starter_depleted
            // A streaming request can still be retried while nothing has been
            // written to the client: the Depleted signal is raised by
            // `dispatch_upstream` itself, before any byte reaches the sink.
            // Gating on `is_streaming` alone therefore discards the paid quota
            // on every streaming request and turns a recoverable exhaustion
            // into a hard 403 for the caller.
            && !ctx.req.has_stream_output()
            && let Some(prepared) = zai_route.as_mut()
            && crate::stages::zai::prepare_zai_paid_fallback(
                &ctx.pipeline,
                prepared,
                model.model_id.as_str(),
                &adapter.config().base_url,
            )
            .await
            .is_ok()
            && !ctx
                .race_cancel
                .as_ref()
                .is_some_and(|cancel| cancel.is_cancelled())
        {
            let mut paid_headers =
                adapter.build_headers(&prepared.route.credential, target_format, &model.model_id);
            crate::stages::target_headers::propagate_provider_target_headers_with_profile(
                &mut paid_headers,
                target.provider_id.as_str(),
                adapter.id().as_str(),
                &ctx.req.request_headers,
                &ctx.req.openai_request,
                codex_ws,
                current.client_profile(),
            );
            let body_bytes = match adapter.finalize_request(
                body_bytes,
                &mut paid_headers,
                target_format,
                &model.model_id,
                current,
            ) {
                Ok(body) => body,
                Err(error) => return fail_stage!(ctx, target, &error, Some(model)),
            };
            result = ctx
                .pipeline
                .dispatcher
                .dispatch_upstream(crate::upstream_dispatcher::DispatchParams {
                    target,
                    combo,
                    req: ctx.req.clone(),
                    model,
                    target_format,
                    url: &prepared.route.url,
                    headers: &paid_headers,
                    body_bytes,
                    resolved_timeouts: &resolved_timeouts,
                    started,
                    attempt,
                    race_size,
                    trace_id: ctx.trace_id.clone(),
                    is_streaming: ctx.is_streaming.unwrap_or(false),
                })
                .await;
        }

        if let Some(prepared) = zai_route.as_mut()
            && let Some(error) = result.error.as_ref()
            && let Err(error) = crate::stages::zai::record_zai_route_failure(
                &ctx.pipeline,
                prepared,
                model.model_id.as_str(),
                error,
            )
            .await
        {
            return fail_stage!(ctx, target, &error, Some(model), error.http_status());
        }
        update_circuit_breaker_on_result(&ctx.pipeline, target, model, &result);
        update_predictive_limiter_on_result(&ctx.pipeline, target, &result);
        update_account_rate_limited_until_on_result(&ctx.pipeline, target, &result);

        Ok(result)
    }
}

async fn try_lazy_fetch_antigravity_project(
    pipeline: &crate::Pipeline,
    current: &mut crate::context::ResolvedTarget,
) {
    if current.target.provider_id.as_str() != "antigravity" {
        return;
    }
    let Some(custom_meta) = current.custom_meta.as_mut() else {
        return;
    };
    if custom_meta.antigravity_project.is_some() {
        return;
    }
    let Some(ref meta_str) = custom_meta.antigravity_metadata else {
        return;
    };
    let Ok(metadata) = serde_json::from_str::<serde_json::Value>(meta_str) else {
        return;
    };

    tracing::info!(
        "Lazy fetching antigravity projectId for target {}",
        current.target.id.0
    );
    match openproxy_adapters::adapters::antigravity::load_code_assist(
        &pipeline.config.upstream_client,
        &custom_meta.access_token,
        &metadata,
    )
    .await
    {
        Ok(Some(pid)) => {
            tracing::info!("Successfully fetched antigravity projectId: {}", pid);
            custom_meta.antigravity_project = Some(pid.clone());
            if let Some(account_id) = current.target.account_id {
                let result = pipeline
                    .async_repo()
                    .run(move |repo| repo.update_antigravity_project_id(account_id.0, &pid))
                    .await;
                if let Err(e) = result {
                    tracing::error!("Failed to update antigravity project id in db: {}", e);
                }
            }
        }
        Ok(None) => tracing::warn!("loadCodeAssist returned Ok(None)"),
        Err(e) => tracing::error!("loadCodeAssist failed: {}", e),
    }
}

pub(crate) use crate::stages::target_breaker::*;

#[derive(Clone, Copy)]
pub struct CustomAdapterStage;

impl PipelineStage for CustomAdapterStage {
    async fn execute(
        &self,
        ctx: &mut PipelineContext,
        next: PipelineNext<'_>,
    ) -> Result<PipelineResult, CoreError> {
        let Some(current) = ctx.current_target.as_mut() else {
            return Err(CoreError::Internal(
                "missing current_target in pipeline context".into(),
            ));
        };
        let cloned_target = current.target.clone();
        let target = &cloned_target;
        let Some(_adapter) = crate::context::pinned_adapter(
            &ctx.req,
            &ctx.pipeline.config.adapters,
            &target.provider_id,
        ) else {
            let err = CoreError::ProviderNotFound(target.provider_id.to_string());
            return fail_stage!(with_trace; ctx, target, &err, Some(&current.model), 0);
        };

        next.execute(ctx).await
    }
}
