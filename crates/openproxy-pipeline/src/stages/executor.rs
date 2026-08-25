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
        let Some(combo) = &ctx.combo else {
            return Err(CoreError::Validation("No combo resolved".to_string()));
        };
        let to_run = std::mem::take(&mut ctx.targets);

        if to_run.is_empty() {
            return Err(CoreError::NoHealthyTargets(combo.id.0));
        }

        let race_size: usize = (combo.race_size as usize)
            .min(to_run.len())
            .min(ctx.pipeline.config.racing.max_race_size as usize);

        let mut last_result: Option<PipelineResult> = None;

        if combo.race_size > 1 && to_run.len() >= 2 {
            let race_n = (combo.race_size as usize)
                .min(to_run.len())
                .min(ctx.pipeline.config.racing.max_race_size as usize);
            let race_result = crate::racing::run_race(
                &ctx.pipeline,
                ctx.req.clone(),
                combo,
                to_run.clone(),
                race_n as u8,
            )
            .await;

            if race_result.error.is_none() {
                ctx.pipeline
                    .tracker
                    .mark_client_response(race_result.usage_tuple.clone());
                return Ok(race_result);
            }

            tracing::warn!(
                combo_id = combo.id.0,
                race_size = race_n,
                total_targets = to_run.len(),
                last_error = ?race_result.error,
                "race exhausted all lanes; falling through to sequential targets"
            );
            last_result = Some(race_result);
        }

        let mut overall_attempt: u8 = 1;

        for target in &to_run {
            let client_disconnected = {
                let mut rx = tokio::sync::watch::Receiver::clone(&ctx.req.client_disconnected);
                crate::Pipeline::is_client_disconnected(&mut rx)
            };
            if let Some(reason) = client_disconnected {
                tracing::warn!(
                    combo_id = combo.id.0,
                    target_id = target.target.id.0,
                    provider = %target.target.provider_id,
                    attempt = ctx.attempt,
                    "client cancelled between targets; aborting pipeline"
                );
                return Ok(crate::Pipeline::client_disconnected_result(ctx.attempt, reason));
            }

            let policy = RetryPolicy::from_config(&ctx.pipeline.config.retries);
            let mut target_local_retry_count: u8 = 1;
            let cancel_tok = openproxy_adapters::upstream::CancellationToken::new();

            let (is_use_proxies, is_incremental_mode, proxy_rotation_errors) = {
                let conn = ctx.pipeline.conn.lock();
                if let Ok(Some(prov)) =
                    openproxy_db::providers::get(&conn, &target.target.provider_id)
                {
                    (
                        prov.use_proxies,
                        prov.proxy_rotation_mode == "incremental_race"
                            || prov.proxy_rotation_mode == "incremental",
                        prov.proxy_rotation_errors,
                    )
                } else {
                    (false, false, String::new())
                }
            };
            let can_incremental_race =
                is_use_proxies && is_incremental_mode && target.target.account_id.is_none();

            let mut consecutive_failures: u8 = 0;
            let mut incremental_batch_size: usize = 2;

            let mut result = ctx
                .pipeline
                .execute_single(crate::SingleExecutionParams {
                    req: ctx.req.clone(),
                    combo,
                    resolved_target: target,
                    attempt: overall_attempt,
                    race_size: race_size as u8,
                    total_targets: to_run.len() as u8,
                    race_cancel: &cancel_tok,
                })
                .await;

            while let Some(e) = &result.error {
                if !RetryPolicy::is_retryable(e, ctx.pipeline.config.idle_chunk_retryable) {
                    break;
                }
                if matches_proxy_rotation_errors(e, &proxy_rotation_errors) {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                } else {
                    consecutive_failures = 0;
                }
                let max_attempts = if e.is_proxy_rotated() {
                    policy.max_attempts.max(150)
                } else {
                    policy.max_attempts
                };
                if target_local_retry_count >= max_attempts {
                    break;
                }
                let client_disconnected = {
                    let mut rx = tokio::sync::watch::Receiver::clone(&ctx.req.client_disconnected);
                    crate::Pipeline::is_client_disconnected(&mut rx)
                };
                if let Some(reason) = client_disconnected {
                    tracing::warn!(
                        combo_id = combo.id.0,
                        target_id = target.target.id.0,
                        provider = %target.target.provider_id,
                        attempt = ctx.attempt,
                        "client cancelled before target dispatch"
                    );
                    return Ok(crate::Pipeline::client_disconnected_result(ctx.attempt, reason));
                }

                if can_incremental_race && consecutive_failures >= 3 {
                    let candidate_proxies = {
                        let conn = ctx.pipeline.conn.lock();
                        openproxy_db::free_proxies::get_candidate_proxies_for_provider(
                            &conn,
                            &target.target.provider_id,
                            incremental_batch_size,
                        )
                        .unwrap_or_default()
                    };

                    if candidate_proxies.len() >= 2 {
                        tracing::info!(
                            provider = %target.target.provider_id,
                            batch_size = candidate_proxies.len(),
                            consecutive_failures,
                            "triggering incremental proxy race"
                        );
                        overall_attempt = overall_attempt.saturating_add(1);
                        target_local_retry_count =
                            target_local_retry_count.saturating_add(candidate_proxies.len() as u8);

                        result = crate::proxy_race::run_proxy_race(
                            &ctx.pipeline,
                            ctx.req.clone(),
                            combo,
                            target,
                            candidate_proxies,
                            overall_attempt,
                        )
                        .await;

                        if result.error.is_none() {
                            break;
                        }

                        // Wait until all failed before attempting new proxies, then double batch size
                        incremental_batch_size = (incremental_batch_size * 2).min(16);
                        continue;
                    }
                }

                let delay = match policy.delay_after_attempt(target_local_retry_count) {
                    Some(d) => d,
                    None => {
                        if e.is_proxy_rotated() {
                            std::time::Duration::from_millis(0)
                        } else {
                            break;
                        }
                    }
                };
                let delay = if let CoreError::RateLimited {
                    retry_after_ms,
                    is_proxy_rotated,
                    ..
                } = e
                {
                    if *is_proxy_rotated {
                        std::time::Duration::from_millis(0)
                    } else {
                        let upstream = std::time::Duration::from_millis(*retry_after_ms);
                        if upstream > delay { upstream } else { delay }
                    }
                } else if let CoreError::UpstreamError {
                    status: 429,
                    is_proxy_rotated: true,
                    ..
                } = e
                {
                    std::time::Duration::from_millis(0)
                } else {
                    delay
                };

                // CAP THE DELAY
                // If the upstream delay is absurdly long (e.g. > 15 seconds) and we are not rotating proxies,
                // we should NOT sleep in a live pipeline, as the client will disconnect anyway.
                // We break the loop to fall through to the next target instead.
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
                    overall_attempt,
                    delay_ms = delay.as_millis() as u64,
                    error = %e,
                    is_proxy_rotated = e.is_proxy_rotated(),
                    "target failed retryably; retrying same target"
                );
                tokio::time::sleep(delay).await;
                target_local_retry_count = target_local_retry_count.saturating_add(1);
                overall_attempt = overall_attempt.saturating_add(1);
                let retry_cancel = openproxy_adapters::upstream::CancellationToken::new();
                result = ctx
                    .pipeline
                    .execute_single(crate::SingleExecutionParams {
                        req: ctx.req.clone(),
                        combo,
                        resolved_target: target,
                        attempt: overall_attempt,
                        race_size: race_size as u8,
                        total_targets: to_run.len() as u8,
                        race_cancel: &retry_cancel,
                    })
                    .await;
            }

            match result.error.as_ref() {
                None => {
                    ctx.pipeline
                        .tracker
                        .mark_client_response(result.usage_tuple.clone());
                    return Ok(result);
                }
                Some(e) => {
                    let is_rate_limit = matches!(e, CoreError::RateLimited { .. })
                        || (matches!(e, CoreError::UpstreamError { status, .. } if *status == 429));
                    if is_rate_limit {
                        tracing::warn!(
                            combo_id = combo.id.0,
                            target_id = target.target.id.0,
                            provider = %target.target.provider_id,
                            model_row_id = ?target.target.model_row_id,
                            attempts_on_target = target_local_retry_count,
                            overall_attempt,
                            retryable = RetryPolicy::is_retryable(e, ctx.pipeline.config.idle_chunk_retryable),
                            error = %e,
                            is_proxy_rotated = e.is_proxy_rotated(),
                            remaining_targets = to_run.len(),
                            "target rate-limited; trying next target in combo"
                        );
                    } else {
                        tracing::debug!(
                            combo_id = combo.id.0,
                            target_id = target.target.id.0,
                            provider = %target.target.provider_id,
                            strategy = ?combo.strategy,
                            retryable = RetryPolicy::is_retryable(e, ctx.pipeline.config.idle_chunk_retryable),
                            error = %e,
                            is_proxy_rotated = e.is_proxy_rotated(),
                            "target failed; trying next target"
                        );
                    }
                    ctx.combo_walk_log.push(format!(
                        "  target_id={} provider={} attempts={} error={}",
                        target.target.id.0, target.target.provider_id, target_local_retry_count, e
                    ));
                    last_result = Some(result);
                }
            }
            overall_attempt = overall_attempt.saturating_add(1);
        }

        if let Some(r) = last_result
            && r.error.is_some()
        {
            tracing::warn!(
                combo_id = combo.id.0,
                total_targets = to_run.len(),
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

        Err(CoreError::NoHealthyTargets(combo.id.0))
    }
}

pub(crate) fn matches_proxy_rotation_errors(err: &CoreError, rotation_errors_csv: &str) -> bool {
    if err.is_proxy_rotated() {
        return true;
    }
    let parts: Vec<&str> = rotation_errors_csv.split(',').map(str::trim).collect();
    match err {
        CoreError::RateLimited { .. } => parts.iter().any(|&e| e == "429" || e == "rate_limited"),
        CoreError::UpstreamError { status, .. } => {
            let sc_str = status.to_string();
            parts.iter().any(|&e| e == sc_str)
        }
        CoreError::UpstreamConnection(_) => parts
            .iter()
            .any(|&e| e == "connect_error" || e == "timeout"),
        CoreError::UpstreamTimeout { .. } => parts
            .iter()
            .any(|&e| e == "timeout" || e == "connect_error"),
        _ => false,
    }
}
