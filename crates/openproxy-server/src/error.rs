//! HTTP error mapping.
//!
//! Wraps a [`CoreError`] so handler return types stay `ApiResult<T>`,
//! and turns the error into a JSON `{"error": {"code", "message"}}` response
//! with the appropriate HTTP status code (per spec §2 and [`CoreError::http_status`]).

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use openproxy_core::CoreError;
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

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.0.http_status())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        // LOW fix: cap the serialized error message so an upstream
        // returning a multi-MiB HTML error page, a Python traceback,
        // or any other verbose body does NOT get amplified into the
        // JSON response we ship back to our own client. The cap is
        // applied at the HTTP boundary, AFTER `Display` produces the
        // full string, so all the existing error variants are covered
        // (validation messages, database messages, upstream bodies)
        // without having to add a cap to each variant.
        //
        // MEDIUM-2 fix: also run the error message through
        // `redact_error_msg` before truncating. This strips patterns
        // like `sk-...`, `x-api-key: ...`, `Authorization: Bearer ...`
        // that upstream proxies might echo in their error responses.
        // The DB-persisted form has always been redacted (cost.rs);
        // now the live HTTP response is too.
        let raw = self.0.to_string();
        let redacted = openproxy_core::cost::redact_error_msg(&raw);
        let message = truncate_error_message(&redacted.0);
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

fn truncate_error_message(raw: &str) -> String {
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
}

// =====================================================================
// ApiResult: handler return type
// =====================================================================
//
// Axum 0.7's blanket `IntoResponse for Result<T, _>` only covers the
// *default* error type (`axum_core::response::ErrorResponse`). We can't
// add a generic `impl IntoResponse for Result<T, ApiError>` because the
// orphan rules require the local type (`ApiError`) to appear *before*
// the uncovered type parameter `T` — and `Result`'s parameter order is
// `Result<T, E>`, so the foreign `T` comes first.
//
// The standard workaround is a local newtype. `ApiResult<T>` wraps
// `Result<T, ApiError>`; the wrapper is local so the orphan rules are
// happy, and we can implement `IntoResponse` on it directly. The
// downside is that the `?` operator requires stable `Try`, which
// Rust 1.96 still gates behind `try_trait_v2`; the patterns below
// (`ApiResult::ok`, `ApiResult::err`, or an inner `Result<T, ApiError>`
// unwrapped at the end) cover the common cases without `?`.

/// Newtype around `Result<T, ApiError>` returned by every handler.
#[derive(Debug)]
pub struct ApiResult<T>(Result<T, ApiError>);

impl<T> ApiResult<T> {
    /// Wrap a successful value.
    pub fn ok(value: T) -> Self {
        Self(Ok(value))
    }

    /// Wrap an error value.
    pub fn err(err: ApiError) -> Self {
        Self(Err(err))
    }

    /// Unwrap into the inner `Result` for use with the `?` operator
    /// inside an IIFE — see the chat handler for the canonical pattern.
    pub fn into_inner(self) -> Result<T, ApiError> {
        self.0
    }
}

impl<T> IntoResponse for ApiResult<T>
where
    T: IntoResponse,
{
    fn into_response(self) -> Response {
        match self.0 {
            Ok(value) => value.into_response(),
            Err(err) => err.into_response(),
        }
    }
}

impl<T> From<Result<T, ApiError>> for ApiResult<T> {
    fn from(r: Result<T, ApiError>) -> Self {
        Self(r)
    }
}

impl<T> From<ApiError> for ApiResult<T> {
    fn from(err: ApiError) -> Self {
        Self(Err(err))
    }
}
