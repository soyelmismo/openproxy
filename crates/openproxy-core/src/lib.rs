//! openproxy-core: headless LLM proxy library.
//!
//! See docs/architecture.md and docs/mvp-spec.md for the full spec.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod capabilities;
pub mod config;
pub(crate) use openproxy_types::error;
pub(crate) use openproxy_types::ids;
pub mod routing;

pub mod accounts;

pub mod admin;
pub mod analytics;

pub mod api_keys;
pub mod bootstrap;

pub mod discovery_scheduler;
pub mod free_proxies;
pub mod model_normalize;
pub mod models;
pub mod models_dev_sync;
pub mod notifications;
pub mod oauth;

pub mod pricing;
pub use pricing::{cost, quota};
pub mod providers;
pub mod quota_sync;

pub use openproxy_db::batch;
pub mod seed;
pub mod smart_warmup;

pub mod token_estimate;

pub mod usage;

// Gate 0: hyper-based upstream client. See `upstream/mod.rs` for the
// architecture and the `upstream-hyper` feature flag in `Cargo.toml`.
// This module coexists with the existing hyper-based call sites;
// Gate 0 does NOT migrate any call site.

pub use config::AppConfig;

pub mod di;
pub mod validation;
pub use di::ServiceContainer;
pub use validation::Validatable;

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
/// by `UpstreamClient` (for the OAuth admin HTTPS calls) but
/// rustls only accepts a single provider per process.
#[cfg(feature = "upstream-hyper")]
pub fn install_rustls_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}
