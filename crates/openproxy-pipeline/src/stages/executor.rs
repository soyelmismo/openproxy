mod helpers;
mod race;
mod retry;
#[cfg(test)]
mod tests;

use helpers::{check_client_cancellation, finalize_exhausted_combo, should_skip_preventive_target};
use race::try_initial_race;
use retry::run_target_with_retries;

use crate::PipelineResult;
use crate::context::PipelineContext;
use crate::stage::PipelineStage;
use openproxy_types::error::CoreError;

#[derive(Clone, Copy)]
pub struct UpstreamExecutorStage;

impl PipelineStage for UpstreamExecutorStage {
    async fn execute(
        &self,
        ctx: &mut PipelineContext,
        _next: crate::stage::PipelineNext<'_>,
    ) -> Result<PipelineResult, CoreError> {
        let (combo, to_run, race_size) = extract_execution_plan(ctx)?;
        let last_result = match evaluate_initial_race(ctx, &combo, &to_run, race_size).await {
            InitialRaceOutcome::Success(res) => return Ok(res),
            InitialRaceOutcome::Exhausted(res) => res,
        };

        execute_sequential_targets(ctx, &combo, &to_run, race_size, last_result).await
    }
}

enum InitialRaceOutcome {
    Success(PipelineResult),
    Exhausted(Option<PipelineResult>),
}

enum TargetLoopOutcome {
    Finish(PipelineResult),
    Continue(Option<PipelineResult>),
    Skip,
}

fn extract_execution_plan(
    ctx: &mut PipelineContext,
) -> Result<
    (
        openproxy_types::Combo,
        Vec<crate::context::ResolvedTarget>,
        usize,
    ),
    CoreError,
> {
    let Some(combo) = ctx.combo.clone() else {
        return Err(CoreError::Validation("No combo resolved".to_string()));
    };
    let to_run = std::mem::take(&mut ctx.targets);
    if to_run.is_empty() {
        return Err(CoreError::NoHealthyTargets(combo.id.0));
    }

    let race_size: usize = (combo.race_size as usize)
        .min(to_run.len())
        .min(ctx.pipeline.config.racing.max_race_size as usize);

    Ok((combo, to_run, race_size))
}

async fn evaluate_initial_race(
    ctx: &mut PipelineContext,
    combo: &openproxy_types::Combo,
    to_run: &[crate::context::ResolvedTarget],
    race_size: usize,
) -> InitialRaceOutcome {
    match try_initial_race(ctx, combo, to_run, race_size).await {
        Some(res) if res.error.is_none() => InitialRaceOutcome::Success(res),
        other => InitialRaceOutcome::Exhausted(other),
    }
}

async fn execute_sequential_targets(
    ctx: &mut PipelineContext,
    combo: &openproxy_types::Combo,
    to_run: &[crate::context::ResolvedTarget],
    race_size: usize,
    mut last_result: Option<PipelineResult>,
) -> Result<PipelineResult, CoreError> {
    let mut overall_attempt: u8 = 1;
    let mut failed_targets = std::collections::HashSet::new();
    let mut failed_models = std::collections::HashSet::new();

    for (idx, target) in to_run.iter().enumerate() {
        if failed_targets.contains(&target.target.id) {
            tracing::info!(
                combo_id = combo.id.0,
                target_id = target.target.id.0,
                provider = %target.target.provider_id,
                "skipping remaining account for target that already failed in this request"
            );
            continue;
        }
        if let Some(m) = target.target.model_row_id
            && failed_models.contains(&m)
        {
            tracing::info!(
                combo_id = combo.id.0,
                target_id = target.target.id.0,
                model_row_id = m.0,
                provider = %target.target.provider_id,
                "skipping target whose model already failed in this request"
            );
            continue;
        }

        match execute_single_target_step(ctx, combo, to_run, idx, race_size, &mut overall_attempt)
            .await
        {
            TargetLoopOutcome::Finish(res) => return Ok(res),
            TargetLoopOutcome::Continue(res) => {
                if let Some(ref r) = res
                    && let Some(ref err) = r.error
                    && (crate::pipeline::is_upstream_health_issue(err) || err.is_hard_skip())
                {
                    failed_targets.insert(target.target.id);
                    if let Some(m) = target.target.model_row_id {
                        failed_models.insert(m);
                    }
                }
                last_result = res;
            }
            TargetLoopOutcome::Skip => {}
        }
    }

    finalize_exhausted_combo(ctx, combo.id.0, to_run.len(), last_result)
}

