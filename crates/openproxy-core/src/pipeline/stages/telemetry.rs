use async_trait::async_trait;
use std::sync::Arc;
use crate::error::CoreError;
use crate::pipeline::{PipelineResult, ErrorPhase};
use crate::pipeline::stage::PipelineStage;
use crate::pipeline::context::PipelineContext;

pub struct TelemetryRecorderStage;

#[async_trait]
impl PipelineStage for TelemetryRecorderStage {
    async fn execute(&self, ctx: &mut PipelineContext, next: crate::pipeline::stage::PipelineNext<'_>) -> Result<PipelineResult, CoreError> {
        let started = std::time::Instant::now();
        
        let result = next.execute(ctx).await;
        
        match result {
            Ok(res) => Ok(res),
            Err(e) => {
                // Determine phase based on the error or where we are in the context
                let phase = if ctx.combo.is_none() || ctx.targets.is_empty() {
                    ErrorPhase::Route
                } else {
                    ErrorPhase::Retry
                };

                // If routing failed with NoHealthyTargets, record it explicitly
                if let CoreError::NoHealthyTargets(_) = e
                    && let Some(ref combo) = ctx.combo {
                        ctx.pipeline.tracker.record_no_healthy_targets_row(
                            Arc::clone(&ctx.req),
                            combo,
                            started,
                        );
                    }

                Ok(ctx.pipeline.failure(e, ctx.attempt, phase))
            }
        }
    }
}
