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
        let body = json!({
            "error": {
                "code": self.0.code(),
                "message": self.0.to_string(),
            }
        });
        (status, Json(body)).into_response()
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