async fn execute_single_target_step(
    ctx: &mut PipelineContext,
    combo: &openproxy_types::Combo,
    to_run: &[crate::context::ResolvedTarget],
    idx: usize,
    race_size: usize,
    overall_attempt: &mut u8,
) -> TargetLoopOutcome {
    let target = &to_run[idx];
    if let Some(disc) = check_client_cancellation(ctx, combo.id.0, target) {
        return TargetLoopOutcome::Finish(disc);
    }
    let now_ms = crate::predictive_rate_limit::PredictiveRateLimiter::now_ms();
    let target_key =
        crate::predictive_rate_limit::PredictiveRateLimiter::compute_target_key(&target.target);
    let remaining = &to_run[idx + 1..];
    if should_skip_preventive_target(
        &ctx.pipeline.predictive_limiter,
        combo,
        target,
        target_key,
        remaining,
        now_ms,
    ) {
        let skip_trace_id = format!("{}:{}", ctx.req.trace_id, *overall_attempt);
        ctx.pipeline.tracker.record_predictive_skipped_row(
            &ctx.req,
            combo,
            target,
            *overall_attempt,
        );
        *overall_attempt = overall_attempt.saturating_add(1);
        openproxy_types::emit_stage_event!(
            request_id: ctx.req.request_id,
            trace_id: skip_trace_id,
            stage: "predict_skipped",
            elapsed_ms: 0,
            provider_id: target.target.provider_id.0.as_str(),
            upstream_model_id: target.model.model_id.0.as_str(),
            error: "predictive rate limit: skipped to avoid 429",
            endpoint_kind: ctx.req.endpoint_kind,
        );
        return TargetLoopOutcome::Skip;
    }

    if combo.preventive_rate_limit {
        let _ = ctx
            .pipeline
            .predictive_limiter
            .acquire_key(target_key, now_ms);
    }

    let step =
        run_target_with_retries(ctx, combo, target, race_size, to_run.len(), overall_attempt).await;
    *overall_attempt = overall_attempt.saturating_add(1);
    match step {
        TargetStepResult::Success(r) => TargetLoopOutcome::Finish(r),
        TargetStepResult::ClientDisconnected(r) => {
            let is_true_client_disconnect = r.error.as_ref().is_some_and(|e| {
                matches!(
                    e,
                    CoreError::Cancelled(openproxy_types::CancelReason::ClientDisconnected)
                )
            });
            if is_true_client_disconnect || idx + 1 >= to_run.len() {
                TargetLoopOutcome::Finish(r)
            } else {
                tracing::warn!(
                    combo_id = combo.id.0,
                    target_id = target.target.id.0,
                    provider = %target.target.provider_id,
                    "target timed out or cancelled via watchdog; rotating to next target in combo"
                );
                TargetLoopOutcome::Continue(Some(r))
            }
        }
        TargetStepResult::Failed(r) => TargetLoopOutcome::Continue(Some(r)),
    }
}

pub(super) enum TargetStepResult {
    Success(PipelineResult),
    ClientDisconnected(PipelineResult),
    Failed(PipelineResult),
}

pub(super) async fn execute_single_target(
    ctx: &PipelineContext,
    combo: &openproxy_types::Combo,
    target: &crate::context::ResolvedTarget,
    attempt: u8,
    race_size: usize,
    total_targets: usize,
) -> PipelineResult {
    let cancel_tok = openproxy_adapters::upstream::CancellationToken::new();
    ctx.pipeline
        .execute_single(crate::SingleExecutionParams {
            req: ctx.req.clone(),
            combo,
            resolved_target: target,
            attempt,
            race_size: race_size as u8,
            total_targets: total_targets as u8,
            race_cancel: &cancel_tok,
        })
        .await
}
