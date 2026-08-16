//! `POST /v1/images/generations` — OpenAI-compatible image generation endpoint.
//! `POST /v1/images/edits` — OpenAI-compatible image edit endpoint.
//! `POST /v1/images/variations` — OpenAI-compatible image variation endpoint.
//!
//! Delegates routing resolution, credential decryption, upstream dispatch,
//! and usage recording to [`openproxy_core::images`].

use axum::{
    Json,
    extract::{Multipart, State},
    http::HeaderMap,
    response::{IntoResponse, Response},
};
use openproxy_core::images::{
    MultipartFile, ParsedImageMultipartBody, execute_image_edit, execute_image_generation,
    execute_image_variation,
};
use openproxy_types::{CoreError, ids::ApiKeyId, images::ImageGenerationRequest};

use crate::{error::ApiError, state::AppState};

/// `POST /v1/images/generations`.
pub async fn generate_images(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut req): Json<ImageGenerationRequest>,
) -> Result<Response, ApiError> {
    if req.prompt.is_empty() {
        return Err(ApiError(CoreError::Validation(
            "prompt cannot be empty".into(),
        )));
    }

    if req.model.is_empty() {
        req.model = "dall-e-2".to_string();
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

    let response = execute_image_generation(
        state.db_pool().as_ref(),
        state.adapters().as_slice(),
        state.upstream_client(),
        &state.circuit_breaker(),
        state.master_key().as_ref(),
        req,
        api_key_id,
    )
    .await
    .map_err(ApiError)?;

    Ok(Json(response).into_response())
}

/// `POST /v1/images/edits`.
pub async fn edit_images(
    State(state): State<AppState>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Result<Response, ApiError> {
    let parsed_body = parse_image_multipart(multipart).await?;

    if !parsed_body
        .files
        .iter()
        .any(|f| f.name == "image" && !f.bytes.is_empty())
    {
        return Err(ApiError(CoreError::Validation(
            "missing or empty 'image' in multipart body".into(),
        )));
    }

    let has_prompt = parsed_body
        .form_fields
        .iter()
        .any(|(k, v)| k == "prompt" && !v.trim().is_empty());
    if !has_prompt {
        return Err(ApiError(CoreError::Validation(
            "prompt cannot be empty".into(),
        )));
    }

    let auth_result =
        crate::middleware::auth::authenticate(&state, &headers, &parsed_body.model_name)?;
    let api_key_id: Option<ApiKeyId> = auth_result.as_ref().map(|r| r.key_id);

    // Check combo authorization if applicable.
    if let Ok(openproxy_core::routing::RoutingPlan::Combo { combo_id, .. }) =
        openproxy_core::routing::resolve(&state.db_pool().reader(), &parsed_body.model_name)
        && let Some(auth) = &auth_result
        && !auth.is_combo_allowed(combo_id.0)
    {
        return Err(ApiError(CoreError::Auth(
            "combo not allowed for this key".into(),
        )));
    }

    let response = execute_image_edit(
        state.db_pool().as_ref(),
        state.adapters().as_slice(),
        state.upstream_client(),
        &state.circuit_breaker(),
        state.master_key().as_ref(),
        parsed_body,
        api_key_id,
    )
    .await
    .map_err(ApiError)?;

    Ok(Json(response).into_response())
}

/// `POST /v1/images/variations`.
pub async fn create_image_variation(
    State(state): State<AppState>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Result<Response, ApiError> {
    let parsed_body = parse_image_multipart(multipart).await?;

    if !parsed_body
        .files
        .iter()
        .any(|f| f.name == "image" && !f.bytes.is_empty())
    {
        return Err(ApiError(CoreError::Validation(
            "missing or empty 'image' in multipart body".into(),
        )));
    }

    let auth_result =
        crate::middleware::auth::authenticate(&state, &headers, &parsed_body.model_name)?;
    let api_key_id: Option<ApiKeyId> = auth_result.as_ref().map(|r| r.key_id);

    // Check combo authorization if applicable.
    if let Ok(openproxy_core::routing::RoutingPlan::Combo { combo_id, .. }) =
        openproxy_core::routing::resolve(&state.db_pool().reader(), &parsed_body.model_name)
        && let Some(auth) = &auth_result
        && !auth.is_combo_allowed(combo_id.0)
    {
        return Err(ApiError(CoreError::Auth(
            "combo not allowed for this key".into(),
        )));
    }

    let response = execute_image_variation(
        state.db_pool().as_ref(),
        state.adapters().as_slice(),
        state.upstream_client(),
        &state.circuit_breaker(),
        state.master_key().as_ref(),
        parsed_body,
        api_key_id,
    )
    .await
    .map_err(ApiError)?;

    Ok(Json(response).into_response())
}

async fn parse_image_multipart(
    mut multipart: Multipart,
) -> Result<ParsedImageMultipartBody, ApiError> {
    let mut model_name = String::new();
    let mut files = Vec::new();
    let mut form_fields = Vec::new();

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError(CoreError::Validation(format!("multipart parse: {e}"))))?
    {
        let name = field.name().unwrap_or("").to_string();
        let is_file = field.file_name().is_some() || name == "image" || name == "mask";
        if is_file && name != "model" {
            let file_name = field.file_name().unwrap_or("image.png").to_string();
            let content_type = field
                .content_type()
                .unwrap_or("image/png")
                .to_string();
            let bytes = field.bytes().await.unwrap_or_default();
            files.push(MultipartFile {
                name,
                file_name,
                content_type,
                bytes,
            });
        } else if name == "model" {
            model_name = field.text().await.unwrap_or_default();
        } else {
            let value = field.text().await.unwrap_or_default();
            form_fields.push((name, value));
        }
    }

    if model_name.is_empty()
        && let Some((_, v)) = form_fields.iter().find(|(k, _)| k == "model")
    {
        model_name = v.clone();
    }
    if model_name.is_empty() {
        model_name = "dall-e-2".to_string();
    }

    Ok(ParsedImageMultipartBody {
        model_name,
        files,
        form_fields,
    })
}
