use crate::context::PipelineContext;
use crate::retry::RetryPolicy;
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
            .record_and_fail($ctx.req.clone(), combo, $target, fail_ctx))
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
        Ok($ctx.pipeline.record_and_fail_with_trace_id(
            $ctx.req.clone(),
            combo,
            $target,
            fail_ctx,
            $ctx.trace_id.clone(),
        ))
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
    let Some(account_id) = current.target.account_id else { return; };
    let Some(custom_meta) = current.custom_meta.as_mut() else { return; };
    let Some(refresh_token) = custom_meta.maybe_refresh.as_ref() else { return; };
    let Some(registry) = pipeline.config.oauth_provider_registry.as_ref() else { return; };

    let provider_id_str = current.target.provider_id.as_str();
    tracing::info!(
        account = account_id.0,
        provider = provider_id_str,
        "pipeline: proactive OAuth token refresh"
    );
    match registry
        .refresh_and_store(
            provider_id_str,
            refresh_token,
            &pipeline.config.upstream_client,
            account_id,
            &pipeline.conn,
            &pipeline.config.master_key,
        )
        .await
    {
        Ok(token) => {
            custom_meta.access_token = token.access_token;
        }
        Err(e) => {
            tracing::warn!(
                account = account_id.0,
                provider = provider_id_str,
                error = %e,
                "pipeline: proactive OAuth refresh failed, continuing with existing token"
            );
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
        let cloned_model = current.model.clone(); let model = &cloned_model;

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
        let stream = ctx.req.openai_request.stream || ctx.req.stream_sink.is_some();
        let messages_ref = prepare_messages_for_formatting(ctx);

        let formatter = crate::formatting::get_formatter(target_format);
        let body_bytes = match formatter
            .format_request(&ctx.req, &current.model, messages_ref, stream, adapter)
            .and_then(|body| {
                adapter.wrap_request_body(body, target_format, &current.model.model_id, current)
            }) {
            Ok(b) => b,
            Err(e) => return fail_stage!(ctx, &current.target, &e, Some(&current.model)),
        };

        ctx.target_format = Some(target_format);
        ctx.body_bytes = Some(body_bytes);
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
        openproxy_adapters::adapters::AdapterFormat::Fx => openproxy_types::TargetFormat::Fx,
    }
}

fn prepare_messages_for_formatting(
    ctx: &PipelineContext,
) -> &[openproxy_types::OpenAIMessage] {
    let cloned_messages_ref = ctx.req.compressed_messages.get_or_init(|| {
        if openproxy_compression::would_compress(
            &ctx.req.openai_request.messages,
            ctx.pipeline.config.compression_mode,
        ) {
            let mut msgs = ctx.req.openai_request.messages.clone();
            let stats = openproxy_compression::apply_compression(
                &mut msgs,
                ctx.pipeline.config.compression_mode,
            );
            *ctx.pipeline.compression_stats_cell.write() = Some(stats);
            Some(msgs)
        } else {
            *ctx.pipeline.compression_stats_cell.write() =
                Some(openproxy_compression::stats::CompressionStats::empty());
            None
        }
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
        let cloned_target = current.target.clone(); let target = &cloned_target;
        let cloned_model = current.model.clone(); let model = &cloned_model;
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
        let url =
            adapter.build_chat_url_for_account(target_format, &model.model_id, account_label_str);
        let headers = adapter.build_headers(api_key, target_format, &model.model_id);

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

        let result = ctx
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
                body_bytes,
                resolved_timeouts: &resolved_timeouts,
                started,
                attempt,
                race_size,
                trace_id,
            })
            .await;

        update_circuit_breaker_on_result(&ctx.pipeline, target, model, &result);

        // Do not call next.execute here. We are the final stage of target execution.
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
    let Some(custom_meta) = current.custom_meta.as_mut() else { return; };
    if custom_meta.antigravity_project.is_some() {
        return;
    }
    let Some(ref meta_str) = custom_meta.antigravity_metadata else { return; };
    let Ok(metadata) = serde_json::from_str::<serde_json::Value>(meta_str) else { return; };

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
            if let Some(ref account_id) = current.target.account_id
                && let Err(e) = pipeline
                    .repo()
                    .update_antigravity_project_id(account_id.0, &pid)
            {
                tracing::error!("Failed to update antigravity project id in db: {}", e);
            }
        }
        Ok(None) => tracing::warn!("loadCodeAssist returned Ok(None)"),
        Err(e) => tracing::error!("loadCodeAssist failed: {}", e),
    }
}

fn update_circuit_breaker_on_result(
    pipeline: &crate::Pipeline,
    target: &openproxy_types::ComboTarget,
    model: &openproxy_types::models::Model,
    result: &PipelineResult,
) {
    let Some(aid) = target.account_id else { return; };
    let key = crate::circuit_breaker::CircuitBreakerKey::from_target(
        aid,
        target.rate_limit_scope,
        target.model_row_id,
    );

    match &result.error {
        Some(CoreError::Cancelled(openproxy_types::CancelReason::ClientDisconnected)) => {
            tracing::debug!(
                account_id = aid.0,
                "client cancelled; leaving circuit breaker untouched"
            );
        }
        Some(e) if RetryPolicy::is_retryable(e, pipeline.config.idle_chunk_retryable) => {
            let outcome = pipeline.circuit_breaker.record_failure_outcome(key);
            if outcome.just_opened {
                notify_circuit_breaker_opened(
                    pipeline,
                    target,
                    model,
                    aid.0,
                    outcome.consecutive_failures.into(),
                    outcome.threshold.into(),
                );
            }
        }
        _ => {
            pipeline.circuit_breaker.record_success(key);
        }
    }
}

fn notify_circuit_breaker_opened(
    pipeline: &crate::Pipeline,
    target: &openproxy_types::ComboTarget,
    model: &openproxy_types::models::Model,
    account_id: i64,
    consecutive_failures: u32,
    threshold: u32,
) {
    let provider_id_str = target.provider_id.to_string();
    let model_id_str = model.model_id.as_str().to_string();
    let combo_target_id = target.id.0;
    let dedup_key = format!("circuit_open:{account_id}");
    let payload = serde_json::json!({
        "code": "circuit_open",
        "message": format!(
            "Circuit breaker opened for account {} on {} ({}) — {}/{} failures",
            account_id, provider_id_str, model_id_str,
            consecutive_failures, threshold,
        ),
        "provider_id": &provider_id_str,
        "details": {
            "combo_target_id": combo_target_id,
            "account_id": account_id,
            "provider_id": &provider_id_str,
            "model_id": &model_id_str,
            "failure_count": consecutive_failures,
            "threshold": threshold,
        },
    });
    let repo = pipeline.repo();
    tokio::task::spawn_blocking(move || {
        let _ = repo.insert_and_broadcast_notification(
            "system",
            &payload,
            Some(&dedup_key),
            Some(&provider_id_str),
        );
    });
}

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
        let cloned_target = current.target.clone(); let target = &cloned_target;
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
