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
    } else if !parked.is_empty() {
        // Health-as-priority: try healthy targets first, but append degraded/unhealthy targets
        // at the end of the candidate list so they act as failovers instead of being dropped.
        let mut combined = eligible;
        combined.extend(parked);
        combined
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
        let mut eligible =
            partition_healthy_targets(&ctx.pipeline.circuit_breaker, flat_targets, combo.id.0);

        if eligible.is_empty() {
            eligible = try_repopulate_targets(ctx, &combo).await?;
        }

        if eligible.is_empty() {
            return Err(CoreError::NoHealthyTargets(combo.id.0));
        }

        let mut resolved = ctx.pipeline.resolve_combo_targets_full(eligible).await;
        if resolved.is_empty() {
            return Err(CoreError::NoHealthyTargets(combo.id.0));
        }

        // Modality gate: a combo advertises the union of its leaves'
        // capabilities, so a media request must not be routed to a text-only
        // hop that would answer with a non-retryable 400.
        let required =
            crate::stages::modality::request_required_modalities(&ctx.req.openai_request);
        if required.any() {
            // A per-combo override wins over the global setting: some combos
            // exist precisely to probe whether a text-only model now accepts
            // images, and the operator must be able to opt out per combo.
            let gate_config = crate::stages::modality::effective_config(
                ctx.pipeline.config.modality_routing,
                combo.modality_gate,
            );
            let filtered = crate::stages::modality::filter_targets_for_modalities(
                resolved,
                required,
                gate_config,
            );
            if filtered.is_empty() {
                return Err(CoreError::NoHealthyTargets(combo.id.0));
            }
            resolved = filtered;
        }

        let is_decision_mode =
            combo.priority_mode == openproxy_types::combos::PriorityMode::Decision;
        let mut session_hash = None;

        if is_decision_mode {
            let should_reset =
                crate::session_affinity::SessionAffinityRegistry::should_reset(&ctx.req);
            session_hash =
                crate::session_affinity::SessionAffinityRegistry::extract_session_hash(&ctx.req);

            let mut current_pinned_id = None;
            if !should_reset && let Some(hash) = session_hash {
                let key = crate::session_affinity::SessionAffinityKey {
                    session_hash: hash,
                    combo_id: combo.id,
                };
                if let Some(pinned_id) = ctx.pipeline.session_affinity.get(key) {
                    if resolved.iter().any(|rt| rt.target.id == pinned_id) {
                        current_pinned_id = Some(pinned_id);
                        ctx.combo_walk_log
                            .push(format!("session_affinity:active={}", pinned_id.0));
                    } else {
                        ctx.pipeline.session_affinity.invalidate(&key);
                        tracing::warn!(
                            combo_id = combo.id.0,
                            pinned_target = pinned_id.0,
                            "session affinity invalidated: pinned target is unhealthy or absent"
                        );
                    }
                }
            }

            crate::stages::decision::apply_decision_routing(
                ctx,
                &combo,
                &mut resolved,
                current_pinned_id,
            )
            .await;
        }

        ctx.targets = resolved;
        let res = next.execute(ctx).await?;

        if is_decision_mode
            && res.error.is_none()
            && matches!(res.status_code, 200..=299)
            && let Some(hash) = session_hash
        {
            let successful_target_id = res
                .usage_tuple
                .as_ref()
                .map(|(_, _, tid)| *tid)
                .or_else(|| ctx.targets.first().map(|rt| rt.target.id));

            if let Some(tid) = successful_target_id {
                let key = crate::session_affinity::SessionAffinityKey {
                    session_hash: hash,
                    combo_id: combo.id,
                };
                ctx.pipeline.session_affinity.set(key, tid);
            }
        }

        Ok(res)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::circuit_breaker::{CircuitBreakerKey, CircuitBreakerRegistry};
    use openproxy_types::ProviderId;
    use openproxy_types::config::CircuitBreakerConfig;
    use openproxy_types::ids::{AccountId, ComboTargetId};

    fn make_cb() -> CircuitBreakerRegistry {
        CircuitBreakerRegistry::new(&CircuitBreakerConfig {
            failure_threshold: 1,
            unhealthy_duration_ms: 60_000,
        })
    }

    fn make_target(id: i64, account_id: Option<i64>) -> ComboTarget {
        ComboTarget {
            id: ComboTargetId(id),
            provider_id: ProviderId::from("p1"),
            account_id: account_id.map(AccountId),
            ..Default::default()
        }
    }

    #[test]
    fn test_partition_healthy_targets_with_all_healthy() {
        let cb = make_cb();
        let targets = vec![make_target(1, Some(10)), make_target(2, Some(20))];
        let result = partition_healthy_targets(&cb, targets, 1);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].id.0, 1);
        assert_eq!(result[1].id.0, 2);
    }

    #[test]
    fn test_partition_healthy_targets_with_all_unhealthy_falls_through() {
        let cb = make_cb();
        cb.record_failure(CircuitBreakerKey::Account(AccountId(10)));
        cb.record_failure(CircuitBreakerKey::Account(AccountId(20)));

        let targets = vec![make_target(1, Some(10)), make_target(2, Some(20))];
        let result = partition_healthy_targets(&cb, targets, 1);
        // Does not reject! Falls through to parked targets as failover
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].id.0, 1);
        assert_eq!(result[1].id.0, 2);
    }

    #[test]
    fn test_partition_healthy_targets_prioritizes_healthy_then_unhealthy() {
        let cb = make_cb();
        cb.record_failure(CircuitBreakerKey::Account(AccountId(10)));

        let targets = vec![
            make_target(1, Some(10)), // unhealthy
            make_target(2, Some(20)), // healthy
        ];
        let result = partition_healthy_targets(&cb, targets, 1);
        assert_eq!(result.len(), 2);
        // Healthy target 2 must come first, unhealthy target 1 must follow as failover
        assert_eq!(result[0].id.0, 2);
        assert_eq!(result[1].id.0, 1);
    }
}
