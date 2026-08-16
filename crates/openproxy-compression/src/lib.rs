#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod compression;
pub mod content_router;
pub mod diff_compressor;
pub mod lite;
pub mod log_compressor;
pub mod rtk;
pub mod smart_crusher;
pub mod stats;
pub mod token_estimate;
pub mod visitor;

pub use compression::{
    CompressionMode, TextCompressor, apply_compression, measure_compression, would_compress,
};
pub use stats::CompressionStats;
pub use token_estimate::{
    estimate_completion_tokens, estimate_prompt_tokens, message_content_to_text,
};
pub use visitor::mutate_message_text;

