use crate::PipelineResult;
use crate::context::PipelineContext;
use crate::retry::RetryPolicy;
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
) -> Result<(openproxy_types::Combo, Vec<crate::context::ResolvedTarget>, usize), CoreError> {
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
    for idx in 0..to_run.len() {
        match execute_single_target_step(ctx, combo, to_run, idx, race_size, &mut overall_attempt).await {
            TargetLoopOutcome::Finish(res) => return Ok(res),
            TargetLoopOutcome::Continue(res) => last_result = res,
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
    let remaining = &to_run[idx + 1..];
    if should_skip_preventive_target(&ctx.pipeline, combo, target, remaining, now_ms) {
        return TargetLoopOutcome::Skip;
    }

    if combo.preventive_rate_limit {
        let _ = ctx.pipeline.predictive_limiter.acquire_target(combo.id, target.target.id, now_ms);
    }

    let step = run_target_with_retries(ctx, combo, target, race_size, to_run.len(), overall_attempt).await;
    *overall_attempt = overall_attempt.saturating_add(1);
    match step {
        TargetStepResult::Success(r) | TargetStepResult::ClientDisconnected(r) => TargetLoopOutcome::Finish(r),
        TargetStepResult::Failed(r) => TargetLoopOutcome::Continue(Some(r)),
    }
}

enum TargetStepResult {
    Success(PipelineResult),
    ClientDisconnected(PipelineResult),
    Failed(PipelineResult),
}

async fn try_initial_race(
    ctx: &mut PipelineContext,
    combo: &openproxy_types::Combo,
    to_run: &[crate::context::ResolvedTarget],
    _race_size: usize,
) -> Option<PipelineResult> {
    if combo.race_size <= 1 || to_run.len() < 2 {
        return None;
    }
    let race_n = (combo.race_size as usize)
        .min(to_run.len())
        .min(ctx.pipeline.config.racing.max_race_size as usize);
    let race_result = crate::racing::run_race(
        &ctx.pipeline,
        ctx.req.clone(),
        combo,
        to_run.to_vec(),
        race_n as u8,
    )
    .await;

    if race_result.error.is_none() {
        ctx.pipeline
            .tracker
            .mark_client_response(race_result.usage_tuple.clone());
        return Some(race_result);
    }

    tracing::warn!(
        combo_id = combo.id.0,
        race_size = race_n,
        total_targets = to_run.len(),
        last_error = ?race_result.error,
        "race exhausted all lanes; falling through to sequential targets"
    );
    Some(race_result)
}

fn check_client_cancellation(
    ctx: &PipelineContext,
    combo_id: i64,
    target: &crate::context::ResolvedTarget,
) -> Option<PipelineResult> {
    let mut rx = tokio::sync::watch::Receiver::clone(&ctx.req.client_disconnected);
    let reason = crate::Pipeline::is_client_disconnected(&mut rx)?;
    tracing::warn!(
        combo_id,
        target_id = target.target.id.0,
        provider = %target.target.provider_id,
        attempt = ctx.attempt,
        "client cancelled between targets; aborting pipeline"
    );
    Some(crate::Pipeline::client_disconnected_result(
        ctx.attempt,
        reason,
    ))
}

fn resolve_target_proxy_mode(
    ctx: &PipelineContext,
    target: &crate::context::ResolvedTarget,
) -> (bool, String) {
    let conn = ctx.pipeline.conn.lock();
    let Ok(Some(prov)) = openproxy_db::providers::get(&conn, &target.target.provider_id) else {
        return (false, String::new());
    };
    let is_incremental_mode = matches!(prov.proxy_rotation_mode.as_str(), "incremental_race" | "incremental");
    let can_incremental_race = prov.use_proxies && is_incremental_mode && target.target.account_id.is_none();
    (can_incremental_race, prov.proxy_rotation_errors)
}

fn should_retry_target(
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

fn calculate_upstream_delay(err: &CoreError, base_delay: std::time::Duration) -> std::time::Duration {
    match err {
        CoreError::RateLimited { is_proxy_rotated: true, .. }
        | CoreError::UpstreamError { status: 429, is_proxy_rotated: true, .. } => {
            std::time::Duration::ZERO
        }
        CoreError::RateLimited { retry_after_ms, is_proxy_rotated: false, .. } => {
            std::time::Duration::from_millis(*retry_after_ms).max(base_delay)
        }
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

async fn try_incremental_proxy_race(
    ctx: &PipelineContext,
    combo: &openproxy_types::Combo,
    target: &crate::context::ResolvedTarget,
    incremental_batch_size: &mut usize,
    consecutive_failures: u8,
    overall_attempt: &mut u8,
    target_local_retry_count: &mut u8,
) -> Option<PipelineResult> {
    let candidate_proxies = {
        let conn = ctx.pipeline.conn.lock();
        openproxy_db::free_proxies::get_candidate_proxies_for_provider(
            &conn,
            &target.target.provider_id,
            *incremental_batch_size,
        )
        .unwrap_or_default()
    };

    if candidate_proxies.len() < 2 {
        return None;
    }

    tracing::info!(
        provider = %target.target.provider_id,
        batch_size = candidate_proxies.len(),
        consecutive_failures,
        "triggering incremental proxy race"
    );
    *overall_attempt = overall_attempt.saturating_add(1);
    *target_local_retry_count = target_local_retry_count.saturating_add(candidate_proxies.len() as u8);

    let race_res = crate::proxy_race::run_proxy_race(
        &ctx.pipeline,
        ctx.req.clone(),
        combo,
        target,
        candidate_proxies,
        *overall_attempt,
    )
    .await;

    if race_res.error.is_some() {
        *incremental_batch_size = (*incremental_batch_size * 2).min(16);
    }
    Some(race_res)
}

struct TargetRetryState {
    policy: RetryPolicy,
    target_local_retry_count: u8,
    can_incremental_race: bool,
    proxy_rotation_errors: String,
    consecutive_failures: u8,
    incremental_batch_size: usize,
    race_size: usize,
    total_targets: usize,
}

impl TargetRetryState {
    fn new(ctx: &PipelineContext, target: &crate::context::ResolvedTarget, race_size: usize, total_targets: usize) -> Self {
        let policy = RetryPolicy::from_config(&ctx.pipeline.config.retries);
        let (can_incremental_race, proxy_rotation_errors) = resolve_target_proxy_mode(ctx, target);
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

    fn track_failure(&mut self, err: &CoreError) {
        if matches_proxy_rotation_errors(err, &self.proxy_rotation_errors) {
            self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        } else {
            self.consecutive_failures = 0;
        }
    }
}

enum RetryStep {
    Done(PipelineResult),
    Next(PipelineResult),
    Abort,
    ClientDisconnected(PipelineResult),
}

async fn execute_single_target(
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

async fn try_trigger_incremental_race_fallback(
    ctx: &PipelineContext,
    combo: &openproxy_types::Combo,
    target: &crate::context::ResolvedTarget,
    state: &mut TargetRetryState,
    overall_attempt: &mut u8,
) -> Option<RetryStep> {
    if !state.can_incremental_race || state.consecutive_failures < 3 {
        return None;
    }
    let race_res = try_incremental_proxy_race(
        ctx,
        combo,
        target,
        &mut state.incremental_batch_size,
        state.consecutive_failures,
        overall_attempt,
        &mut state.target_local_retry_count,
    )
    .await?;

    Some(if race_res.error.is_none() {
        RetryStep::Done(race_res)
    } else {
        RetryStep::Next(race_res)
    })
}

async fn compute_and_wait_retry_delay(
    ctx: &PipelineContext,
    combo: &openproxy_types::Combo,
    target: &crate::context::ResolvedTarget,
    state: &mut TargetRetryState,
    err: &CoreError,
    overall_attempt: &mut u8,
) -> Option<PipelineResult> {
    let delay = check_retry_delay(&state.policy, state.target_local_retry_count, err, combo.id.0, target)?;

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

    Some(execute_single_target(ctx, combo, target, *overall_attempt, state.race_size, state.total_targets).await)
}

async fn perform_retry_iteration(
    ctx: &PipelineContext,
    combo: &openproxy_types::Combo,
    target: &crate::context::ResolvedTarget,
    state: &mut TargetRetryState,
    err: &CoreError,
    overall_attempt: &mut u8,
) -> RetryStep {
    if !should_retry_target(err, state.target_local_retry_count, &state.policy, ctx.pipeline.config.idle_chunk_retryable) {
        return RetryStep::Abort;
    }
    state.track_failure(err);

    if let Some(disc) = check_client_cancellation(ctx, combo.id.0, target) {
        return RetryStep::ClientDisconnected(disc);
    }

    if let Some(step) = try_trigger_incremental_race_fallback(ctx, combo, target, state, overall_attempt).await {
        return step;
    }

    let Some(next_res) = compute_and_wait_retry_delay(
        ctx,
        combo,
        target,
        state,
        err,
        overall_attempt,
    )
    .await else {
        return RetryStep::Abort;
    };

    RetryStep::Next(next_res)
}

fn finalize_target_result(
    ctx: &mut PipelineContext,
    combo: &openproxy_types::Combo,
    target: &crate::context::ResolvedTarget,
    result: PipelineResult,
    target_local_retry_count: u8,
    overall_attempt: u8,
    total_targets: usize,
) -> TargetStepResult {
    if result.error.is_none() {
        ctx.pipeline
            .tracker
            .mark_client_response(result.usage_tuple.clone());
        TargetStepResult::Success(result)
    } else {
        log_target_failure(ctx, combo, target, &result, target_local_retry_count, overall_attempt, total_targets);
        TargetStepResult::Failed(result)
    }
}

async fn run_target_with_retries(
    ctx: &mut PipelineContext,
    combo: &openproxy_types::Combo,
    target: &crate::context::ResolvedTarget,
    race_size: usize,
    total_targets: usize,
    overall_attempt: &mut u8,
) -> TargetStepResult {
    let mut state = TargetRetryState::new(ctx, target, race_size, total_targets);
    let mut result = execute_single_target(ctx, combo, target, *overall_attempt, race_size, total_targets).await;

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
            RetryStep::ClientDisconnected(disc) => return TargetStepResult::ClientDisconnected(disc),
        }
    }

    finalize_target_result(ctx, combo, target, result, state.target_local_retry_count, *overall_attempt, total_targets)
}

fn log_target_failure(
    ctx: &mut PipelineContext,
    combo: &openproxy_types::Combo,
    target: &crate::context::ResolvedTarget,
    result: &PipelineResult,
    target_local_retry_count: u8,
    overall_attempt: u8,
    total_targets: usize,
) {
    let Some(e) = result.error.as_ref() else { return; };
    let is_rate_limit = matches!(e, CoreError::RateLimited { .. } | CoreError::UpstreamError { status: 429, .. });
    let retryable = RetryPolicy::is_retryable(e, ctx.pipeline.config.idle_chunk_retryable);
    if is_rate_limit {
        tracing::warn!(
            combo_id = combo.id.0,
            target_id = target.target.id.0,
            provider = %target.target.provider_id,
            model_row_id = ?target.target.model_row_id,
            attempts_on_target = target_local_retry_count,
            overall_attempt,
            retryable,
            error = %e,
            is_proxy_rotated = e.is_proxy_rotated(),
            remaining_targets = total_targets,
            "target rate-limited; trying next target in combo"
        );
    } else {
        tracing::debug!(
            combo_id = combo.id.0,
            target_id = target.target.id.0,
            provider = %target.target.provider_id,
            strategy = ?combo.strategy,
            retryable,
            error = %e,
            is_proxy_rotated = e.is_proxy_rotated(),
            "target failed; trying next target"
        );
    }
    ctx.combo_walk_log.push(format!(
        "  target_id={} provider={} attempts={} error={}",
        target.target.id.0, target.target.provider_id, target_local_retry_count, e
    ));
}

fn finalize_exhausted_combo(
    ctx: &PipelineContext,
    combo_id: i64,
    total_targets: usize,
    last_result: Option<PipelineResult>,
) -> Result<PipelineResult, CoreError> {
    if let Some(r) = last_result
        && r.error.is_some()
    {
        tracing::warn!(
            combo_id,
            total_targets,
            targets_tried = ctx.combo_walk_log.len(),
            last_error = ?r.error,
            "combo exhausted: all {} target(s) failed, returning last error to client.\nCombo walk summary:\n{}",
            ctx.combo_walk_log.len(),
            ctx.combo_walk_log.join("\n")
        );
        ctx.pipeline
            .tracker
            .mark_client_response(r.usage_tuple.clone());
        return Ok(r);
    }

    Err(CoreError::NoHealthyTargets(combo_id))
}

fn error_matches_part(err: &CoreError, part: &str) -> bool {
    match err {
        CoreError::RateLimited { .. } => matches!(part, "429" | "rate_limited"),
        CoreError::UpstreamError { status, .. } => part.parse::<u16>().is_ok_and(|s| s == *status),
        CoreError::UpstreamConnection(_) | CoreError::UpstreamTimeout { .. } => {
            matches!(part, "connect_error" | "timeout")
        }
        _ => false,
    }
}

pub(crate) fn matches_proxy_rotation_errors(err: &CoreError, rotation_errors_csv: &str) -> bool {
    err.is_proxy_rotated()
        || rotation_errors_csv
            .split(',')
            .map(str::trim)
            .any(|part| error_matches_part(err, part))
}

fn should_skip_preventive_target(
    pipeline: &crate::Pipeline,
    combo: &openproxy_types::Combo,
    target: &crate::context::ResolvedTarget,
    remaining_targets: &[crate::context::ResolvedTarget],
    now_ms: u64,
) -> bool {
    if !combo.preventive_rate_limit {
        return false;
    }
    let readiness = pipeline.predictive_limiter.evaluate_target(combo.id, target.target.id, now_ms);
    let crate::predictive_rate_limit::TargetReadiness::Saturated { learned_burst, window_count, reset_in_ms } = readiness else {
        return false;
    };

    let has_healthy_alternative = remaining_targets.iter().any(|alt| {
        !matches!(
            pipeline.predictive_limiter.evaluate_target(combo.id, alt.target.id, now_ms),
            crate::predictive_rate_limit::TargetReadiness::Saturated { .. }
        )
    });

    if has_healthy_alternative {
        tracing::info!(
            combo_id = combo.id.0,
            target_id = target.target.id.0,
            provider = %target.target.provider_id,
            learned_burst,
            window_count,
            reset_in_ms,
            "preventive_rate_limit: predicted rate limit 429; skipping target and advancing in chain"
        );
        return true;
    }

    false
}
