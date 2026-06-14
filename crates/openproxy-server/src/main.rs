//! `openproxy` — headless LLM proxy binary.
//!
//! Endpoints (per spec §2):
//! - `POST /v1/chat/completions`
//! - `GET  /v1/models`
//! - `GET  /v1/health`
//! - `*    /v1/admin/*`  (CRUD for providers, accounts, combos, models, usage)
//!
//! Startup sequence:
//! 1. Load config from `OPENPROXY_CONFIG` (defaults to `./config.toml`).
//! 2. Init `tracing`.
//! 3. Build the shared [`AppState`] (DB pool, master key, adapters, HTTP client).
//! 4. Build the axum router.
//! 5. Bind the configured TCP listener and serve until shutdown.

use openproxy_core::AppConfig;
use std::env;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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
    axum::serve(listener, app).await?;
    Ok(())
}
