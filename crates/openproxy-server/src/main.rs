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

/// Hard cap on concurrently-accepted TCP connections.
///
/// Security (OP-13): without a cap, an unauthenticated client can open an
/// unbounded number of sockets (each pinning a tokio task + buffers).
const MAX_CONNECTIONS: usize = 1024;

/// Max time to receive the request head (request line + headers) before the
/// connection is dropped.
///
/// Security (OP-13): without this deadline a slowloris — sockets that drip
/// header bytes — are never closed by hyper's defaults.
const HTTP1_HEADER_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

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

fn resolve_config_path() -> String {
    env::var("OPENPROXY_CONFIG").unwrap_or_else(|_| {
        let home_cfg = env::var("HOME").ok().map(|h| format!("{h}/.openproxy/config.toml"));
        if let Some(ref p) = home_cfg
            && std::path::Path::new(p).exists()
        {
            p.clone()
        } else {
            "config.toml".to_string()
        }
    })
}

fn load_server_config() -> anyhow::Result<AppConfig> {
    let config_path = resolve_config_path();
    let config = AppConfig::load_or_default(&config_path)?;
    openproxy_server::telemetry::init(&config.logging)?;
    Ok(config)
}

/// Whether `bind` restricts the listener to the loopback interface.
fn is_loopback_bind(bind: &str) -> bool {
    let host = bind.rsplit_once(':').map_or(bind, |(h, _)| h);
    host == "localhost" || host.starts_with("127.") || host == "[::1]" || host == "::1"
}

async fn run_server(state: openproxy_server::state::AppState) -> anyhow::Result<()> {
    let bind_addr = state.config().server.bind.clone();
    // Security (OP-04): this binary has no TLS support — every credential
    // (admin Bearer tokens included) would travel in cleartext. Make an
    // externally-reachable bind an explicit, warned operator decision instead
    // of a silent default.
    if !is_loopback_bind(&bind_addr) {
        tracing::warn!(
            addr = %bind_addr,
            "openproxy is binding a NON-LOOPBACK interface over plain HTTP. \
             The binary has no TLS support: admin and API credentials will travel \
             unencrypted unless a TLS-terminating reverse proxy fronts this port."
        );
    }
    let app = openproxy_server::router::build_router(state);
    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    tracing::info!(addr = %bind_addr, "openproxy listening");
    serve_with_limits(listener, app).await
}

/// Accept loop with connection hardening (OP-13).
///
/// `axum::serve` does not expose `http1_header_read_timeout` or a connection
/// cap, so the loop drives hyper-util's auto connection builder directly —
/// the same stack `axum::serve` uses internally — adding:
///
/// - `MAX_CONNECTIONS`: a semaphore-bounded accept; excess connections are
///   closed immediately instead of accumulating tasks/FDs.
/// - `HTTP1_HEADER_READ_TIMEOUT`: sockets that never finish sending their
///   request head are dropped after 10 s (slowloris).
async fn serve_with_limits(
    listener: tokio::net::TcpListener,
    app: axum::Router,
) -> anyhow::Result<()> {
    use hyper_util::{
        rt::{TokioExecutor, TokioIo},
        server::conn::auto::Builder,
        service::TowerToHyperService,
    };
    use tower::{Service, ServiceExt};

    let connection_slots =
        std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    let mut make_service = app.into_make_service_with_connect_info::<std::net::SocketAddr>();

    loop {
        let (tcp, remote_addr) = listener.accept().await?;
        let io = TokioIo::new(tcp);

        // `IntoMakeServiceWithConnectInfo::poll_ready` is always `Ready(Ok)`
        // (see axum's connect_info.rs), so `call` can be invoked directly.
        let tower_service = make_service
            .call(remote_addr)
            .await
            .unwrap_or_else(|err| match err {})
            .map_request(|req: axum::extract::Request<_>| req.map(axum::body::Body::new));
        let hyper_service = TowerToHyperService::new(tower_service);

        // Bound concurrent connections: if all slots are taken, shed the new
        // connection instead of queuing it (unbounded task growth).
        let Ok(permit) = std::sync::Arc::clone(&connection_slots).try_acquire_owned() else {
            tracing::warn!(
                peer = %remote_addr,
                "connection limit reached ({}), dropping new connection",
                MAX_CONNECTIONS
            );
            // Drop `io` (closing the socket) and continue accepting.
            drop(io);
            continue;
        };

        tokio::spawn(async move {
            let mut builder = Builder::new(TokioExecutor::new());
            // CONNECT protocol needed for HTTP/2 websockets.
            builder.http2().enable_connect_protocol();
            // `header_read_timeout` requires an explicit timer on the
            // connection (hyper panics otherwise).
            builder
                .http1()
                .timer(hyper_util::rt::TokioTimer::new())
                .header_read_timeout(HTTP1_HEADER_READ_TIMEOUT);
            let conn = builder.serve_connection_with_upgrades(io, hyper_service);
            if let Err(e) = conn.await {
                tracing::debug!(%e, "connection error");
            }
            // Release the slot when the connection has fully finished.
            drop(permit);
        });
    }
}

fn main() -> anyhow::Result<()> {
    // Healthcheck mode (OP-23): distroless containers have no shell, wget or
    // curl, so the image's HEALTHCHECK invokes the binary itself. This runs a
    // TCP connect against the configured bind address and exits 0/1 — usable
    // by Docker/Kubernetes without any extra tooling in the image.
    if std::env::args().any(|a| a == "--healthcheck") {
        let config_path = resolve_config_path();
        let config = AppConfig::load_or_default(&config_path)?;
        let bind = config.server.bind;
        let addr = bind
            .parse::<std::net::SocketAddr>()
            .or_else(|_| {
                // Bind strings like "localhost:8787" need DNS; try the
                // canonical loopback form as a fallback.
                bind.replacen("localhost", "127.0.0.1", 1)
                    .parse::<std::net::SocketAddr>()
            })
            .map_err(|e| anyhow::anyhow!("invalid bind address {bind:?}: {e}"))?;
        match std::net::TcpStream::connect_timeout(
            &addr,
            std::time::Duration::from_secs(3),
        ) {
            Ok(_) => Ok(()),
            Err(e) => anyhow::bail!("healthcheck: cannot connect to {addr}: {e}"),
        }
    } else {
        run_main()
    }
}

fn run_main() -> anyhow::Result<()> {
    // 0. Programmatic allocator configuration: tune mimalloc before telemetry,
    //    the Tokio runtime, or any DB connection exists.
    configure_allocator();

    // 1. rustls crypto provider. Mandatory since rustls 0.23: without it the
    //    first upstream TLS handshake panics with `Could not automatically
    //    determine the process-level CryptoProvider`. `ring` is pure-Rust and
    //    transitively available; `aws-lc-rs` is also pulled in by
    //    `UpstreamClient`. `install_default` is idempotent, so a second call in
    //    the same process is a no-op.
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
