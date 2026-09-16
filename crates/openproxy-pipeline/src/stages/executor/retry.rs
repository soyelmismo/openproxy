use super::TargetStepResult;
use super::execute_single_target;
use super::helpers::{
    check_client_cancellation, finalize_target_result, is_max_request_timeout,
    matches_proxy_rotation_errors, resolve_target_proxy_mode,
};
use super::race::try_trigger_incremental_race_fallback;
use crate::PipelineResult;
use crate::context::PipelineContext;
use crate::retry::RetryPolicy;
use openproxy_types::error::CoreError;

pub(super) struct TargetRetryState {
    pub(super) policy: RetryPolicy,
    pub(super) target_local_retry_count: u8,
    pub(super) can_incremental_race: bool,
    pub(super) proxy_rotation_errors: String,
    pub(super) consecutive_failures: u8,
    pub(super) incremental_batch_size: usize,
    pub(super) race_size: usize,
    pub(super) total_targets: usize,
}

impl TargetRetryState {
    pub(super) async fn new(
        ctx: &PipelineContext,
        target: &crate::context::ResolvedTarget,
        race_size: usize,
        total_targets: usize,
    ) -> Self {
        let policy = RetryPolicy::from_config(&ctx.pipeline.config.retries);
        let (can_incremental_race, proxy_rotation_errors) =
            resolve_target_proxy_mode(ctx, target).await;
        Self {
            policy,
            target_local_retry_count: 1,
            can_incremental_race,
            proxy_rotation_errors,
            consecutive_failures: 0,
            incremental_batch_size: 2,
            race_size,
            total_targets,
        }
    }

    pub(super) fn track_failure(&mut self, err: &CoreError) {
        if matches_proxy_rotation_errors(err, &self.proxy_rotation_errors) {
            self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        } else {
            self.consecutive_failures = 0;
        }
    }
}

pub(super) enum RetryStep {
    Done(PipelineResult),
    Next(PipelineResult),
    Abort,
    ClientDisconnected(PipelineResult),
}

pub(super) fn should_retry_target(
    err: &CoreError,
    retry_count: u8,
    policy: &RetryPolicy,
    idle_chunk_retryable: bool,
) -> bool {
    if !RetryPolicy::is_retryable(err, idle_chunk_retryable) {
        return false;
    }
    let max_attempts = if err.is_proxy_rotated() {
        policy.max_attempts.max(150)
    } else {
        policy.max_attempts
    };
    retry_count < max_attempts
}

fn resolve_base_retry_delay(
    policy: &RetryPolicy,
    retry_count: u8,
    is_proxy_rotated: bool,
) -> Option<std::time::Duration> {
    policy
        .delay_after_attempt(retry_count)
        .or_else(|| is_proxy_rotated.then_some(std::time::Duration::ZERO))
}

fn calculate_upstream_delay(
    err: &CoreError,
    base_delay: std::time::Duration,
) -> std::time::Duration {
    match err {
        CoreError::RateLimited {
            is_proxy_rotated: true,
            ..
        }
        | CoreError::UpstreamError {
            status: 429,
            is_proxy_rotated: true,
            ..
        } => std::time::Duration::ZERO,
        CoreError::RateLimited {
            retry_after_ms,
            is_proxy_rotated: false,
            ..
        } => std::time::Duration::from_millis(*retry_after_ms).max(base_delay),
        _ => base_delay,
    }
}

fn compute_retry_delay(
    policy: &RetryPolicy,
    retry_count: u8,
    err: &CoreError,
) -> Option<std::time::Duration> {
    let base_delay = resolve_base_retry_delay(policy, retry_count, err.is_proxy_rotated())?;
    Some(calculate_upstream_delay(err, base_delay))
}

fn check_retry_delay(
    policy: &RetryPolicy,
    target_local_retry_count: u8,
    err: &CoreError,
    combo_id: i64,
    target: &crate::context::ResolvedTarget,
) -> Option<std::time::Duration> {
    let delay = compute_retry_delay(policy, target_local_retry_count, err)?;
    if delay.as_secs() > 15 {
        tracing::warn!(
            combo_id,
            target_id = target.target.id.0,
            provider = %target.target.provider_id,
            delay_secs = delay.as_secs(),
            "delay too long; aborting retry for this target"
        );
        return None;
    }
    Some(delay)
}

