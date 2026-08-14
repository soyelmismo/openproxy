#![allow(clippy::ptr_arg)]
#![allow(clippy::vec_init_then_push)]

mod compression;
pub mod content_router;
pub mod diff_compressor;
pub mod lite;
pub mod log_compressor;
pub mod rtk;
pub mod smart_crusher;
pub mod stats;
pub mod visitor;

pub use compression::{
    CompressionMode, TextCompressor, apply_compression, measure_compression, would_compress,
};
pub use stats::CompressionStats;
pub use visitor::mutate_message_text;
