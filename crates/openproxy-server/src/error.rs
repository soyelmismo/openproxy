//! HTTP error mapping.
//!
//! Wraps a [`CoreError`] so handler return types stay `Result<T, ApiError>`,
//! and turns the error into a JSON `{"error": {"code", "message"}}` response
//! with the appropriate HTTP status code (per spec §2 and [`CoreError::http_status`]).

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use openproxy_types::CoreError;
use serde_json::json;

/// Wrapper around [`CoreError`] that adapts a typed error into the
/// `{"error": {"code", "message"}}` JSON envelope for the client.
///
/// Use `.into()` / `?` to lift a `CoreError` into an [`ApiError`]; both
/// paths go through the [`From<CoreError>`] impl below.
pub struct ApiError(pub CoreError);

impl From<CoreError> for ApiError {
    fn from(err: CoreError) -> Self {
        Self(err)
    }
}

impl From<tokio::task::JoinError> for ApiError {
    fn from(err: tokio::task::JoinError) -> Self {
        ApiError(CoreError::Internal(err.to_string()))
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::fmt::Debug for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ApiError").field(&self.0).finish()
    }
}

impl ApiError {
    /// Return the error message sanitized (secrets redacted) and truncated to length limit.
    pub fn sanitized_message(&self) -> String {
        let raw = self.0.to_string();
        let redacted = openproxy_core::cost::redact_error_msg(&raw);
        truncate_error_message(&redacted.0)
    }

    /// Render this error as a pre-formatted SSE frame (`Bytes`).
    ///
    /// - Anthropic format: `event: error\ndata: {"type":"error","error":{"type":...,"message":...}}\n\n`
    /// - OpenAI / default format: `data: {"error":{"message":...,"type":...,"code":...}}\n\n`
    pub fn to_sse_error_frame(&self, format: openproxy_types::TargetFormat) -> bytes::Bytes {
        let message = self.sanitized_message();
        let error_str = match format {
            openproxy_types::TargetFormat::Anthropic => {
                let error_json = serde_json::json!({
                    "type": "error",
                    "error": {
                        "type": self.0.code(),
                        "message": message,
                    }
                });
                serde_json::to_string(&error_json).unwrap_or_else(|_| {
                    r#"{"type":"error","error":{"type":"internal_error","message":"Internal server error"}}"#.to_string()
                })
            }
            openproxy_types::TargetFormat::Openai
            | openproxy_types::TargetFormat::Gemini
            | openproxy_types::TargetFormat::Responses
            | openproxy_types::TargetFormat::Atomesus => {
                let error_json = serde_json::json!({
                    "error": {
                        "message": message,
                        "type": self.0.code(),
                        "code": self.0.http_status(),
                    }
                });
                serde_json::to_string(&error_json).unwrap_or_else(|_| {
                    r#"{"error":{"message":"Internal server error","type":"internal_error","code":500}}"#.to_string()
                })
            }
        };

        let mut frame = bytes::BytesMut::with_capacity(error_str.len() + 16);
        match format {
            openproxy_types::TargetFormat::Anthropic => {
                frame.extend_from_slice(b"event: error\ndata: ");
            }
            openproxy_types::TargetFormat::Openai
            | openproxy_types::TargetFormat::Gemini
            | openproxy_types::TargetFormat::Responses
            | openproxy_types::TargetFormat::Atomesus => {
                frame.extend_from_slice(b"data: ");
            }
        }
        frame.extend_from_slice(error_str.as_bytes());
        frame.extend_from_slice(b"\n\n");
        frame.freeze()
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.0.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let message = self.sanitized_message();
        let body = json!({
            "error": {
                "code": self.0.code(),
                "message": message,
            }
        });
        (status, Json(body)).into_response()
    }
}

/// Maximum length, in bytes, of the `error.message` we ship back to
/// our client. Matches the `redact_error_msg` cap used for the DB
/// (`cost.rs`), so the API response and the persisted row never
/// disagree on how big an error message can be.
const API_ERROR_MESSAGE_MAX: usize = 2048;

pub(crate) fn truncate_error_message(raw: &str) -> String {
    if raw.len() <= API_ERROR_MESSAGE_MAX {
        return raw.to_string();
    }
    // Walk back to a valid UTF-8 boundary so we never slice a code
    // point in half. `is_char_boundary` is O(1) so this stays cheap.
    let mut idx = API_ERROR_MESSAGE_MAX;
    while idx > 0 && !raw.is_char_boundary(idx) {
        idx -= 1;
    }
    let mut out = String::with_capacity(idx + "...[truncated]".len());
    out.push_str(&raw[..idx]);
    out.push_str("...[truncated]");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_error_message_returns_short_strings_unchanged() {
        let s = "upstream error: status=503 body=Service Unavailable";
        assert_eq!(truncate_error_message(s), s);
    }

    #[test]
    fn truncate_error_message_caps_long_strings() {
        // 1 MiB of garbage simulating a verbose upstream body.
        let huge = "x".repeat(1024 * 1024);
        let out = truncate_error_message(&huge);
        assert!(
            out.len() <= API_ERROR_MESSAGE_MAX + "...[truncated]".len(),
            "output len {} exceeds cap {}",
            out.len(),
            API_ERROR_MESSAGE_MAX + "...[truncated]".len()
        );
        assert!(out.ends_with("...[truncated]"));
    }

    #[test]
    fn truncate_error_message_respects_utf8_boundaries() {
        // Multi-byte chars at the cap boundary. The truncation must
        // land on a char boundary, not split a code point.
        let mut s = String::new();
        while s.len() < API_ERROR_MESSAGE_MAX + 10 {
            s.push('\u{2603}'); // 3-byte snowman
        }
        let out = truncate_error_message(&s);
        assert!(out.ends_with("...[truncated]"));
        // Round-trip via std::str to verify we did not produce invalid UTF-8.
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    #[test]
    fn to_sse_error_frame_openai_format() {
        let err = ApiError(CoreError::Validation(
            "bad key sk-abcdef1234567890abcdef".into(),
        ));
        let frame = err.to_sse_error_frame(openproxy_types::TargetFormat::Openai);
        let frame_str = std::str::from_utf8(&frame).unwrap();

        assert!(frame_str.starts_with("data: "));
        assert!(frame_str.ends_with("\n\n"));
        assert!(!frame_str.contains("sk-abcdef1234567890abcdef"));
        assert!(frame_str.contains("[REDACTED]"));
        assert!(frame_str.contains("\"type\":\"validation\""));
    }

    #[test]
    fn to_sse_error_frame_anthropic_format() {
        let err = ApiError(CoreError::Validation(
            "bad key Authorization: Bearer secret_token_12345".into(),
        ));
        let frame = err.to_sse_error_frame(openproxy_types::TargetFormat::Anthropic);
        let frame_str = std::str::from_utf8(&frame).unwrap();

        assert!(frame_str.starts_with("event: error\ndata: "));
        assert!(frame_str.ends_with("\n\n"));
        assert!(!frame_str.contains("secret_token_12345"));
        assert!(frame_str.contains("[REDACTED]"));
        assert!(frame_str.contains("\"type\":\"error\""));
    }
}
