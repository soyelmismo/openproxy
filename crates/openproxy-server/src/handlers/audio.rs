//! `POST /v1/audio/transcriptions` — OpenAI-compatible Whisper endpoint.
//!
//! This is a *standalone* handler that delegates resolution, dispatch,
//! and usage recording to [`openproxy_core::audio::execute_transcribe`].

use axum::{
    extract::{Multipart, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::Response,
};
use openproxy_core::audio::{ParsedAudioBody, execute_transcribe};
use openproxy_types::CoreError;

use crate::{error::ApiError, middleware::auth::authenticate_and_authorize_model, state::AppState};

pub fn router() -> axum::Router<AppState> {
    axum::Router::new().route("/transcriptions", axum::routing::post(transcribe))
}

/// `POST /v1/audio/transcriptions`.
pub async fn transcribe(
    State(state): State<AppState>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Result<Response, ApiError> {
    // 1. Parse the multipart body.
    let parsed_body = parse_multipart_body(multipart).await?;

    // 2. Authenticate & authorize model (chat scope + combo check).
    let api_key_id = authenticate_and_authorize_model(&state, &headers, &parsed_body.model_name)?;

    // 3. Delegate resolution, multi-target dispatch, and usage recording to core::audio.
    let response = call_unary_executor!(execute_transcribe, state, parsed_body, api_key_id);

    Ok(build_audio_response(
        response.status_code,
        &response.content_type,
        response.body_bytes,
    ))
}

async fn parse_multipart_body(mut multipart: Multipart) -> Result<ParsedAudioBody, ApiError> {
    let mut model_name = String::new();
    let mut file_bytes: Option<bytes::Bytes> = None;
    let mut file_name = String::from("audio");
    let mut file_content_type = String::from("application/octet-stream");
    let mut form_fields: Vec<(String, String)> = Vec::new();

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError(CoreError::Validation(format!("multipart parse: {e}"))))?
    {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "model" => {
                model_name = field.text().await.unwrap_or_default();
            }
            "file" => {
                file_name = field.file_name().unwrap_or("audio").to_string();
                file_content_type = field
                    .content_type()
                    .unwrap_or("application/octet-stream")
                    .to_string();
                file_bytes = Some(field.bytes().await.unwrap_or_default());
            }
            _ => {
                let value = field.text().await.unwrap_or_default();
                form_fields.push((name, value));
            }
        }
    }

    let file_bytes = file_bytes.ok_or_else(|| {
        ApiError(CoreError::Validation(
            "missing 'file' part in multipart body".into(),
        ))
    })?;
    if file_bytes.is_empty() {
        return Err(ApiError(CoreError::Validation(
            "empty 'file' part in multipart body".into(),
        )));
    }
    if model_name.is_empty() {
        return Err(ApiError(CoreError::Validation(
            "missing 'model' field in multipart body".into(),
        )));
    }

    Ok(ParsedAudioBody {
        model_name,
        file_bytes,
        file_name,
        file_content_type,
        form_fields,
    })
}

fn build_audio_response(status_code: u16, content_type: &str, body: bytes::Bytes) -> Response {
    let mut builder = Response::builder()
        .status(StatusCode::from_u16(status_code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR));
    if let Ok(v) = HeaderValue::from_str(content_type) {
        builder = builder.header(axum::http::header::CONTENT_TYPE, v);
    }
    builder
        .body(axum::body::Body::from(body))
        .unwrap_or_else(|_| {
            let mut res = Response::new(axum::body::Body::empty());
            *res.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            res
        })
}
