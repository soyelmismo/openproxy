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

pub fn router() -> axum::Router<AppState> {
    axum::Router::new().route("/embeddings", axum::routing::post(create_embeddings))
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
        crate::middleware::auth::authenticate_and_authorize_model(&state, &headers, &req.model)?;

    let response = call_unary_executor!(execute_embeddings, state, req, api_key_id);

    Ok(Json(response).into_response())
}
