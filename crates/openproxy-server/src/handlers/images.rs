//! `POST /v1/images/generations` — OpenAI-compatible image generation endpoint.
//! `POST /v1/images/edits` — OpenAI-compatible image edit endpoint.
//! `POST /v1/images/variations` — OpenAI-compatible image variation endpoint.
//!
//! Delegates routing resolution, credential decryption, upstream dispatch,
//! and usage recording to [`openproxy_core::images`].

use std::sync::Arc;

use axum::{
    Json,
    extract::{FromRequest, Multipart, Request, State},
    http::{HeaderMap, header::CONTENT_TYPE},
    response::{IntoResponse, Response},
};
use openproxy_core::images::{
    MultipartFile, ParsedImageMultipartBody, execute_image_edit, execute_image_generation,
    execute_image_variation,
};
use openproxy_types::{CoreError, images::ImageGenerationRequest};

use crate::{error::ApiError, state::AppState};

pub fn router() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/generations", axum::routing::post(generate_images))
        .route("/edits", axum::routing::post(edit_images))
        .route("/variations", axum::routing::post(create_image_variation))
}

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

    let api_key_id =
        crate::middleware::auth::authenticate_and_authorize_model(&state, &headers, &req.model)?;

    let response = call_unary_executor!(execute_image_generation, state, req, api_key_id);

    Ok(Json(response).into_response())
}

/// `POST /v1/images/edits`.
pub async fn edit_images(
    State(state): State<AppState>,
    headers: HeaderMap,
    req: Request,
) -> Result<Response, ApiError> {
    let parsed_body = parse_image_request(req, state.upstream_client()).await?;

    if !parsed_body
        .files
        .iter()
        .any(|f| f.name == "image" && !f.bytes.is_empty())
    {
        return Err(ApiError(CoreError::Validation(
            "missing or empty 'image' in request body".into(),
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

    let api_key_id = crate::middleware::auth::authenticate_and_authorize_model(
        &state,
        &headers,
        &parsed_body.model_name,
    )?;

    let response = call_unary_executor!(execute_image_edit, state, parsed_body, api_key_id);

    Ok(Json(response).into_response())
}

/// `POST /v1/images/variations`.
pub async fn create_image_variation(
    State(state): State<AppState>,
    headers: HeaderMap,
    req: Request,
) -> Result<Response, ApiError> {
    let parsed_body = parse_image_request(req, state.upstream_client()).await?;

    if !parsed_body
        .files
        .iter()
        .any(|f| f.name == "image" && !f.bytes.is_empty())
    {
        return Err(ApiError(CoreError::Validation(
            "missing or empty 'image' in request body".into(),
        )));
    }

    let api_key_id = crate::middleware::auth::authenticate_and_authorize_model(
        &state,
        &headers,
        &parsed_body.model_name,
    )?;

    let response = call_unary_executor!(execute_image_variation, state, parsed_body, api_key_id);

    Ok(Json(response).into_response())
}

async fn parse_image_request(
    req: Request,
    upstream_client: &Arc<openproxy_adapters::UpstreamClient>,
) -> Result<ParsedImageMultipartBody, ApiError> {
    let content_type = req
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    if content_type.contains("multipart/form-data") {
        let multipart = Multipart::from_request(req, &())
            .await
            .map_err(|e| ApiError(CoreError::Validation(format!("multipart extract: {e}"))))?;
        return parse_image_multipart(multipart).await;
    }

    // Otherwise, parse as JSON payload
    let body_bytes = axum::body::to_bytes(req.into_body(), 64 * 1024 * 1024)
        .await
        .map_err(|e| ApiError(CoreError::Validation(format!("read body: {e}"))))?;

    let json: serde_json::Value = serde_json::from_slice(&body_bytes)
        .map_err(|e| ApiError(CoreError::Validation(format!("invalid json body: {e}"))))?;

    let mut model_name = json
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("dall-e-2")
        .to_string();
    if model_name.is_empty() {
        model_name = "dall-e-2".to_string();
    }

    let mut files = Vec::new();
    let mut form_fields = Vec::new();

    if let Some(map) = json.as_object() {
        for (k, v) in map {
            if k == "image" {
                if let Some(s) = v.as_str() {
                    let file = resolve_image_input(s, "image", upstream_client).await?;
                    files.push(file);
                }
            } else if k == "mask" {
                if let Some(s) = v.as_str() {
                    let file = resolve_image_input(s, "mask", upstream_client).await?;
                    files.push(file);
                }
            } else if let Some(s) = v.as_str() {
                form_fields.push((k.clone(), s.to_string()));
            } else if let Some(n) = v.as_i64() {
                form_fields.push((k.clone(), n.to_string()));
            } else if let Some(f) = v.as_f64() {
                form_fields.push((k.clone(), f.to_string()));
            } else if let Some(b) = v.as_bool() {
                form_fields.push((k.clone(), b.to_string()));
            }
        }
    }

    Ok(ParsedImageMultipartBody {
        model_name,
        files,
        form_fields,
    })
}

fn png_multipart_file(field_name: &str, bytes: bytes::Bytes) -> MultipartFile {
    MultipartFile {
        name: field_name.to_string(),
        file_name: format!("{field_name}.png"),
        content_type: "image/png".to_string(),
        bytes,
    }
}

async fn resolve_image_input(
    raw: &str,
    field_name: &str,
    upstream_client: &Arc<openproxy_adapters::UpstreamClient>,
) -> Result<MultipartFile, ApiError> {
    use base64::Engine as _;
    let trimmed = raw.trim();

    if trimmed.starts_with("data:image/")
        && let Some((_, b64_part)) = trimmed.split_once(";base64,")
    {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64_part.trim())
            .map_err(|e| ApiError(CoreError::Validation(format!("invalid base64 image: {e}"))))?;
        return Ok(png_multipart_file(field_name, bytes.into()));
    }

    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        let req = openproxy_adapters::UpstreamRequest::get(trimmed);
        let cancel = openproxy_adapters::CancellationToken::new();
        let resp = upstream_client
            .call(req, openproxy_adapters::TimeoutProfile::Chat, cancel)
            .await
            .map_err(|e| {
                ApiError(CoreError::UpstreamConnection(format!(
                    "failed to fetch image URL: {e}"
                )))
            })?;
        let bytes = resp.collect().await.map_err(|e| {
            ApiError(CoreError::UpstreamConnection(format!(
                "failed to read image URL body: {e}"
            )))
        })?;
        return Ok(png_multipart_file(field_name, bytes));
    }

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(trimmed)
        .map_err(|e| {
            ApiError(CoreError::Validation(format!(
                "invalid base64 image data: {e}"
            )))
        })?;
    Ok(png_multipart_file(field_name, bytes.into()))
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
            let content_type = field.content_type().unwrap_or("image/png").to_string();
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
