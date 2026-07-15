#![allow(
    clippy::too_many_arguments,
    clippy::needless_range_loop,
    clippy::ptr_arg
)]
#![allow(
    clippy::vec_init_then_push,
    clippy::items_after_test_module,
    clippy::manual_range_contains,
    clippy::op_ref,
    clippy::needless_borrow,
    clippy::doc_overindented_list_items,
    clippy::match_single_binding
)]
//! openproxy-core: headless LLM proxy library.
//!
//! See docs/architecture.md and docs/mvp-spec.md for the full spec.

pub mod capabilities;
pub mod config;
#[allow(unused_imports)]
pub(crate) use openproxy_types::error::{self, CoreError, ErrorContext, Result};
pub(crate) use openproxy_types::ids;
pub mod routing;

pub mod accounts;

pub mod admin;
pub mod analytics;

pub mod api_keys;
pub mod bootstrap;
pub mod combos;

pub mod cost;

pub mod discovery_scheduler;
pub(crate) use openproxy_types::endpoint;
pub mod executor_antigravity;
pub mod schema_cleaner;

pub mod executor_kiro;
pub mod free_proxies;
pub mod model_normalize;
pub mod models;
pub mod models_dev_sync;
pub mod notifications;
pub mod oauth;
pub mod oauth_antigravity;
pub mod oauth_codex;
pub mod oauth_generic;

pub mod oauth_kiro;
pub mod oauth_tickets;

pub mod pricing;
pub mod providers;
pub mod quota;
pub mod quota_sync;
pub mod race;

#[allow(unused_imports)]
pub(crate) use openproxy_db::secrets;
pub mod seed;
pub mod smart_warmup;


pub mod token_estimate;

pub mod usage;

// Gate 0: hyper-based upstream client. See `upstream/mod.rs` for the
// architecture and the `upstream-hyper` feature flag in `Cargo.toml`.
// This module coexists with the existing hyper-based call sites;
// Gate 0 does NOT migrate any call site.


pub use config::AppConfig;

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
pub mod pipeline_repository_tests;
