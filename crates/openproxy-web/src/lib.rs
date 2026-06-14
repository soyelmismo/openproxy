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
            http: reqwest::Client::new(),
            admin_token,
        }
    }
}

pub fn build_router(state: WebState) -> axum::Router {
    use axum::{routing::get, Router};

    Router::new()
        // Dashboard UI
        .route("/", get(handlers::index_html))
        .route("/app.js", get(handlers::app_js))
        .route("/styles.css", get(handlers::styles_css))
        .route("/callback.html", get(handlers::callback_html))
        // API proxy: todo /web/api/* se forwarda al core
        .nest("/web/api", api_proxy::router())
        // Health check del web
        .route("/web/health", get(handlers::web_health))
        .with_state(state)
}
