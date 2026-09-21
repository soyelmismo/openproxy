//! `openproxy` — headless LLM proxy binary.
//!
//! Endpoints (per spec §2):
//! - `POST /v1/chat/completions`
//! - `GET  /v1/models`
//! - `GET  /v1/health`
//! - `*    /admin/*`  (CRUD for providers, accounts, combos, models, usage)
//!
//! Startup sequence:

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
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

fn configure_allocator() {
    unsafe {
        // Immediate segment purge on free (disables 10ms purge delay)
        libmimalloc_sys::mi_option_set(15 /* mi_option_purge_delay */, 0);
        // Disable eager arena commit on Linux overcommit systems
        libmimalloc_sys::mi_option_set(4 /* mi_option_arena_eager_commit */, 0);
        // Reduce initial arena reservation from 1 GiB to 64 MiB (in KiB)
        libmimalloc_sys::mi_option_set(23 /* mi_option_arena_reserve */, 64 * 1024);
        // Immediately purge pages when threads terminate (spawn_blocking pool)
        libmimalloc_sys::mi_option_set(12 /* mi_option_abandoned_page_purge */, 1);
    }
    for (k, v) in [
        ("MIMALLOC_PURGE_DELAY", "0"),
        ("MIMALLOC_ARENA_EAGER_COMMIT", "0"),
        ("MIMALLOC_ARENA_RESERVE", "65536"),
        ("MIMALLOC_ABANDONED_PAGE_PURGE", "1"),
    ] {
        if std::env::var_os(k).is_none() {
            unsafe {
                std::env::set_var(k, v);
            }
        }
    }
}

fn load_server_config() -> anyhow::Result<AppConfig> {
    let config_path = env::var("OPENPROXY_CONFIG").unwrap_or_else(|_| "config.toml".to_string());
    let config = AppConfig::load_or_default(&config_path)?;
    openproxy_server::telemetry::init(&config.logging)?;
    Ok(config)
}

async fn run_server(state: openproxy_server::state::AppState) -> anyhow::Result<()> {
    let bind_addr = state.config().server.bind.clone();
    let app = openproxy_server::router::build_router(state);
    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    tracing::info!(addr = %bind_addr, "openproxy listening");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;
    Ok(())
}

fn main() -> anyhow::Result<()> {
    // 0. Programmatic allocator configuration: Tune mimalloc options before
    // initializing telemetry, Tokio runtime, or database connections.
    configure_allocator();

    // 1. Install rustls crypto provider. `ring` is pure-Rust and
    // transitively available; `aws-lc-rs` is also pulled in by
    // `UpstreamClient`. `install_default` is idempotent — a second call in
    // the same process is a no-op, so it's safe even if a future
    // test harness re-instruments startup.
    openproxy_core::install_rustls_crypto_provider();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    runtime.block_on(async {
        let config = load_server_config()?;
        let state = openproxy_server::state::AppState::new(config)?;

        #[cfg(feature = "laya-engine")]
        {
            let pool = std::sync::Arc::clone(state.db_pool());
            tokio::task::spawn_blocking(move || {
                let conn = pool.writer();
                let is_laya_active = openproxy_core::providers::get(
                    &conn,
                    &openproxy_types::ProviderId::new("laya"),
                )
                .is_ok_and(|p_opt| p_opt.is_some_and(|p| p.active));
                if is_laya_active {
                    openproxy_adapters::laya_engine::spawn_init_background();
                }
            });
        }

        unsafe {
            libmimalloc_sys::mi_collect(true);
        }
        run_server(state).await
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allocator_options_configured() {
        configure_allocator();
        unsafe {
            assert_eq!(libmimalloc_sys::mi_option_get(15), 0);
            assert_eq!(libmimalloc_sys::mi_option_get(4), 0);
            assert_eq!(libmimalloc_sys::mi_option_get(23), 64 * 1024);
            assert_eq!(libmimalloc_sys::mi_option_get(12), 1);
        }
        assert_eq!(std::env::var("MIMALLOC_PURGE_DELAY").unwrap(), "0");
        assert_eq!(std::env::var("MIMALLOC_ARENA_EAGER_COMMIT").unwrap(), "0");
        assert_eq!(std::env::var("MIMALLOC_ARENA_RESERVE").unwrap(), "65536");
        assert_eq!(std::env::var("MIMALLOC_ABANDONED_PAGE_PURGE").unwrap(), "1");
    }
}
