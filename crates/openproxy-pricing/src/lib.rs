//! Per-model pricing in USD per 1M tokens.
//!
//! Re-exports pricing types and lookup functions from `openproxy_db::pricing`.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub(crate) use openproxy_types::error;
pub(crate) use openproxy_types::ids;

pub mod cost;
pub mod quota;

pub use self as pricing;
pub use openproxy_db::pricing::*;
