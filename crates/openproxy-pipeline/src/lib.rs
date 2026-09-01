#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod pipeline;
pub use pipeline::{
    ErrorPhase, FailureContext, PartialFailureParams, Pipeline, PipelineConfig, PipelineRequest,
    PipelineResult, SSE_DONE_BYTES, SingleExecutionParams, is_upstream_health_issue,
    parse_retry_after_ms,
};
pub mod race_sink;
pub use race_sink::{StreamSink, StreamSinkError};
pub use streaming::{StreamAction, StreamingChunkStage, StreamingStagePipeline};
pub use streaming_state::ReasoningNormalizer;

// Crate modules
pub mod circuit_breaker;
pub mod context;
pub mod credentials;
pub mod execution;
pub mod formatting;
pub mod load_balancing;
pub mod oauth;
pub mod predictive_rate_limit;
pub use predictive_rate_limit::{PredictiveRateLimiter, TargetRateState, TargetReadiness};
pub mod proxy_race;
pub mod quotas;
pub mod racing;
pub use proxy_race::run_proxy_race;
pub mod repository;
pub mod retry;
pub use repository::SqlitePipelineRepository;
pub mod redact;
pub mod sse;
pub mod sse_accumulator;
pub mod stage;
pub mod stages;
pub mod streaming;
pub mod streaming_state;
pub mod test_utils;
pub mod think_extractor;
pub mod timeouts;
pub mod translation;
pub mod upstream_dispatcher;
pub mod usage_tracker;
pub mod worker;

pub use openproxy_types::schema_cleaner;

#[cfg(test)]
mod repository_tests;