async fn compute_and_wait_retry_delay(
    ctx: &PipelineContext,
    combo: &openproxy_types::Combo,
    target: &crate::context::ResolvedTarget,
    state: &mut TargetRetryState,
    err: &CoreError,
    overall_attempt: &mut u8,
) -> Option<PipelineResult> {
    let delay = check_retry_delay(
        &state.policy,
        state.target_local_retry_count,
        err,
        combo.id.0,
        target,
    )?;

    tracing::debug!(
        combo_id = combo.id.0,
        target_id = target.target.id.0,
        provider = %target.target.provider_id,
        target_local_retry_count = state.target_local_retry_count,
        next_attempt = state.target_local_retry_count + 1,
        overall_attempt = *overall_attempt,
        delay_ms = delay.as_millis() as u64,
        error = %err,
        is_proxy_rotated = err.is_proxy_rotated(),
        "target failed retryably; retrying same target"
    );
    tokio::time::sleep(delay).await;
    state.target_local_retry_count = state.target_local_retry_count.saturating_add(1);
    *overall_attempt = overall_attempt.saturating_add(1);

    Some(
        execute_single_target(
            ctx,
            combo,
            target,
            *overall_attempt,
            state.race_size,
            state.total_targets,
        )
        .await,
    )
}

async fn perform_retry_iteration(
    ctx: &PipelineContext,
    combo: &openproxy_types::Combo,
    target: &crate::context::ResolvedTarget,
    state: &mut TargetRetryState,
    err: &CoreError,
    overall_attempt: &mut u8,
) -> RetryStep {
    if state.total_targets > 1 && is_max_request_timeout(err) {
        tracing::info!(
            combo_id = combo.id.0,
            target_id = target.target.id.0,
            provider = %target.target.provider_id,
            "target reached maximum request timeout; rotating to next target in combo"
        );
        return RetryStep::Abort;
    }

    if !should_retry_target(
        err,
        state.target_local_retry_count,
        &state.policy,
        ctx.pipeline.config.idle_chunk_retryable,
    ) {
        return RetryStep::Abort;
    }

    if combo.preventive_rate_limit {
        let key =
            crate::predictive_rate_limit::PredictiveRateLimiter::compute_target_key(&target.target);
        let fingerprint = crate::predictive_rate_limit::compute_error_fingerprint(err);
        let now_ms = crate::predictive_rate_limit::PredictiveRateLimiter::now_ms();
        if !ctx.pipeline.predictive_limiter.should_retry_key(
            key,
            fingerprint,
            state.target_local_retry_count,
            now_ms,
        ) {
            tracing::info!(
                combo_id = combo.id.0,
                target_id = target.target.id.0,
                provider = %target.target.provider_id,
                target_local_retry_count = state.target_local_retry_count,
                fingerprint,
                "preventive health: fast-fail triggered on repeating/degraded target; advancing in chain"
            );
            return RetryStep::Abort;
        }
    }

    state.track_failure(err);

    if let Some(disc) = check_client_cancellation(ctx, combo.id.0, target) {
        return RetryStep::ClientDisconnected(disc);
    }

    if let Some(step) =
        try_trigger_incremental_race_fallback(ctx, combo, target, state, overall_attempt).await
    {
        return step;
    }

    let Some(next_res) =
        compute_and_wait_retry_delay(ctx, combo, target, state, err, overall_attempt).await
    else {
        return RetryStep::Abort;
    };

    RetryStep::Next(next_res)
}

pub(super) async fn run_target_with_retries(
    ctx: &mut PipelineContext,
    combo: &openproxy_types::Combo,
    target: &crate::context::ResolvedTarget,
    race_size: usize,
    total_targets: usize,
    overall_attempt: &mut u8,
) -> TargetStepResult {
    let mut state = TargetRetryState::new(ctx, target, race_size, total_targets).await;
    let mut result = execute_single_target(
        ctx,
        combo,
        target,
        *overall_attempt,
        race_size,
        total_targets,
    )
    .await;

    while let Some(e) = &result.error {
        match perform_retry_iteration(ctx, combo, target, &mut state, e, overall_attempt).await {
            RetryStep::Done(r) => {
                result = r;
                break;
            }
            RetryStep::Next(r) => {
                result = r;
            }
            RetryStep::Abort => break,
            RetryStep::ClientDisconnected(disc) => {
                return TargetStepResult::ClientDisconnected(disc);
            }
        }
    }

    finalize_target_result(
        ctx,
        combo,
        target,
        result,
        state.target_local_retry_count,
        *overall_attempt,
        total_targets,
    )
}
