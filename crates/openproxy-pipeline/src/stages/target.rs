use crate::context::PipelineContext;
use crate::stage::{PipelineNext, PipelineStage};
use crate::timeouts;
use crate::timeouts::ModelTimeoutOverrides;
use crate::{FailureContext, PipelineResult};
use openproxy_types::error::CoreError;

macro_rules! fail_stage {
    ($ctx:expr, $target:expr, $err:expr, $model:expr) => {
        fail_stage!($ctx, $target, $err, $model, 0)
    };
    ($ctx:expr, $target:expr, $err:expr, $model:expr, $status:expr) => {{
        let combo = $ctx
            .combo
            .as_ref()
            .ok_or_else(|| CoreError::Internal("missing combo in pipeline context".into()))?;
        let fail_ctx = FailureContext {
            proxy_url: None,
            proxy_status: None,
            attempt: $ctx.current_target_attempt,
            race_size: $ctx.race_size,
            err: $err,
            started: $ctx.started.unwrap_or_else(std::time::Instant::now),
            model: $model,
            connect_ms: None,
            ttft_ms: None,
            status_code: $status,
        };
        Ok($ctx
            .pipeline
            .record_and_fail($ctx.req.clone(), combo, $target, fail_ctx)
            .await)
    }};
    (with_trace; $ctx:expr, $target:expr, $err:expr, $model:expr, $status:expr) => {{
        let combo = $ctx
            .combo
            .as_ref()
            .ok_or_else(|| CoreError::Internal("missing combo in pipeline context".into()))?;
        let fail_ctx = FailureContext {
            proxy_url: None,
            proxy_status: None,
            attempt: $ctx.current_target_attempt,
            race_size: $ctx.race_size,
            err: $err,
            started: $ctx.started.unwrap_or_else(std::time::Instant::now),
            model: $model,
            connect_ms: None,
            ttft_ms: None,
            status_code: $status,
        };
        Ok($ctx
            .pipeline
            .record_and_fail_with_trace_id(
                $ctx.req.clone(),
                combo,
                $target,
                fail_ctx,
                $ctx.trace_id.clone(),
            )
            .await)
    }};
}

#[derive(Clone, Copy)]
pub struct OAuthRefreshStage;

impl PipelineStage for OAuthRefreshStage {
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
        try_proactive_oauth_refresh(&ctx.pipeline, current).await;
        next.execute(ctx).await
    }
}

async fn try_proactive_oauth_refresh(
    pipeline: &crate::Pipeline,
    current: &mut crate::context::ResolvedTarget,
) {
    let Some(account_id) = current.target.account_id else {
        return;
    };
    let Some(custom_meta) = current.custom_meta.as_mut() else {
        return;
    };
    let Some(refresh_token) = custom_meta.maybe_refresh.as_ref() else {
        return;
    };
    let Some(registry) = pipeline.config.oauth_provider_registry.as_ref() else {
        return;
    };

    let provider_id_str = current.target.provider_id.as_str();
    tracing::info!(
        account = account_id.0,
        provider = provider_id_str,
        "pipeline: proactive OAuth token refresh"
    );
    let refresh_res = match pipeline.db_pool.as_ref() {
        Some(pool) => {
            registry
                .refresh_and_store(
                    provider_id_str,
                    refresh_token,
                    &pipeline.config.upstream_client,
                    account_id,
                    Some(pool),
                    &pipeline.config.master_key,
                )
                .await
        }
        None => {
            registry
                .refresh_and_store_shared(
                    provider_id_str,
                    refresh_token,
                    &pipeline.config.upstream_client,
                    account_id,
                    &pipeline.conn,
                    &pipeline.config.master_key,
                )
                .await
        }
    };
    match refresh_res {
        Ok(token) => {
            custom_meta.access_token = token.access_token;
            custom_meta.maybe_refresh = None;
        }
        Err(e) => {
            tracing::warn!(
                account = account_id.0,
                provider = provider_id_str,
                error = %e,
                "pipeline: proactive OAuth refresh failed, continuing with existing token"
            );
            custom_meta.maybe_refresh = None;
        }
    }
}

#[derive(Clone, Copy)]
pub struct TimeoutResolutionStage;

