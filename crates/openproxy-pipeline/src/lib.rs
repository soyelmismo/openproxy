#![allow(clippy::too_many_arguments)]

pub mod pipeline;
pub use pipeline::{
    ErrorPhase, FailureContext, Pipeline, PipelineConfig, PipelineRequest, PipelineResult,
    SSE_DONE_BYTES, is_upstream_health_issue, parse_retry_after_ms,
};
pub mod race_sink;
pub use race_sink::{StreamSink, StreamSinkError};

// Crate modules
pub mod circuit_breaker;
pub mod context;
pub mod credentials;
pub mod execution;
pub mod formatting;
pub mod load_balancing;
pub mod oauth;
pub mod quotas;
pub mod racing;
pub mod repository;
pub mod retry;
pub use repository::SqlitePipelineRepository;
pub mod redact;
pub mod service;
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

pub mod schema_cleaner;
#[cfg(test)]
pub mod tests;

#[cfg(test)]
mod repository_tests;
