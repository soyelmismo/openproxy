//! openproxy-web: dashboard SPA que consume openproxy-core via API.

pub mod api_proxy;
pub mod handlers;

use openproxy_api_client::Client;
use std::sync::Arc;

#[derive(Clone)]
pub struct WebState {
    pub core_client: Arc<Client>,
    pub core_url: String,
    pub http: reqwest::Client,
    pub admin_token: Option<String>,
}

impl WebState {
    pub fn new(core_url: String, admin_token: Option<String>) -> Self {
        Self {
            core_client: Arc::new(Client::new(core_url.clone())),
            core_url,
            // Bug fix: set a 30s timeout on the proxy client so a
            // hung core-server request doesn't block the dashboard
            // indefinitely. The previous `reqwest::Client::new()`
            // had NO timeout — if the core server hung (e.g. a
            // deadlock in the refresh handler), the proxy would wait
            // forever and the dashboard would show "error sending
            // request" only after the browser's own TCP timeout
            // (typically 60-120s). With a 30s timeout, the proxy
            // returns a 502 after 30s, which the dashboard can
            // surface immediately.
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("reqwest client builder"),
            admin_token,
        }
    }
}

pub fn build_router(state: WebState) -> axum::Router {
    use axum::{Router, routing::get};

    Router::new()
        // Dashboard UI
        .route("/", get(handlers::index_html))
        .route("/callback.html", get(handlers::callback_html))
        // API proxy: todo /web/api/* se forwarda al core
        .nest("/web/api", api_proxy::router())
        // Health check del web
        .route("/web/health", get(handlers::web_health))
        // Static assets (src/, styles/) — catches the refactored
        // frontend bundle served from crates/openproxy-web/src/static.
        .fallback(handlers::serve_static)
        .with_state(state)
}
