use super::TargetStepResult;
use crate::PipelineResult;
use crate::context::PipelineContext;
use crate::retry::RetryPolicy;
use openproxy_types::error::CoreError;

pub(super) fn check_client_cancellation(
    ctx: &PipelineContext,
    combo_id: i64,
    target: &crate::context::ResolvedTarget,
) -> Option<PipelineResult> {
    let mut rx = tokio::sync::watch::Receiver::clone(&ctx.req.client_disconnected);
    let reason = crate::Pipeline::is_client_disconnected(&mut rx)?;
    if reason != openproxy_types::CancelReason::ClientDisconnected {
        return None;
    }
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

pub(super) async fn resolve_target_proxy_mode(
    ctx: &PipelineContext,
    target: &crate::context::ResolvedTarget,
) -> (bool, String) {
    let conn_arc = std::sync::Arc::clone(&ctx.pipeline.conn);
    let provider_id = target.target.provider_id.clone();
    let prov_opt = tokio::task::spawn_blocking(move || {
        let conn = conn_arc.lock();
        openproxy_db::providers::get(&conn, &provider_id)
            .ok()
            .flatten()
    })
    .await
    .unwrap_or_default();

    let Some(prov) = prov_opt else {
        return (false, String::new());
    };
    let is_incremental_mode = matches!(
        prov.proxy_rotation_mode.as_ref(),
        "incremental_race" | "incremental"
    );
    let can_incremental_race =
        prov.use_proxies && is_incremental_mode && target.target.account_id.is_none();
    (can_incremental_race, prov.proxy_rotation_errors.to_string())
}

pub(super) fn finalize_target_result(
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
            .mark_client_response(result.usage_tuple);
        TargetStepResult::Success(result)
    } else {
        log_target_failure(
            ctx,
            combo,
            target,
            &result,
            target_local_retry_count,
            overall_attempt,
            total_targets,
        );
        TargetStepResult::Failed(result)
    }
}

pub(super) fn log_target_failure(
    ctx: &mut PipelineContext,
    combo: &openproxy_types::Combo,
    target: &crate::context::ResolvedTarget,
    result: &PipelineResult,
    target_local_retry_count: u8,
    overall_attempt: u8,
    total_targets: usize,
) {
    let Some(e) = result.error.as_ref() else {
        return;
    };
    let is_rate_limit = matches!(
        e,
        CoreError::RateLimited { .. } | CoreError::UpstreamError { status: 429, .. }
    );
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

pub(super) fn finalize_exhausted_combo(
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
        ctx.pipeline.tracker.mark_client_response(r.usage_tuple);
        return Ok(r);
    }

    Err(CoreError::NoHealthyTargets(combo_id))
}

pub(crate) fn is_max_request_timeout(err: &CoreError) -> bool {
    match err {
        CoreError::UpstreamTimeout { phase, .. } => phase.contains("total") || phase == "total_ms",
        CoreError::Cancelled(openproxy_types::CancelReason::WatchdogTimeout) => true,
        CoreError::UpstreamError {
            status: 504, body, ..
        } => body.contains("total") || body.contains("timeout"),
        _ => false,
    }
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

pub(super) fn should_skip_preventive_target(
    limiter: &crate::predictive_rate_limit::PredictiveRateLimiter,
    combo: &openproxy_types::Combo,
    target: &crate::context::ResolvedTarget,
    target_key: u64,
    remaining_targets: &[crate::context::ResolvedTarget],
    now_ms: u64,
) -> bool {
    if !combo.preventive_rate_limit || target.target.is_cooldown_disabled(combo) {
        return false;
    }
    let readiness = limiter.evaluate_key(target_key, now_ms);
    let crate::predictive_rate_limit::TargetReadiness::Saturated {
        learned_burst,
        window_count,
        reset_in_ms,
    } = readiness
    else {
        return false;
    };

    let has_healthy_alternative = remaining_targets.iter().any(|alt| {
        if alt.target.is_cooldown_disabled(combo) {
            return true;
        }
        let alt_key =
            crate::predictive_rate_limit::PredictiveRateLimiter::compute_target_key(&alt.target);
        !limiter.evaluate_key(alt_key, now_ms).is_saturated()
    });

    if has_healthy_alternative {
        tracing::info!(
            combo_id = combo.id.0,
            target_id = target.target.id.0,
            provider = %target.target.provider_id,
            account_id = ?target.target.account_id,
            model_row_id = ?target.target.model_row_id,
            learned_burst,
            window_count,
            reset_in_ms,
            "preventive_rate_limit: predicted rate limit 429; skipping target and advancing in chain"
        );
        return true;
    }

    false
}
