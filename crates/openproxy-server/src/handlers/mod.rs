//! HTTP handler modules.
//!
//! Each submodule is one cluster of axum handlers (`chat`, `models`, `admin`, `audio`).
//! The router in [`crate::router`] wires them up; shared concerns like
//! error mapping and state extraction live in [`crate::error`] and
//! [`crate::state`].

macro_rules! call_unary_executor {
    ($executor:path, $state:expr, $req:expr, $api_key_id:expr) => {
        $executor(
            $state.db_pool().as_ref(),
            $state.adapters().as_slice(),
            $state.upstream_client(),
            &$state.circuit_breaker(),
            $state.master_key().as_ref(),
            $req,
            $api_key_id,
        )
        .await
        .map_err(crate::error::ApiError)?
    };
}

pub mod admin;
pub mod audio;
pub mod chat;
pub mod embeddings;
pub mod images;
pub mod messages;
pub mod models;

use crate::state::AppState;
use axum::routing::post;

/// Shared chat/messages endpoint wrapper with disconnect, rate limiting,
/// routing, and auth middlewares.
pub(crate) fn chat_endpoint<H, T>(
    state: &AppState,
    handler: H,
) -> axum::routing::MethodRouter<AppState>
where
    H: axum::handler::Handler<T, AppState>,
    T: 'static,
{
    post(handler)
        .route_layer(axum::middleware::from_fn(
            crate::disconnect::client_disconnect_middleware,
        ))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::middleware::rate_limit::rate_limit_middleware,
        ))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::middleware::routing::routing_middleware,
        ))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::middleware::auth::auth_middleware,
        ))
}

/// Assembles all public API sub-routers into the unified public API router under `/v1`.
pub fn public_api_routes(state: &AppState) -> axum::Router<AppState> {
    let v1_routes = axum::Router::new()
        .merge(models::router())
        .merge(messages::router(state))
        .merge(embeddings::router())
        .nest("/chat", chat::router(state))
        .nest("/audio", audio::router())
        .nest("/images", images::router());

    axum::Router::new().nest("/v1", v1_routes)
}