impl PipelineStage for TimeoutResolutionStage {
    async fn execute(
        &self,
        ctx: &mut PipelineContext,
        next: PipelineNext<'_>,
    ) -> Result<PipelineResult, CoreError> {
        let current = ctx.current_target.as_ref().ok_or_else(|| {
            CoreError::Internal("missing current_target in pipeline context".into())
        })?;
        let cloned_model = current.model.clone();
        let model = &cloned_model;

        let model_overrides =
            match ModelTimeoutOverrides::from_json(model.timeout_overrides_json.as_deref()) {
                Ok(o) => o,
                Err(e) => return fail_stage!(ctx, &current.target, &e, Some(model)),
            };

        let resolved_timeouts =
            timeouts::resolve(&ctx.pipeline.config.defaults, Some(&model_overrides));

        tracing::debug!(
            target_id = current.target.id.0,
            provider = %current.target.provider_id,
            model = %model.model_id.as_str(),
            total_ms = resolved_timeouts.total.as_millis() as u64,
            "resolved timeouts for target"
        );

        ctx.resolved_timeouts = Some(resolved_timeouts);
        next.execute(ctx).await
    }
}

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
        let Some(adapter) = ctx
            .pipeline
            .config
            .adapters
            .iter()
            .find(|a| a.id() == &current.target.provider_id)
        else {
            let err = CoreError::ProviderNotFound(current.target.provider_id.to_string());
            return fail_stage!(ctx, &current.target, &err, None);
        };

        let target_format = resolve_target_format(adapter, current.model.target_format);
        let model_streaming = current.model.capabilities().and_then(|c| c.streaming);
        let adapter_streaming = adapter.forced_streaming();
        let stream = match (model_streaming, adapter_streaming) {
            (Some(forced), _) => forced,
            (None, Some(forced)) => forced,
            (None, None) => ctx.req.openai_request.stream || ctx.req.stream_sink.is_some(),
        };
        let messages_ref = prepare_messages_for_formatting(ctx);

        let mut req_override;
        let req_ref = if let Some(ref effort) = current.target.thinking_effort {
            req_override = ctx.req.clone();
            let mut patched_openai_req = (*req_override.openai_request).clone();
            patched_openai_req
                .extra
                .insert("reasoning_effort".to_string(), serde_json::json!(effort));
            req_override.openai_request = std::sync::Arc::new(patched_openai_req);
            &req_override
        } else {
            &ctx.req
        };

        let formatter = crate::formatting::get_formatter(target_format);
        let body_bytes = match formatter
            .format_request(req_ref, &current.model, messages_ref, stream, adapter)
            .and_then(|body| {
                adapter.wrap_request_body(body, target_format, &current.model.model_id, current)
            }) {
            Ok(b) => b,
            Err(e) => return fail_stage!(ctx, &current.target, &e, Some(&current.model)),
        };

        ctx.target_format = Some(target_format);
        ctx.body_bytes = Some(body_bytes);
        ctx.is_streaming = Some(stream);
        next.execute(ctx).await
    }
}

fn resolve_target_format(
    adapter: &openproxy_adapters::adapters::ProviderAdapterEnum,
    model_format: openproxy_types::TargetFormat,
) -> openproxy_types::TargetFormat {
    match adapter.format() {
        openproxy_adapters::adapters::AdapterFormat::Openai => {
            openproxy_types::TargetFormat::Openai
        }
        openproxy_adapters::adapters::AdapterFormat::Anthropic => {
            openproxy_types::TargetFormat::Anthropic
        }
        openproxy_adapters::adapters::AdapterFormat::Mixed => model_format,
        openproxy_adapters::adapters::AdapterFormat::Gemini => {
            openproxy_types::TargetFormat::Gemini
        }
        openproxy_adapters::adapters::AdapterFormat::Responses => {
            openproxy_types::TargetFormat::Responses
        }
        openproxy_adapters::adapters::AdapterFormat::Atomesus => {
            openproxy_types::TargetFormat::Atomesus
        }
        openproxy_adapters::adapters::AdapterFormat::CommandCodeGo => {
            openproxy_types::TargetFormat::CommandCodeGo
        }
        openproxy_adapters::adapters::AdapterFormat::SystemOne => {
            openproxy_types::TargetFormat::SystemOne
        }
    }
}

