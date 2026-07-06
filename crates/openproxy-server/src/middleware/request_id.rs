//! Request-ID middleware.
//!
//! On every incoming request:
//! 1. Read `x-request-id` from headers; if it parses as a UUID, adopt it.
//! 2. Otherwise mint a fresh [`RequestId`] (UUID v4).
//! 3. Insert the ID into the request's extension bag so handlers can pull
//!    it out without re-parsing headers.
//! 4. Echo the same value back in the response's `x-request-id` header.
//!
//! This is the first link in the per-request observability chain
//! (request_id + trace_id, per spec §1 and §11).

use axum::{extract::Request, http::HeaderValue, middleware::Next, response::Response};
use openproxy_core::ids::RequestId;
use uuid::Uuid;

/// Canonical header name, lowercase per HTTP/1.1 §8.1.2.
pub const X_REQUEST_ID: &str = "x-request-id";

/// Axum middleware fn: see module docs.
pub async fn request_id(mut req: Request, next: Next) -> Response {
    let id = req
        .headers()
        .get(X_REQUEST_ID)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| Uuid::parse_str(s).ok())
        .map(RequestId)
        .unwrap_or_else(RequestId::new);

    req.extensions_mut().insert(id);

    let mut response = next.run(req).await;

    // `RequestId`'s `Display` is the bare UUID; HeaderValue::from_str can
    // only fail on non-visible-ASCII / control chars, which a UUID never
    // contains, so the `from_static` fallback is unreachable in practice.
    let header_value = HeaderValue::from_str(&id.to_string())
        .unwrap_or_else(|_| HeaderValue::from_static("invalid"));
    response.headers_mut().insert(X_REQUEST_ID, header_value);

    response
}
