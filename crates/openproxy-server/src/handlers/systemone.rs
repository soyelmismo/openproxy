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

pub fn router() -> axum::Router<AppState> {
    axum::Router::new().route("/systemone", axum::routing::post(handle_system_one))
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
        crate::middleware::auth::authenticate_and_authorize_model(&state, &headers, model_name)?;

    let response = call_unary_executor!(execute_system_one, state, req, api_key_id);

    Ok(Json(response).into_response())
}
