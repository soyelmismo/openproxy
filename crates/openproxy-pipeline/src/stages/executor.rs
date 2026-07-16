use crate::PipelineResult;
use crate::context::PipelineContext;
use crate::retry::RetryPolicy;
use crate::stage::PipelineStage;
use openproxy_adapters::upstream::CancellationToken;
use openproxy_types::combos::Strategy;
use openproxy_types::error::CoreError;

#[derive(Clone, Copy)]
pub struct UpstreamExecutorStage;

impl PipelineStage for UpstreamExecutorStage {
    async fn execute(
        &self,
        ctx: &mut PipelineContext,
        _next: crate::stage::PipelineNext<'_>,
    ) -> Result<PipelineResult, CoreError> {
        let combo = match &ctx.combo {
            Some(c) => c,
            None => return Err(CoreError::Validation("No combo resolved".to_string())),
        };
        let to_run = ctx.targets.clone();

        if to_run.is_empty() {
            return Err(CoreError::NoHealthyTargets(combo.id.0));
        }

        let race_size: usize = match combo.strategy {
            Strategy::Priority => to_run.len(),
            Strategy::RoundRobin | Strategy::Shuffle => (combo.race_size as usize)
                .min(to_run.len())
                .min(ctx.pipeline.config.racing.max_race_size as usize),
        };

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

        for target in to_run.iter() {
            let client_disconnected = {
                let mut rx = ctx.req.client_disconnected.clone();
                ctx.pipeline.is_client_disconnected(&mut rx)
            };
            if client_disconnected {
                tracing::warn!(
                    combo_id = combo.id.0,
                    target_id = target.target.id.0,
                    provider = %target.target.provider_id,
                    attempt = ctx.attempt,
                    "client cancelled between targets; aborting pipeline"
                );
                return Ok(ctx.pipeline.client_disconnected_result(ctx.attempt));
            }

            let policy = RetryPolicy::from_config(&ctx.pipeline.config.retries);
            let mut target_attempt: u8 = 1;
            let mut result = ctx
                .pipeline
                .execute_single(
                    ctx.req.clone(),
                    combo,
                    target,
                    target_attempt,
                    race_size as u8,
                    &CancellationToken::new(),
                )
                .await;

            while let Some(e) = &result.error {
                if !RetryPolicy::is_retryable(e, ctx.pipeline.config.idle_chunk_retryable) {
                    break;
                }
                if target_attempt >= policy.max_attempts {
                    break;
                }
                let client_disconnected = {
                    let mut rx = ctx.req.client_disconnected.clone();
                    ctx.pipeline.is_client_disconnected(&mut rx)
                };
                if client_disconnected {
                    break;
                }
                let delay = match policy.delay_after_attempt(target_attempt) {
                    Some(d) => d,
                    None => break,
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
                    target_attempt,
                    next_attempt = target_attempt + 1,
                    delay_ms = delay.as_millis() as u64,
                    error = %e,
                    is_proxy_rotated = e.is_proxy_rotated(),
                    "target failed retryably; retrying same target"
                );
                tokio::time::sleep(delay).await;
                target_attempt = target_attempt.saturating_add(1);
                result = ctx
                    .pipeline
                    .execute_single(
                        ctx.req.clone(),
                        combo,
                        target,
                        target_attempt,
                        race_size as u8,
                        &CancellationToken::new(),
                    )
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
                            attempt = target_attempt,
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
                        target.target.id.0, target.target.provider_id, target_attempt, e
                    ));
                    last_result = Some(result);
                }
            }
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
