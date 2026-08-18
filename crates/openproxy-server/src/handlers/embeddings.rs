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
use openproxy_types::{CoreError, embeddings::EmbeddingRequest, ids::ApiKeyId};

use crate::{error::ApiError, state::AppState};

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

    let auth_result = crate::middleware::auth::authenticate(&state, &headers, &req.model)?;
    let api_key_id: Option<ApiKeyId> = auth_result.as_ref().map(|r| r.key_id);

    // Check combo authorization if applicable.
    if let Ok(openproxy_core::routing::RoutingPlan::Combo { combo_id, .. }) =
        openproxy_core::routing::resolve(&state.db_pool().reader(), &req.model)
        && let Some(auth) = &auth_result
        && !auth.is_combo_allowed(combo_id.0)
    {
        return Err(ApiError(CoreError::Auth(
            "combo not allowed for this key".into(),
        )));
    }

    let response = call_unary_executor!(execute_embeddings, state, req, api_key_id);

    Ok(Json(response).into_response())
}
