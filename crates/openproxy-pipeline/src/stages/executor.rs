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

        let mut last_result = try_initial_race(ctx, &combo, &to_run, race_size).await;
        if let Some(res) = last_result {
            if res.error.is_none() {
                return Ok(res);
            }
            last_result = Some(res);
        }

        let mut overall_attempt: u8 = 1;
        for target in &to_run {
            if let Some(disc) = check_client_cancellation(ctx, combo.id.0, target) {
                return Ok(disc);
            }
            let step = run_target_with_retries(ctx, &combo, target, race_size, to_run.len(), &mut overall_attempt).await;
            match step {
                TargetStepResult::Success(r) | TargetStepResult::ClientDisconnected(r) => return Ok(r),
                TargetStepResult::Failed(r) => {
                    last_result = Some(r);
                }
            }
            overall_attempt = overall_attempt.saturating_add(1);
        }

        finalize_exhausted_combo(ctx, combo.id.0, to_run.len(), last_result)
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

fn compute_retry_delay(
    policy: &RetryPolicy,
    retry_count: u8,
    err: &CoreError,
) -> Option<std::time::Duration> {
    let base_delay = match policy.delay_after_attempt(retry_count) {
        Some(d) => d,
        None if err.is_proxy_rotated() => std::time::Duration::from_millis(0),
        None => return None,
    };
    match err {
        CoreError::RateLimited { is_proxy_rotated: true, .. }
        | CoreError::UpstreamError { status: 429, is_proxy_rotated: true, .. } => {
            Some(std::time::Duration::from_millis(0))
        }
        CoreError::RateLimited { retry_after_ms, is_proxy_rotated: false, .. } => {
            let upstream = std::time::Duration::from_millis(*retry_after_ms);
            Some(upstream.max(base_delay))
        }
        _ => Some(base_delay),
    }
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

async fn run_target_with_retries(
    ctx: &mut PipelineContext,
    combo: &openproxy_types::Combo,
    target: &crate::context::ResolvedTarget,
    race_size: usize,
    total_targets: usize,
    overall_attempt: &mut u8,
) -> TargetStepResult {
    let policy = RetryPolicy::from_config(&ctx.pipeline.config.retries);
    let mut target_local_retry_count: u8 = 1;
    let cancel_tok = openproxy_adapters::upstream::CancellationToken::new();

    let (can_incremental_race, proxy_rotation_errors) = resolve_target_proxy_mode(ctx, target);
    let mut consecutive_failures: u8 = 0;
    let mut incremental_batch_size: usize = 2;

    let mut result = ctx
        .pipeline
        .execute_single(crate::SingleExecutionParams {
            req: ctx.req.clone(),
            combo,
            resolved_target: target,
            attempt: *overall_attempt,
            race_size: race_size as u8,
            total_targets: total_targets as u8,
            race_cancel: &cancel_tok,
        })
        .await;

    while let Some(e) = &result.error {
        if !should_retry_target(e, target_local_retry_count, &policy, ctx.pipeline.config.idle_chunk_retryable) {
            break;
        }
        if matches_proxy_rotation_errors(e, &proxy_rotation_errors) {
            consecutive_failures = consecutive_failures.saturating_add(1);
        } else {
            consecutive_failures = 0;
        }

        if let Some(disc) = check_client_cancellation(ctx, combo.id.0, target) {
            return TargetStepResult::ClientDisconnected(disc);
        }

        if can_incremental_race && consecutive_failures >= 3
            && let Some(race_res) = try_incremental_proxy_race(
                ctx, combo, target, &mut incremental_batch_size, consecutive_failures,
                overall_attempt, &mut target_local_retry_count,
            ).await
        {
            result = race_res;
            if result.error.is_none() {
                break;
            }
            continue;
        }

        let Some(delay) = compute_retry_delay(&policy, target_local_retry_count, e) else {
            break;
        };

        if delay.as_secs() > 15 {
            tracing::warn!(
                combo_id = combo.id.0,
                target_id = target.target.id.0,
                provider = %target.target.provider_id,
                delay_secs = delay.as_secs(),
                "delay too long; aborting retry for this target"
            );
            break;
        }

        tracing::debug!(
            combo_id = combo.id.0,
            target_id = target.target.id.0,
            provider = %target.target.provider_id,
            target_local_retry_count,
            next_attempt = target_local_retry_count + 1,
            overall_attempt = *overall_attempt,
            delay_ms = delay.as_millis() as u64,
            error = %e,
            is_proxy_rotated = e.is_proxy_rotated(),
            "target failed retryably; retrying same target"
        );
        tokio::time::sleep(delay).await;
        target_local_retry_count = target_local_retry_count.saturating_add(1);
        *overall_attempt = overall_attempt.saturating_add(1);
        let retry_cancel = openproxy_adapters::upstream::CancellationToken::new();
        result = ctx
            .pipeline
            .execute_single(crate::SingleExecutionParams {
                req: ctx.req.clone(),
                combo,
                resolved_target: target,
                attempt: *overall_attempt,
                race_size: race_size as u8,
                total_targets: total_targets as u8,
                race_cancel: &retry_cancel,
            })
            .await;
    }

    if result.error.is_none() {
        ctx.pipeline
            .tracker
            .mark_client_response(result.usage_tuple.clone());
        TargetStepResult::Success(result)
    } else {
        log_target_failure(ctx, combo, target, &result, target_local_retry_count, *overall_attempt, total_targets);
        TargetStepResult::Failed(result)
    }
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
