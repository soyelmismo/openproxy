//! `POST /v1/systemone` — System One (Jev / Laya) protocol public entry point.
//!
//! Delegates routing resolution, credential decryption, upstream dispatch,
//! and usage recording to [`openproxy_core::systemone::execute_system_one`].

use axum::{
    Json,
    extract::State,
    http::HeaderMap,
    response::{IntoResponse, Response},
};
use openproxy_core::systemone::execute_system_one;
use openproxy_types::{CoreError, systemone::SystemOneRequest};

use crate::{error::ApiError, state::AppState};

pub fn router(state: &crate::state::AppState) -> axum::Router<AppState> {
    axum::Router::new()
        .route("/systemone", axum::routing::post(handle_system_one))
        // Security (OP-02): header-only auth BEFORE the Json extractor buffers
        // the request body (pre-auth 32 MiB buffering per request).
        // Security (OP-03): rate limit + per-key concurrency cap.
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::middleware::rate_limit::rate_limit_middleware,
        ))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::middleware::auth::key_auth_middleware,
        ))
}

/// `POST /v1/systemone`.
pub async fn handle_system_one(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SystemOneRequest>,
) -> Result<Response, ApiError> {
    if req.questions.is_empty() {
        return Err(ApiError(CoreError::Validation(
            "questions cannot be empty".into(),
        )));
    }

    let model_name = req.model.as_deref().unwrap_or("jev-latest");
    let api_key_id =
        crate::middleware::auth::authenticate_and_authorize_model(&state, &headers, model_name)
            .await?;

    let adapters_arc = state.adapters();
    let deps = openproxy_core::unary::UnaryDeps {
        db_pool: state.db_pool().as_ref(),
        adapters: adapters_arc.as_slice(),
        upstream_client: state.upstream_client(),
        circuit_breaker: &state.circuit_breaker(),
        master_key: state.master_key().as_ref(),
    };
    let response = execute_system_one(deps, req, api_key_id, state.is_recording())
        .await
        .map_err(crate::error::ApiError)?;

    Ok(Json(response).into_response())
}
