use async_trait::async_trait;
use crate::error::CoreError;
use crate::pipeline::PipelineResult;
use crate::pipeline::stage::PipelineStage;
use crate::pipeline::context::PipelineContext;

pub struct QuotaEnforcerStage;

#[async_trait]
impl PipelineStage for QuotaEnforcerStage {
    async fn execute(&self, ctx: &mut PipelineContext, next: crate::pipeline::stage::PipelineNext<'_>) -> Result<PipelineResult, CoreError> {
        let eligible = ctx.targets.clone();
        if eligible.is_empty() {
            return next.execute(ctx).await;
        }

        let filtered = {
            let conn = ctx.pipeline.conn.lock();
            crate::pipeline::quotas::apply_quota_routing(
                ctx.pipeline.config.quota_protection.enabled,
                ctx.pipeline.config.quota_protection.threshold_percentage,
                &conn,
                eligible,
                &ctx.req.openai_request.model,
            )
        };
        if filtered.is_empty()
            && let Some(ref combo) = ctx.combo {
                return Err(CoreError::NoHealthyTargets(combo.id.0));
            }

        ctx.targets = filtered;
        next.execute(ctx).await
    }
}
