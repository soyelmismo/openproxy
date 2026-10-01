//! `POST /v1/embeddings` — OpenAI-compatible embeddings endpoint.
//!
//! Delegates routing resolution, credential decryption, upstream dispatch,
//! and usage recording to [`openproxy_core::embeddings::execute_embeddings`].

use axum::{
    Json,
    extract::State,
    http::HeaderMap,
    response::{IntoResponse, Response},
};
use openproxy_core::embeddings::execute_embeddings;
use openproxy_types::{CoreError, embeddings::EmbeddingRequest};

use crate::{error::ApiError, state::AppState};

pub fn router(state: &crate::state::AppState) -> axum::Router<AppState> {
    axum::Router::new()
        .route("/embeddings", axum::routing::post(create_embeddings))
        // Security (OP-02): header-only auth BEFORE the Json extractor buffers
        // the request body — an unauthenticated client used to be able to pin
        // up to 32 MiB per request in RAM (multiplied by concurrency).
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

/// `POST /v1/embeddings`.
pub async fn create_embeddings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<EmbeddingRequest>,
) -> Result<Response, ApiError> {
    if req.input.is_empty() {
        return Err(ApiError(CoreError::Validation(
            "input cannot be empty".into(),
        )));
    }

    let api_key_id =
        crate::middleware::auth::authenticate_and_authorize_model(&state, &headers, &req.model)
            .await?;

    let response = call_unary_executor!(execute_embeddings, state, req, api_key_id);

    Ok(Json(response).into_response())
}
