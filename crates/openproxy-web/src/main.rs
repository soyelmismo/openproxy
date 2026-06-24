use openproxy_web::{WebState, build_router};
use std::env;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let core_url =
        env::var("OPENPROXY_CORE_URL").unwrap_or_else(|_| "http://127.0.0.1:8787".to_string());
    // NOTE: Default is 0.0.0.0 for local dev / docker. In production, override via
    // OPENPROXY_WEB_BIND=127.0.0.1:8788 to keep the web UI loopback-only.
    let bind = env::var("OPENPROXY_WEB_BIND").unwrap_or_else(|_| "0.0.0.0:8788".to_string());

    tracing::info!(core_url = %core_url, bind = %bind, "openproxy-web starting");

    let admin_token = env::var("OPENPROXY_WEB_ADMIN_KEY").ok();
    let state = WebState::new(core_url, admin_token);
    let app = build_router(state);

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
