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
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct ApiError(pub CoreError);

impl From<CoreError> for ApiError {
    fn from(err: CoreError) -> Self {
        Self(err)
    }
}

/// Execute synchronous administrative work without blocking a Tokio worker.
pub(crate) async fn run_blocking<T, F>(operation: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ApiError> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| {
            ApiError(CoreError::Internal(format!(
                "blocking task failed: {error}"
            )))
        })?
}

impl ApiError {
    /// Return the error message sanitized (secrets redacted) and truncated to length limit.
    pub fn sanitized_message(&self) -> String {
        match &self.0 {
            CoreError::Internal(_)
            | CoreError::Database { .. }
            | CoreError::Migration { .. }
            | CoreError::Config(_) => {
                tracing::error!(error = %self.0, "internal server error");
                "Internal server error".to_string()
            }
            _ => {
                let raw = self.0.to_string();
                let redacted = openproxy_core::cost::redact_error_msg(&raw);
                truncate_error_message(&redacted.0).into_owned()
            }
        }
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
            openproxy_types::TargetFormat::Responses => {
                let error_json = serde_json::json!({
                    "type": "response.failed",
                    "response": {
                        "status": "failed",
                        "error": {
                            "code": self.0.code(),
                            "message": message,
                        }
                    }
                });
                serde_json::to_string(&error_json).unwrap_or_else(|_| {
                    r#"{"type":"response.failed","response":{"status":"failed","error":{"code":"internal_error","message":"Internal server error"}}}"#.to_string()
                })
            }
            openproxy_types::TargetFormat::Openai
            | openproxy_types::TargetFormat::Gemini
            | openproxy_types::TargetFormat::Atomesus
            | openproxy_types::TargetFormat::CommandCodeGo
            | openproxy_types::TargetFormat::Devin
            | openproxy_types::TargetFormat::SystemOne => {
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

        let mut frame = bytes::BytesMut::with_capacity(error_str.len() + 32);
        match format {
            openproxy_types::TargetFormat::Anthropic => {
                frame.extend_from_slice(b"event: error\ndata: ");
            }
            openproxy_types::TargetFormat::Responses => {
                frame.extend_from_slice(b"event: response.failed\ndata: ");
            }
            openproxy_types::TargetFormat::Openai
            | openproxy_types::TargetFormat::Gemini
            | openproxy_types::TargetFormat::Atomesus
            | openproxy_types::TargetFormat::CommandCodeGo
            | openproxy_types::TargetFormat::Devin
            | openproxy_types::TargetFormat::SystemOne => {
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
        let mut response = (status, Json(body)).into_response();
        // A rate-limited client must be told when to come back, like any
        // generic rate limiter: the quota path carries the window's reset
        // instant, the upstream path its retry hint.
        if let Some(retry_after) = retry_after_value(&self.0) {
            if let Ok(value) = axum::http::HeaderValue::from_str(&retry_after) {
                response.headers_mut().insert(axum::http::header::RETRY_AFTER, value);
            }
        }
        response
    }
}

/// The `Retry-After` a rate-limited client must honor, in whole seconds.
///
/// - `QuotaExceeded` (the key's 5h/weekly window): seconds until `resets_at`.
///   The rolling-window fallback ("rolling window") has no instant, so no
///   header goes out.
/// - `RateLimited` (upstream pressure): the provider's retry hint in ms.
fn retry_after_value(err: &CoreError) -> Option<String> {
    match err {
        CoreError::QuotaExceeded { resets_at, .. } => {
            let instant = chrono::DateTime::parse_from_rfc3339(resets_at)
                .or_else(|_| {
                    chrono::NaiveDateTime::parse_from_str(resets_at, "%Y-%m-%d %H:%M:%S")
                        .map(|naive| naive.and_utc().fixed_offset())
                })
                .ok()?;
            let secs = instant.timestamp() - chrono::Utc::now().timestamp();
            Some(secs.max(1).to_string())
        }
        CoreError::RateLimited { retry_after_ms, .. } => {
            Some((*retry_after_ms).div_ceil(1000).max(1).to_string())
        }
        _ => None,
    }
}

/// Maximum length, in bytes, of the `error.message` returned to the client.
/// Matches the `redact_error_msg` cap in `cost.rs` so the API response and the
/// persisted row never disagree on message size.
const API_ERROR_MESSAGE_MAX: usize = 2048;

pub(crate) fn truncate_error_message(raw: &str) -> std::borrow::Cow<'_, str> {
    if raw.len() <= API_ERROR_MESSAGE_MAX {
        return std::borrow::Cow::Borrowed(raw);
    }
    // Walk back to a UTF-8 boundary so no code point is split (O(1) per check).
    let mut idx = API_ERROR_MESSAGE_MAX;
    while idx > 0 && !raw.is_char_boundary(idx) {
        idx -= 1;
    }
    let mut out = String::with_capacity(idx + "...[truncated]".len());
    out.push_str(&raw[..idx]);
    out.push_str("...[truncated]");
    std::borrow::Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_exceeded_sets_the_retry_after_header() {
        let err = ApiError(CoreError::QuotaExceeded {
            window: "5h".into(),
            spent_usd: 2.0,
            cap_usd: 2.0,
            resets_at: (chrono::Utc::now() + chrono::Duration::seconds(300)).to_rfc3339(),
        });
        let response = err.into_response();
        assert_eq!(response.status().as_u16(), 429);
        let header = response
            .headers()
            .get(axum::http::header::RETRY_AFTER)
            .expect("retry-after")
            .to_str()
            .expect("ascii");
        let secs: u64 = header.parse().expect("numeric retry-after");
        assert!((250..=300).contains(&secs), "seconds until reset, got {secs}");
    }

    #[test]
    fn rate_limited_carries_the_upstream_retry_hint() {
        let err = ApiError(CoreError::RateLimited {
            provider: "mock".into(),
            retry_after_ms: 1500,
            is_proxy_rotated: false,
        });
        let response = err.into_response();
        assert_eq!(response.status().as_u16(), 429);
        let header = response
            .headers()
            .get(axum::http::header::RETRY_AFTER)
            .expect("retry-after")
            .to_str()
            .expect("ascii");
        assert_eq!(header, "2", "1500ms rounds up to 2s");
    }

    #[test]
    fn insufficient_credits_maps_to_402_with_the_buy_hint() {
        let err = ApiError(CoreError::InsufficientCredits { balance_usd: 0.0 });
        let message = err.sanitized_message();
        let response = err.into_response();
        assert_eq!(response.status().as_u16(), 402);
        assert!(message.contains("/checkout"));
    }

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
        // Multi-byte chars straddling the cap: the cut must land on a char
        // boundary, never mid code point.
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

    #[test]
    fn test_sanitized_message_internal_errors() {
        let err = ApiError(CoreError::Internal("stack trace here".into()));
        assert_eq!(err.sanitized_message(), "Internal server error");

        let err = ApiError(CoreError::Database {
            message: "db timeout".into(),
            source: None,
        });
        assert_eq!(err.sanitized_message(), "Internal server error");

        let err = ApiError(CoreError::Migration {
            version: 1,
            message: "migration failed".into(),
        });
        assert_eq!(err.sanitized_message(), "Internal server error");

        let err = ApiError(CoreError::Config("bad config".into()));
        assert_eq!(err.sanitized_message(), "Internal server error");
    }
}
