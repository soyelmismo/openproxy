use crate::PipelineResult;
use crate::context::PipelineContext;
use crate::stage::PipelineStage;
use openproxy_types::combos::ComboTarget;
use openproxy_types::error::CoreError;

#[derive(Clone, Copy)]
pub struct RouterStage;

async fn resolve_initial_targets(
    ctx: &PipelineContext,
    combo: &openproxy_types::combos::Combo,
) -> Result<Vec<ComboTarget>, CoreError> {
    let targets = ctx
        .pipeline
        .resolve_targets(combo, ctx.req.targets_override.as_deref())
        .await?;
    ctx.pipeline.flatten_targets(&combo.id, targets).await
}

fn partition_healthy_targets(
    circuit_breaker: &crate::circuit_breaker::CircuitBreakerRegistry,
    flat_targets: Vec<ComboTarget>,
    combo_id: i64,
) -> Vec<ComboTarget> {
    let (eligible, parked): (Vec<ComboTarget>, Vec<ComboTarget>) = flat_targets
        .into_iter()
        .partition(|t| circuit_breaker.is_target_healthy(t));

    if eligible.is_empty() && !parked.is_empty() {
        tracing::warn!(
            combo_id = combo_id,
            parked = parked.len(),
            "all targets' accounts unhealthy in circuit_breaker; falling through to pre-CB dispatch"
        );
        parked
    } else {
        eligible
    }
}

async fn try_repopulate_targets(
    ctx: &PipelineContext,
    combo: &openproxy_types::combos::Combo,
) -> Result<Vec<ComboTarget>, CoreError> {
    if ctx.attempt != 1 || ctx.pipeline.auto_populate_if_empty(combo).await? == 0 {
        return Ok(Vec::new());
    }
    let targets = ctx
        .pipeline
        .resolve_targets(combo, ctx.req.targets_override.as_deref())
        .await?;
    let flat_targets = ctx.pipeline.flatten_targets(&combo.id, targets).await?;
    Ok(flat_targets
        .into_iter()
        .filter(|t| ctx.pipeline.circuit_breaker.is_target_healthy(t))
        .collect())
}

impl PipelineStage for RouterStage {
    async fn execute(
        &self,
        ctx: &mut PipelineContext,
        next: crate::stage::PipelineNext<'_>,
    ) -> Result<PipelineResult, CoreError> {
        let combo = ctx.pipeline.load_combo(&ctx.req).await?;
        ctx.combo = Some(combo.clone());

        let flat_targets = resolve_initial_targets(ctx, &combo).await?;
        let mut eligible = partition_healthy_targets(
            &ctx.pipeline.circuit_breaker,
            flat_targets,
            combo.id.0,
        );

        if eligible.is_empty() {
            eligible = try_repopulate_targets(ctx, &combo).await?;
        }

        if eligible.is_empty() {
            return Err(CoreError::NoHealthyTargets(combo.id.0));
        }

        let resolved = ctx.pipeline.resolve_combo_targets_full(eligible).await;
        if resolved.is_empty() {
            return Err(CoreError::NoHealthyTargets(combo.id.0));
        }

        ctx.targets = resolved;
        next.execute(ctx).await
    }
}
