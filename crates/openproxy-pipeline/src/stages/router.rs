use crate::PipelineResult;
use crate::circuit_breaker::Health;
use crate::context::PipelineContext;
use crate::stage::PipelineStage;
use openproxy_types::combos::ComboTarget;
use openproxy_types::error::CoreError;

#[derive(Clone, Copy)]
pub struct RouterStage;

impl PipelineStage for RouterStage {
    async fn execute(
        &self,
        ctx: &mut PipelineContext,
        next: crate::stage::PipelineNext<'_>,
    ) -> Result<PipelineResult, CoreError> {
        let combo = ctx.pipeline.load_combo(&ctx.req).await?;

        ctx.combo = Some(combo.clone());

        let attempt = ctx.attempt;
        let targets = ctx
            .pipeline
            .resolve_targets(&combo, ctx.req.targets_override.as_deref())
            .await?;

        let flat_targets = ctx.pipeline.flatten_targets(&combo.id, targets).await?;

        let (mut eligible, parked): (Vec<ComboTarget>, Vec<ComboTarget>) =
            flat_targets.into_iter().partition(|t| match t.account_id {
                Some(aid) => {
                    let key = crate::circuit_breaker::CircuitBreakerKey::from_target(
                        aid,
                        t.rate_limit_scope,
                        t.model_row_id,
                    );
                    ctx.pipeline.circuit_breaker.is_healthy(key) == Health::Healthy
                }
                None => true,
            });

        if eligible.is_empty() && !parked.is_empty() {
            tracing::warn!(
                combo_id = combo.id.0,
                parked = parked.len(),
                "all targets' accounts unhealthy in circuit_breaker; falling through to pre-CB dispatch"
            );
            eligible = parked;
        }

        if eligible.is_empty() {
            if attempt == 1 {
                let repopulated = ctx.pipeline.auto_populate_if_empty(&combo).await?;
                if repopulated > 0 {
                    let targets = ctx
                        .pipeline
                        .resolve_targets(&combo, ctx.req.targets_override.as_deref())
                        .await?;
                    let flat_targets = ctx.pipeline.flatten_targets(&combo.id, targets).await?;
                    let re_eligible: Vec<ComboTarget> = flat_targets
                        .into_iter()
                        .filter(|t| match t.account_id {
                            Some(aid) => {
                                let key = crate::circuit_breaker::CircuitBreakerKey::from_target(
                                    aid,
                                    t.rate_limit_scope,
                                    t.model_row_id,
                                );
                                ctx.pipeline.circuit_breaker.is_healthy(key) == Health::Healthy
                            }
                            None => true,
                        })
                        .collect();
                    if !re_eligible.is_empty() {
                        eligible = re_eligible;
                    }
                }
            }
            if eligible.is_empty() {
                return Err(CoreError::NoHealthyTargets(combo.id.0));
            }
        }

        let resolved = ctx.pipeline.resolve_combo_targets_full(eligible).await;

        if resolved.is_empty() {
            return Err(CoreError::NoHealthyTargets(combo.id.0));
        }

        ctx.targets = resolved;

        next.execute(ctx).await
    }
}
