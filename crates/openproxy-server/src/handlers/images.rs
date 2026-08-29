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

fn validate_image_edit_body(body: &ParsedImageMultipartBody) -> Result<(), ApiError> {
    if !body
        .files
        .iter()
        .any(|f| f.name == "image" && !f.bytes.is_empty())
    {
        return Err(ApiError(CoreError::Validation(
            "missing or empty 'image' in request body".into(),
        )));
    }

    let has_prompt = body
        .form_fields
        .iter()
        .any(|(k, v)| k == "prompt" && !v.trim().is_empty());
    if !has_prompt {
        return Err(ApiError(CoreError::Validation(
            "prompt cannot be empty".into(),
        )));
    }
    Ok(())
}

/// `POST /v1/images/edits`.
pub async fn edit_images(
    State(state): State<AppState>,
    headers: HeaderMap,
    req: Request,
) -> Result<Response, ApiError> {
    let parsed_body = parse_image_request(req, state.upstream_client()).await?;
    validate_image_edit_body(&parsed_body)?;

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

fn json_value_to_form_string(v: &serde_json::Value) -> Option<String> {
    if let Some(s) = v.as_str() {
        Some(s.to_string())
    } else if let Some(n) = v.as_i64() {
        Some(n.to_string())
    } else if let Some(f) = v.as_f64() {
        Some(f.to_string())
    } else {
        v.as_bool().map(|b| b.to_string())
    }
}

async fn parse_image_json(
    json: serde_json::Value,
    upstream_client: &Arc<openproxy_adapters::UpstreamClient>,
) -> Result<ParsedImageMultipartBody, ApiError> {
    let model_name = json
        .get("model")
        .and_then(|v| v.as_str())
        .filter(|m| !m.is_empty())
        .unwrap_or("dall-e-2")
        .to_string();

    let mut files = Vec::new();
    let mut form_fields = Vec::new();

    if let Some(map) = json.as_object() {
        for (k, v) in map {
            if k == "image" || k == "mask" {
                if let Some(s) = v.as_str() {
                    files.push(resolve_image_input(s, k, upstream_client).await?);
                }
            } else if let Some(val_str) = json_value_to_form_string(v) {
                form_fields.push((k.clone(), val_str));
            }
        }
    }

    Ok(ParsedImageMultipartBody {
        model_name,
        files,
        form_fields,
    })
}

async fn parse_image_request(
    req: Request,
    upstream_client: &Arc<openproxy_adapters::UpstreamClient>,
) -> Result<ParsedImageMultipartBody, ApiError> {
    let is_multipart = req
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("multipart/form-data"));

    if is_multipart {
        let multipart = Multipart::from_request(req, &())
            .await
            .map_err(|e| ApiError(CoreError::Validation(format!("multipart extract: {e}"))))?;
        return parse_image_multipart(multipart).await;
    }

    let body_bytes = axum::body::to_bytes(req.into_body(), 64 * 1024 * 1024)
        .await
        .map_err(|e| ApiError(CoreError::Validation(format!("read body: {e}"))))?;

    let json: serde_json::Value = serde_json::from_slice(&body_bytes)
        .map_err(|e| ApiError(CoreError::Validation(format!("invalid json body: {e}"))))?;

    parse_image_json(json, upstream_client).await
}

fn png_multipart_file(field_name: &str, bytes: bytes::Bytes) -> MultipartFile {
    MultipartFile {
        name: field_name.to_string(),
        file_name: format!("{field_name}.png"),
        content_type: "image/png".to_string(),
        bytes,
    }
}

async fn fetch_remote_image(
    url: &str,
    upstream_client: &Arc<openproxy_adapters::UpstreamClient>,
) -> Result<bytes::Bytes, ApiError> {
    let uri: axum::http::Uri = url
        .parse()
        .map_err(|e| ApiError(CoreError::Validation(format!("invalid image URL: {e}"))))?;

    let host = uri
        .host()
        .ok_or_else(|| ApiError(CoreError::Validation("image URL must have a host".into())))?;

    let port = uri.port_u16().unwrap_or_else(|| {
        if uri.scheme_str() == Some("https") {
            443
        } else {
            80
        }
    });

    if port != 80 && port != 443 {
        return Err(ApiError(CoreError::Validation(
            "non-standard ports are not allowed".into(),
        )));
    }

    #[allow(clippy::collapsible_if)]
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        if openproxy_adapters::upstream::is_private_or_reserved(&ip) {
            return Err(ApiError(CoreError::Validation(
                "private or reserved IP addresses are not allowed".into(),
            )));
        }
    }

    let addrs = tokio::net::lookup_host((host, port)).await.map_err(|e| {
        ApiError(CoreError::Validation(format!(
            "failed to resolve image URL host: {e}"
        )))
    })?;

    for addr in addrs {
        if openproxy_adapters::upstream::is_private_or_reserved(&addr.ip()) {
            return Err(ApiError(CoreError::Validation(
                "private or reserved IP addresses are not allowed".into(),
            )));
        }
    }

    let req = openproxy_adapters::UpstreamRequest::get(url);
    let cancel = openproxy_adapters::CancellationToken::new();
    let resp = upstream_client
        .call(req, openproxy_adapters::TimeoutProfile::Chat, cancel)
        .await
        .map_err(|e| {
            ApiError(CoreError::UpstreamConnection(format!(
                "failed to fetch image URL: {e}"
            )))
        })?;
    resp.collect().await.map_err(|e| {
        ApiError(CoreError::UpstreamConnection(format!(
            "failed to read image URL body: {e}"
        )))
    })
}

fn decode_base64_image(raw: &str) -> Result<bytes::Bytes, ApiError> {
    use base64::Engine as _;
    let b64_clean = if raw.starts_with("data:image/")
        && let Some((_, b64_part)) = raw.split_once(";base64,")
    {
        b64_part.trim()
    } else {
        raw.trim()
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64_clean)
        .map_err(|e| {
            ApiError(CoreError::Validation(format!(
                "invalid base64 image data: {e}"
            )))
        })?;
    Ok(bytes.into())
}

async fn resolve_image_input(
    raw: &str,
    field_name: &str,
    upstream_client: &Arc<openproxy_adapters::UpstreamClient>,
) -> Result<MultipartFile, ApiError> {
    let trimmed = raw.trim();
    let bytes = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        fetch_remote_image(trimmed, upstream_client).await?
    } else {
        decode_base64_image(trimmed)?
    };
    Ok(png_multipart_file(field_name, bytes))
}

async fn process_multipart_field(
    field: axum::extract::multipart::Field<'_>,
    files: &mut Vec<MultipartFile>,
    form_fields: &mut Vec<(String, String)>,
    model_name: &mut String,
) {
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
        *model_name = field.text().await.unwrap_or_default();
    } else {
        let value = field.text().await.unwrap_or_default();
        form_fields.push((name, value));
    }
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
        process_multipart_field(field, &mut files, &mut form_fields, &mut model_name).await;
    }

    if model_name.is_empty() {
        model_name = form_fields
            .iter()
            .find(|(k, _)| k == "model")
            .map(|(_, v)| v.clone())
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| "dall-e-2".to_string());
    }

    Ok(ParsedImageMultipartBody {
        model_name,
        files,
        form_fields,
    })
}
