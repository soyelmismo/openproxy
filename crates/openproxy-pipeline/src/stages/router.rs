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
        let combo = match ctx.pipeline.load_combo(&ctx.req).await {
            Ok(c) => c,
            Err(e) => return Err(e),
        };

        ctx.combo = Some(combo.clone());

        let attempt = ctx.attempt;
        let targets = match ctx
            .pipeline
            .resolve_targets(&combo, ctx.req.targets_override.as_deref())
            .await
        {
            Ok(t) => t,
            Err(e) => return Err(e),
        };

        let flat_targets = match ctx
            .pipeline
            .flatten_targets(&combo.id, targets.clone())
            .await
        {
            Ok(t) => t,
            Err(e) => return Err(e),
        };

        let pre_cb_snapshot: Vec<ComboTarget> = flat_targets.clone();
        let mut eligible: Vec<ComboTarget> = flat_targets
            .into_iter()
            .filter(|t| match t.account_id {
                Some(aid) => {
                    let key = if t.rate_limit_scope
                        == openproxy_types::providers::RateLimitScope::Model
                    {
                        crate::circuit_breaker::CircuitBreakerKey::Model(
                            aid,
                            t.model_row_id.expect("flattened"),
                        )
                    } else {
                        crate::circuit_breaker::CircuitBreakerKey::Account(aid)
                    };
                    ctx.pipeline.circuit_breaker.is_healthy(key) == Health::Healthy
                }
                None => true,
            })
            .collect();

        if eligible.is_empty() && !pre_cb_snapshot.is_empty() {
            tracing::warn!(
                combo_id = combo.id.0,
                parked = pre_cb_snapshot.len(),
                "all targets' accounts unhealthy in circuit_breaker; falling through to pre-CB dispatch"
            );
            eligible = pre_cb_snapshot.clone();
        }

        if eligible.is_empty() {
            if attempt == 1 {
                let repopulated = match ctx.pipeline.auto_populate_if_empty(&combo).await {
                    Ok(n) => n,
                    Err(e) => return Err(e),
                };
                if repopulated > 0 {
                    let targets = match ctx
                        .pipeline
                        .resolve_targets(&combo, ctx.req.targets_override.as_deref())
                        .await
                    {
                        Ok(t) => t,
                        Err(e) => return Err(e),
                    };
                    let flat_targets = match ctx.pipeline.flatten_targets(&combo.id, targets).await
                    {
                        Ok(t) => t,
                        Err(e) => return Err(e),
                    };
                    let re_eligible: Vec<ComboTarget> = flat_targets
                        .into_iter()
                        .filter(|t| match t.account_id {
                            Some(aid) => {
                                let key = if t.rate_limit_scope
                                    == openproxy_types::providers::RateLimitScope::Model
                                {
                                    crate::circuit_breaker::CircuitBreakerKey::Model(
                                        aid,
                                        t.model_row_id.expect("flattened"),
                                    )
                                } else {
                                    crate::circuit_breaker::CircuitBreakerKey::Account(aid)
                                };
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
