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

/// Adaptive budgets for one target (see `crate::latency_budget`): shortens
/// the resolved TTFT to the learned per-bucket budget (the larger of
/// `mean + 3σ` and the empirical p99, plus a 1s margin) of recent successful
/// requests of the same (provider, model_row), and the resolved idle_chunk
/// timeout to the learned inter-chunk gap budget
/// (`latency_budget::chunk_budget`). No provider is excluded:
/// antigravity's resolved timeout already carries the 120s thinking floor,
/// which acts as the natural ceiling - a thinking target whose learned
/// budget reaches it is simply not predicted, while fast non-thinking
/// targets get cut promptly when they hang. The predictor only shortens:
/// model overrides and provider floors act as hard ceilings, and cuts relax
/// the next budget by 1.5x (anti-starvation, see the executor retry loop).
pub(crate) fn apply_predictive_ttft_budget(
    pipeline: &crate::Pipeline,
    req: &crate::PipelineRequest,
    provider_str: &str,
    target: &openproxy_types::combos::ComboTarget,
    model: &openproxy_types::models::Model,
    mut resolved: crate::timeouts::Timeouts,
) -> crate::timeouts::Timeouts {
    let effective_messages = req
        .compressed_messages
        .get()
        .cloned()
        .flatten()
        .unwrap_or_else(|| req.openai_request.messages.clone());
    let est_prompt_tokens =
        openproxy_compression::token_estimate::estimate_prompt_tokens(&effective_messages).max(1);
    let session_hash = crate::session_affinity::SessionAffinityRegistry::extract_session_hash(req);
    let budget_ms = pipeline.latency_budget.ttft_budget_ms(
        provider_str,
        model.row_id,
        session_hash,
        est_prompt_tokens,
        resolved.ttft.as_millis() as u64,
        // The shared connection is only locked in sync mode (no DB pool,
        // i.e. tests); in production the hot path never touches it
        // (correction 4).
        Some(&pipeline.conn),
    );
    if let Some(budget_ms) = budget_ms
        && (budget_ms as u128) < resolved.ttft.as_millis()
    {
        openproxy_types::emit_stage_event!(
            request_id: req.request_id,
            trace_id: req.trace_id,
            stage: "predictive_ttft_budget",
            elapsed_ms: budget_ms,
            session_hash: crate::session_affinity::SessionAffinityRegistry::extract_session_hash(req),
            provider_id: provider_str.to_string(),
            upstream_model_id: model.model_id.as_str().to_string(),
        );
        tracing::debug!(
            target_id = target.id.0,
            provider = %provider_str,
            model_row_id = model.row_id.0,
            est_prompt_tokens,
            budget_ms,
            resolved_ms = resolved.ttft.as_millis() as u64,
            "adaptive ttft budget applied"
        );
        resolved.ttft = std::time::Duration::from_millis(budget_ms);
    }
    // Learned idle-chunk budget (streaming): same engine, gap distribution.
    // No prompt-size bucketing, so no token estimate is involved. Only
    // shortens the resolved idle_chunk timeout, never lengthens it.
    let chunk_budget_ms = pipeline.latency_budget.idle_chunk_budget_ms(
        provider_str,
        model.row_id,
        session_hash,
        resolved.idle_chunk.as_millis() as u64,
        Some(&pipeline.conn),
    );
    if let Some(budget_ms) = chunk_budget_ms
        && (budget_ms as u128) < resolved.idle_chunk.as_millis()
    {
        openproxy_types::emit_stage_event!(
            request_id: req.request_id,
            trace_id: req.trace_id,
            stage: "predictive_idle_chunk_budget",
            elapsed_ms: budget_ms,
            session_hash: crate::session_affinity::SessionAffinityRegistry::extract_session_hash(req),
            provider_id: provider_str.to_string(),
            upstream_model_id: model.model_id.as_str().to_string(),
        );
        tracing::debug!(
            target_id = target.id.0,
            provider = %provider_str,
            model_row_id = model.row_id.0,
            budget_ms,
            resolved_ms = resolved.idle_chunk.as_millis() as u64,
            "adaptive idle chunk budget applied"
        );
        resolved.idle_chunk = std::time::Duration::from_millis(budget_ms);
    }
    resolved
}

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

        let mut resolved_timeouts =
            timeouts::resolve(&ctx.pipeline.config.defaults, Some(&model_overrides));

        let provider_str = current.target.provider_id.as_str();
        if provider_str == "antigravity" || provider_str == "agy" {
            resolved_timeouts = timeouts::apply_antigravity_floor(resolved_timeouts);
        }

        // Remember the resolved ceiling (pre-predictor) so the executor
        // retry bump (anti-flapping, correction 8) can cap itself.
        ctx.ttft_ceiling_ms = Some(resolved_timeouts.ttft.as_millis() as u64);

        resolved_timeouts = apply_predictive_ttft_budget(
            &ctx.pipeline,
            &ctx.req,
            provider_str,
            &current.target,
            model,
            resolved_timeouts,
        );

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

pub(super) fn resolve_target_format(
    adapter: &openproxy_adapters::adapters::ProviderAdapterEnum,
    model_format: openproxy_types::TargetFormat,
) -> openproxy_types::TargetFormat {
    openproxy_adapters::adapters::resolve_target_format(adapter.format(), model_format)
}

pub(crate) mod dispatch;
pub(crate) mod formatting;
pub(crate) mod prefix;

pub use dispatch::{CustomAdapterStage, DispatchStage};
pub use formatting::FormattingStage;

#[cfg(test)]
#[path = "stream_policy_tests.rs"]
mod stream_policy_tests;

#[cfg(test)]
#[path = "pin_tests.rs"]
mod pin_tests;

#[cfg(test)]
#[path = "target_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "latency_budget_stage_tests.rs"]
mod latency_budget_tests;
