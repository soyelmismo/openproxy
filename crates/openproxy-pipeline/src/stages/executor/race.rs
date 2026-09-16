use super::retry::{RetryStep, TargetRetryState};
use crate::PipelineResult;
use crate::context::PipelineContext;

pub(super) async fn try_initial_race(
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
            .mark_client_response(race_result.usage_tuple);
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

pub(super) async fn try_incremental_proxy_race(
    ctx: &PipelineContext,
    combo: &openproxy_types::Combo,
    target: &crate::context::ResolvedTarget,
    incremental_batch_size: &mut usize,
    consecutive_failures: u8,
    overall_attempt: &mut u8,
    target_local_retry_count: &mut u8,
) -> Option<PipelineResult> {
    let conn_arc = std::sync::Arc::clone(&ctx.pipeline.conn);
    let provider_id = target.target.provider_id.clone();
    let batch_size = *incremental_batch_size;
    let candidate_proxies = tokio::task::spawn_blocking(move || {
        let conn = conn_arc.lock();
        openproxy_db::free_proxies::get_candidate_proxies_for_provider(
            &conn,
            &provider_id,
            batch_size,
        )
        .unwrap_or_default()
    })
    .await
    .unwrap_or_default();

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
    *target_local_retry_count =
        target_local_retry_count.saturating_add(candidate_proxies.len() as u8);

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

pub(super) async fn try_trigger_incremental_race_fallback(
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