pub(crate) fn prepare_messages_for_formatting(
    ctx: &PipelineContext,
) -> &[openproxy_types::OpenAIMessage] {
    let cloned_messages_ref = ctx.req.compressed_messages.get_or_init(|| {
        let pii_enabled = ctx.pipeline.config.pii_config.pii_enabled;
        let compression_needed = openproxy_compression::would_compress(
            &ctx.req.openai_request.messages,
            ctx.pipeline.config.compression_mode,
        );

        if !pii_enabled && !compression_needed {
            *ctx.req.compression_stats.lock() =
                Some(openproxy_compression::stats::CompressionStats::empty());
            return None;
        }

        let mut msgs = ctx.req.openai_request.messages.clone();

        if pii_enabled {
            let engine = crate::pii::PiiEngine::from_config(&ctx.pipeline.config.pii_config);
            let mut session =
                crate::pii::PiiSession::new(ctx.pipeline.config.pii_config.pii_reversible);
            let redacted = engine.redact_messages(&msgs, &mut session);
            msgs = redacted;
            *ctx.req.pii_session.lock() = Some(session);
        }

        if compression_needed {
            let stats = openproxy_compression::apply_compression(
                &mut msgs,
                ctx.pipeline.config.compression_mode,
            );
            *ctx.req.compression_stats.lock() = Some(stats);
        } else {
            *ctx.req.compression_stats.lock() =
                Some(openproxy_compression::stats::CompressionStats::empty());
        }

        Some(msgs)
    });

    cloned_messages_ref
        .as_deref()
        .unwrap_or(&ctx.req.openai_request.messages)
}

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

        let Some(adapter) = ctx
            .pipeline
            .config
            .adapters
            .iter()
            .find(|a| a.id() == &target.provider_id)
        else {
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
            match super::zai::prepare_zai_route(
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
        let effective_key = zai_route
            .as_ref()
            .map_or(api_key, |prepared| prepared.route.credential.as_str());
        let mut headers = adapter.build_headers(effective_key, target_format, &model.model_id);
        let codex_ws = current
            .custom_meta
            .as_ref()
            .and_then(|m| m.codex_workspace_id.as_deref());
        propagate_provider_target_headers(
            &mut headers,
            target.provider_id.as_str(),
            adapter.id().as_str(),
            &ctx.req.request_headers,
            &ctx.req.openai_request,
            codex_ws,
        );

        openproxy_types::emit_stage_event!(
            request_id: ctx.req.request_id,
            trace_id: trace_id,
            stage: "connecting",
            elapsed_ms: started.elapsed().as_millis() as u64,
        );

        let body_bytes = ctx
            .body_bytes
            .take()
            .ok_or_else(|| CoreError::Internal("missing body_bytes in pipeline context".into()))?;
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
            prepared.route.source == openproxy_types::quota::QuotaSource::ZcodeStarter
        }) && result
            .error
            .as_ref()
            .is_some_and(openproxy_adapters::adapters::zai::is_zcode_entitlement_exhaustion);
        if starter_depleted
            && !ctx.is_streaming.unwrap_or(false)
            && ctx.req.stream_sink.is_none()
            && let Some(prepared) = zai_route.as_mut()
            && super::zai::prepare_zai_paid_fallback(
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
            propagate_provider_target_headers(
                &mut paid_headers,
                target.provider_id.as_str(),
                adapter.id().as_str(),
                &ctx.req.request_headers,
                &ctx.req.openai_request,
                codex_ws,
            );
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
            && let Err(error) = super::zai::record_zai_route_failure(
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

pub(crate) use super::target_breaker::*;

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
        let Some(_adapter) = ctx
            .pipeline
            .config
            .adapters
            .iter()
            .find(|a| a.id() == &target.provider_id)
        else {
            let err = CoreError::ProviderNotFound(target.provider_id.to_string());
            return fail_stage!(with_trace; ctx, target, &err, Some(&current.model), 0);
        };

        next.execute(ctx).await
    }
}

use super::target_headers::propagate_provider_target_headers;

#[cfg(test)]
#[path = "target_tests.rs"]
mod tests;
