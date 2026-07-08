//! `openproxy` — headless LLM proxy binary.
//!
//! Endpoints (per spec §2):
//! - `POST /v1/chat/completions`
//! - `GET  /v1/models`
//! - `GET  /v1/health`
//! - `*    /admin/*`  (CRUD for providers, accounts, combos, models, usage)
//!
//! Startup sequence:
//! 0. Install a process-wide rustls crypto provider (mandatory since
//!    rustls 0.23 — without this, the first TLS handshake to an
//!    upstream HTTPS endpoint panics with `Could not automatically
//!    determine the process-level CryptoProvider`).
//! 1. Load config from `OPENPROXY_CONFIG` (defaults to `./config.toml`).
//! 2. Init `tracing`.
//! 3. Build the shared [`AppState`] (DB pool, master key, adapters, HTTP client).
//! 4. Build the axum router.
//! 5. Bind the configured TCP listener and serve until shutdown.

use openproxy_core::AppConfig;
use std::env;

// mimalloc as the global allocator. glibc malloc retains freed arenas
// aggressively, which inflates idle RSS for long-running services that
// go through bursts of allocation (startup migrations, models.dev sync,
// discovery refresh, large request bodies). mimalloc returns memory to
// the OS more eagerly and typically cuts idle RSS 20-40% on Rust
// services like this one. This must be declared at crate scope so it
// lives for the entire program duration.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 0. Install rustls crypto provider. `ring` is pure-Rust and
    // transitively available; `aws-lc-rs` is also pulled in by
    // `UpstreamClient`. `install_default` is idempotent — a second call in
    // the same process is a no-op, so it's safe even if a future
    // test harness re-instruments startup.
    //
    // ponytail: this is a single line and mandatory since
    // rustls 0.23; without it, the first TLS handshake to an
    // upstream HTTPS endpoint panics with `Could not
    // automatically determine the process-level CryptoProvider`.
    openproxy_core::install_rustls_crypto_provider();

    // 1. Load config
    let config_path = env::var("OPENPROXY_CONFIG").unwrap_or_else(|_| "config.toml".to_string());
    let config = AppConfig::load_or_default(&config_path)?;

    // 2. Init telemetry
    openproxy_server::telemetry::init(&config.logging)?;

    // 3. Build state
    let state = openproxy_server::state::AppState::new(config).await?;

    // 4. Build router (state is moved into the router).
    let bind_addr = state.config().server.bind.clone();
    let app = openproxy_server::router::build_router(state);

    // 5. Bind and serve
    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    tracing::info!(addr = %bind_addr, "openproxy listening");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;
    Ok(())
}
