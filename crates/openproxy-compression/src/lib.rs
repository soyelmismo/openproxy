#![allow(clippy::ptr_arg)]
#![allow(clippy::needless_range_loop)]
#![allow(clippy::vec_init_then_push)]

mod compression;
pub mod content_router;
pub mod diff_compressor;
pub mod lite;
pub mod log_compressor;
pub mod rtk;
pub mod smart_crusher;
pub mod stats;

pub use compression::{CompressionMode, apply_compression, would_compress};
pub use stats::CompressionStats;
