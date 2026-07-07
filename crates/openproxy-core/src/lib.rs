#![allow(clippy::too_many_arguments, clippy::needless_range_loop, clippy::ptr_arg)]
#![allow(clippy::vec_init_then_push, clippy::items_after_test_module, clippy::manual_range_contains, clippy::op_ref, clippy::needless_borrow, clippy::doc_overindented_list_items, clippy::match_single_binding)]
//! openproxy-core: headless LLM proxy library.
//!
//! See docs/architecture.md and docs/mvp-spec.md for the full spec.

pub mod capabilities;
pub mod config;
pub mod error;
pub mod ids;
pub mod routing;

pub mod accounts;
pub mod adapters;
pub mod admin;
pub mod analytics;
pub mod api_keys;
pub mod bootstrap;
pub mod circuit_breaker;
pub mod combos;
pub mod compression;
pub mod cooldown;
pub mod cost;
pub mod db;
pub mod free_proxies;
pub mod discovery_scheduler;
pub mod endpoint;
pub mod antigravity_headers;
pub mod executor_antigravity;
pub mod executor_kiro;
pub mod model_normalize;
pub mod models;
pub mod models_dev_sync;
pub mod notifications;
pub mod oauth;
pub mod oauth_antigravity;
pub mod oauth_generic;

pub mod oauth_kiro;
pub mod oauth_tickets;
pub mod pipeline;
pub mod pricing;
pub mod providers;
pub mod quota;
pub mod race;
pub mod race_sink;
pub mod redact;
pub mod retry;
pub mod secrets;
pub mod seed;
pub mod sse;
pub mod sse_accumulator;
pub mod think_extractor;
pub mod timeouts;
pub mod token_estimate;
pub mod translation;
pub mod usage;

// Gate 0: hyper-based upstream client. See `upstream/mod.rs` for the
// architecture and the `upstream-hyper` feature flag in `Cargo.toml`.
// This module coexists with the existing reqwest-based call sites;
// Gate 0 does NOT migrate any call site.
pub mod upstream;

pub use config::AppConfig;
pub use error::{CoreError, ErrorContext, Result};

/// Install the rustls process-level crypto provider.
///
/// Mandatory since rustls 0.23. Without this, the first TLS
/// handshake to an upstream HTTPS endpoint panics with
/// `Could not automatically determine the process-level
/// CryptoProvider`.
///
/// `install_default` is idempotent (it populates a
/// `process-level OnceLock`); a second call is a no-op. The
/// server binary calls this at the very top of `main` so
/// the install is in place before any tokio worker
/// processes an inbound request.
///
/// ponytail: choosing `ring` over `aws-lc-rs` because it's
/// pure-Rust, smaller in binary size, and has no native
/// build step. `aws-lc-rs` is also pulled in transitively
/// by `reqwest` (for the OAuth admin HTTPS calls) but
/// rustls only accepts a single provider per process.
#[cfg(feature = "upstream-hyper")]
pub fn install_rustls_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}
