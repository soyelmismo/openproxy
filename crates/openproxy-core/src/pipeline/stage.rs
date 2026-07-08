use crate::error::CoreError;
use crate::pipeline::PipelineResult;
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

pub trait PipelineStage: Send + Sync {
    /// Executes this stage. A stage can either handle the request completely,
    /// or pass it to the next stage by calling `next.execute(ctx).await`.
    async fn execute(
        &self,
        ctx: &mut PipelineContext,
        next: PipelineNext<'_>,
    ) -> Result<PipelineResult, CoreError>;
}

#[derive(Clone)]
pub enum PipelineStageEnum {
    TelemetryRecorder(crate::pipeline::stages::telemetry::TelemetryRecorderStage),
    QuotaEnforcer(crate::pipeline::stages::quota::QuotaEnforcerStage),
    Router(crate::pipeline::stages::router::RouterStage),
    UpstreamExecutor(crate::pipeline::stages::executor::UpstreamExecutorStage),
    OAuthRefresh(crate::pipeline::stages::target::OAuthRefreshStage),
    CustomAdapter(crate::pipeline::stages::target::CustomAdapterStage),
    TimeoutResolution(crate::pipeline::stages::target::TimeoutResolutionStage),
    Formatting(crate::pipeline::stages::target::FormattingStage),
    Dispatch(crate::pipeline::stages::target::DispatchStage),
}

impl PipelineStage for PipelineStageEnum {
    async fn execute(
        &self,
        ctx: &mut PipelineContext,
        next: PipelineNext<'_>,
    ) -> Result<PipelineResult, CoreError> {
        match self {
            Self::TelemetryRecorder(s) => s.execute(ctx, next).await,
            Self::QuotaEnforcer(s) => s.execute(ctx, next).await,
            Self::Router(s) => s.execute(ctx, next).await,
            Self::UpstreamExecutor(s) => s.execute(ctx, next).await,
            Self::OAuthRefresh(s) => s.execute(ctx, next).await,
            Self::CustomAdapter(s) => s.execute(ctx, next).await,
            Self::TimeoutResolution(s) => s.execute(ctx, next).await,
            Self::Formatting(s) => s.execute(ctx, next).await,
            Self::Dispatch(s) => s.execute(ctx, next).await,
        }
    }
}

/// A helper to compose stages.
#[derive(Clone)]
pub struct PipelineChain {
    stages: Vec<PipelineStageEnum>,
}

impl PipelineChain {
    pub fn new(stages: Vec<PipelineStageEnum>) -> Self {
        Self { stages }
    }

    pub async fn execute(&self, mut ctx: PipelineContext) -> Result<PipelineResult, CoreError> {
        self.execute_stage(0, &mut ctx).await
    }

    /// Executes the pipeline chain over an existing mutable PipelineContext,
    /// allowing for nested inner chains.
    pub async fn execute_nested(
        &self,
        ctx: &mut PipelineContext,
    ) -> Result<PipelineResult, CoreError> {
        self.execute_stage(0, ctx).await
    }

    fn execute_stage<'a>(
        &'a self,
        index: usize,
        ctx: &'a mut PipelineContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<PipelineResult, CoreError>> + Send + 'a>,
    > {
        Box::pin(async move {
            if index < self.stages.len() {
                let stage = self.stages[index].clone();
                let next = PipelineNext {
                    chain: self,
                    next_index: index + 1,
                };
                stage.execute(ctx, next).await
            } else {
                Err(CoreError::Validation(
                    "Pipeline chain reached end without a result".to_string(),
                ))
            }
        })
    }
}
