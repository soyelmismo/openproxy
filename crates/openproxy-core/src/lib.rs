//! openproxy-core: headless LLM proxy library.
//!
//! See docs/architecture.md and docs/mvp-spec.md for the full spec.

pub mod ids;
pub mod error;
pub mod config;
pub mod capabilities;
pub mod routing;

pub mod db;
pub mod models;
pub mod adapters;
pub mod providers;
pub mod accounts;
pub mod api_keys;
pub mod combos;
pub mod usage;
pub mod cost;
pub mod analytics;
pub mod translation;
pub mod sse;
pub mod pipeline;
pub mod timeouts;
pub mod retry;
pub mod circuit_breaker;
pub mod cooldown;
pub mod race;
pub mod pricing;
pub mod secrets;
pub mod oauth;
pub mod oauth_antigravity;
pub mod oauth_kiro;
pub mod executor_kiro;
pub mod executor_antigravity;
pub mod admin;
pub mod quota;
pub mod seed;
pub mod bootstrap;

// Gate 0: hyper-based upstream client. See `upstream/mod.rs` for the
// architecture and the `upstream-hyper` feature flag in `Cargo.toml`.
// This module coexists with the existing reqwest-based call sites;
// Gate 0 does NOT migrate any call site.
pub mod upstream;

pub use config::AppConfig;
pub use error::{CoreError, ErrorContext, Result};
