use crate::PipelineResult;
use crate::context::PipelineContext;
use openproxy_types::error::CoreError;

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
    fn execute(
        &self,
        ctx: &mut PipelineContext,
        next: PipelineNext<'_>,
    ) -> impl std::future::Future<Output = Result<PipelineResult, CoreError>> + Send;
}

pub use PipelineStage as Stage;

#[derive(Clone)]
pub enum PipelineStageEnum {
    TelemetryRecorder(crate::stages::telemetry::TelemetryRecorderStage),
    QuotaEnforcer(crate::stages::quota::QuotaEnforcerStage),
    Router(crate::stages::router::RouterStage),
    UpstreamExecutor(crate::stages::executor::UpstreamExecutorStage),
    OAuthRefresh(crate::stages::target::OAuthRefreshStage),
    CustomAdapter(crate::stages::target::CustomAdapterStage),
    TimeoutResolution(crate::stages::target::TimeoutResolutionStage),
    Formatting(crate::stages::target::FormattingStage),
    Dispatch(crate::stages::target::DispatchStage),
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
