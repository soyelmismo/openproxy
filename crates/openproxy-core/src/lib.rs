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
pub mod model_normalize;
pub mod providers;
pub mod accounts;
pub mod api_keys;
pub mod combos;
pub mod usage;
pub mod cost;
pub mod analytics;
pub mod translation;
pub mod sse;
pub mod sse_accumulator;
pub mod pipeline;
pub mod timeouts;
pub mod retry;
pub mod circuit_breaker;
pub mod cooldown;
pub mod race;
pub mod race_sink;
pub mod pricing;
pub mod secrets;
pub mod oauth;
pub mod oauth_antigravity;
pub mod oauth_gemini;
pub mod oauth_kiro;
pub mod oauth_tickets;
pub mod executor_kiro;
pub mod executor_antigravity;
pub mod admin;
pub mod quota;
pub mod seed;
pub mod bootstrap;
pub mod discovery_scheduler;
pub mod models_dev_sync;
pub mod redact;
pub mod compression;

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
