use crate::PipelineResult;
use crate::context::PipelineContext;
use crate::stage::PipelineStage;
use openproxy_types::error::CoreError;

#[derive(Clone, Copy)]
pub struct QuotaEnforcerStage;

impl PipelineStage for QuotaEnforcerStage {
    async fn execute(
        &self,
        ctx: &mut PipelineContext,
        next: crate::stage::PipelineNext<'_>,
    ) -> Result<PipelineResult, CoreError> {
        let eligible = ctx.targets.clone();
        if eligible.is_empty() {
            return next.execute(ctx).await;
        }

        let repo = ctx.pipeline.repo();
        let master_key = ctx.pipeline.config.master_key.clone();
        let enabled = ctx.pipeline.config.quota_protection.enabled;
        let threshold = ctx.pipeline.config.quota_protection.threshold_percentage;
        let model = ctx.req.openai_request.model.clone();

        let filtered = tokio::task::spawn_blocking(move || {
            crate::quotas::apply_quota_routing(
                enabled,
                threshold,
                repo.as_ref(),
                &master_key,
                eligible,
                &model,
            )
        })
        .await
        .unwrap();
        if filtered.is_empty()
            && let Some(ref combo) = ctx.combo
        {
            return Err(CoreError::NoHealthyTargets(combo.id.0));
        }

        ctx.targets = filtered;
        next.execute(ctx).await
    }
}
