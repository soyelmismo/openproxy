use async_trait::async_trait;
use std::sync::Arc;
use crate::pipeline::PipelineResult;
use crate::error::CoreError;
use crate::pipeline::context::PipelineContext;

pub struct PipelineNext<'a> {
    chain: &'a PipelineChain,
    next_index: usize,
}

impl<'a> PipelineNext<'a> {
    pub async fn execute(self, ctx: &mut PipelineContext) -> Result<PipelineResult, CoreError> {
        self.chain.execute_stage(self.next_index, ctx).await
    }
}

#[async_trait]
pub trait PipelineStage: Send + Sync {
    /// Executes this stage. A stage can either handle the request completely,
    /// or pass it to the next stage by calling `next.execute(ctx).await`.
    async fn execute(&self, ctx: &mut PipelineContext, next: PipelineNext<'_>) -> Result<PipelineResult, CoreError>;
}

/// A helper to compose stages.
pub struct PipelineChain {
    stages: Vec<Arc<dyn PipelineStage>>,
}

impl PipelineChain {
    pub fn new(stages: Vec<Arc<dyn PipelineStage>>) -> Self {
        Self { stages }
    }

    pub async fn execute(&self, mut ctx: PipelineContext) -> Result<PipelineResult, CoreError> {
        self.execute_stage(0, &mut ctx).await
    }

    fn execute_stage<'a>(&'a self, index: usize, ctx: &'a mut PipelineContext) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<PipelineResult, CoreError>> + Send + 'a>> {
        Box::pin(async move {
            if index < self.stages.len() {
                let stage = self.stages[index].clone();
                let next = PipelineNext { chain: self, next_index: index + 1 };
                stage.execute(ctx, next).await
            } else {
                Err(CoreError::Validation("Pipeline chain reached end without a result".to_string()))
            }
        })
    }
}
