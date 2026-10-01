//! openproxy-core: headless LLM proxy library. See docs/architecture.md and
//! docs/mvp-spec.md.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod capabilities;
pub mod config;
pub(crate) use openproxy_types::error;
pub(crate) use openproxy_types::ids;
pub mod routing;

pub mod accounts;

pub use openproxy_oauth::account_scanner;

pub mod admin;
pub use openproxy_analytics::{analytics, usage};
pub mod audio;
pub mod images;
pub use images::{
    MultipartFile, ParsedImageMultipartBody, execute_image_edit, execute_image_generation,
    execute_image_variation,
};
pub mod embeddings;
pub use embeddings::execute_embeddings;
pub mod systemone;
pub use systemone::execute_system_one;
pub mod unary;

pub mod api_keys;
pub mod backup;
pub mod bootstrap;

pub mod discovery_scheduler;
pub mod free_proxies;
pub mod models_dev_sync;
pub use openproxy_discovery::{model_normalize, models};
pub use openproxy_notifications as notifications;
pub use openproxy_oauth::oauth;

pub use openproxy_pricing as pricing;
pub use pricing::{cost, quota};
pub mod codex_resets;
pub mod minimax_checkin;
pub mod providers;
pub mod quota_sync;

pub use openproxy_db::batch;
pub mod seed;
pub mod smart_warmup;

pub mod token_estimate;

pub mod rate_limit;

// Gate 0: hyper-based upstream client (`upstream-hyper` feature) coexists with
// existing hyper call sites; it migrates none of them.

pub use config::AppConfig;

pub mod di;
pub mod validation;
pub use di::ServiceContainer;
pub use validation::Validatable;

/// Install the rustls process-level crypto provider.
///
/// rustls 0.23+ panics on the first HTTPS handshake without it
/// (`Could not automatically determine the process-level CryptoProvider`), so
/// the server binary calls this at the top of `main`, before any tokio worker
/// sees a request. `install_default` is idempotent (process-level `OnceLock`).
///
/// `ring` over `aws-lc-rs`: pure Rust, no native build step. `UpstreamClient`
/// pulls in `aws-lc-rs` transitively, but rustls accepts one provider per
/// process.
#[cfg(feature = "upstream-hyper")]
pub fn install_rustls_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}
