//! Request helper functions and builder methods for UpstreamRequest.

use bytes::Bytes;
use http::{HeaderMap, Method};

use super::client::UpstreamRequest;

/// Build a formatted `"Bearer <token>"` string.
pub fn format_bearer(token: &str) -> String {
    format!("Bearer {token}")
}

/// Build an `("Authorization", "Bearer <token>")` tuple.
pub fn bearer_auth_header(token: &str) -> (String, String) {
    ("Authorization".into(), format_bearer(token))
}

/// Build an `http::HeaderValue` for a Bearer token.
pub fn bearer_header_value(
    token: &str,
) -> Result<http::HeaderValue, http::header::InvalidHeaderValue> {
    http::HeaderValue::from_str(&format_bearer(token))
}

impl UpstreamRequest {
    /// Build a simple GET with no headers / body.
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            method: Method::GET,
            url: url.into(),
            headers: HeaderMap::new(),
            body: None,
            is_streaming: true,
            proxy: None,
            proxy_status: None,
        }
    }

    /// Build a GET request with an `Accept: application/json` header.
    pub fn get_json(url: impl Into<String>) -> Self {
        let mut req = Self::get(url);
        req.headers.insert(
            http::header::ACCEPT,
            http::HeaderValue::from_static("application/json"),
        );
        req
    }

    /// Add an `Accept: application/json` header in a builder style.
    pub fn with_accept_json(mut self) -> Self {
        self.headers.insert(
            http::header::ACCEPT,
            http::HeaderValue::from_static("application/json"),
        );
        self
    }

    /// Add an `Authorization: Bearer <token>` header in a builder style.
    pub fn with_bearer(mut self, token: &str) -> Self {
        if let Ok(hv) = bearer_header_value(token) {
            self.headers.insert(http::header::AUTHORIZATION, hv);
        }
        self
    }

    /// Add a custom header in a builder style.
    pub fn with_header(mut self, name: http::HeaderName, value: http::HeaderValue) -> Self {
        self.headers.insert(name, value);
        self
    }

    /// Build a simple DELETE with no headers / body.
    pub fn delete(url: impl Into<String>) -> Self {
        Self {
            method: Method::DELETE,
            url: url.into(),
            headers: HeaderMap::new(),
            body: None,
            is_streaming: true,
            proxy: None,
            proxy_status: None,
        }
    }

    /// Build a POST with a JSON body and a `Content-Type: application/json` header.
    pub fn post_json(url: impl Into<String>, body: Bytes) -> Self {
        let mut headers = HeaderMap::with_capacity(1);
        headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        Self {
            method: Method::POST,
            url: url.into(),
            headers,
            body: Some(body),
            is_streaming: true,
            proxy: None,
            proxy_status: None,
        }
    }

    /// A POST with a caller-built multipart body. `content_type` must carry
    /// the boundary.
    pub fn post_multipart(url: impl Into<String>, content_type: &str, body: Bytes) -> Self {
        let mut headers = HeaderMap::with_capacity(1);
        headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_str(content_type)
                .unwrap_or_else(|_| http::HeaderValue::from_static("multipart/form-data")),
        );
        Self {
            method: Method::POST,
            url: url.into(),
            headers,
            body: Some(body),
            is_streaming: false,
            proxy: None,
            proxy_status: None,
        }
    }
}
