//! HTTP transport compression middleware.
//!
//! Provides automated gzip compression for static dashboard
//! assets (`/admin/dist/*`, styles, fonts) and JSON API endpoints (`/v1/models`,
//! `/admin/api/*`).
//!
//! Strictly bypasses Server-Sent Events (`text/event-stream`) to preserve zero-latency
//! chunk-to-chunk streaming for LLM completions.

use axum::http::{HeaderMap, Response, header};
use http_body::Body;
use tower_http::compression::CompressionLayer;
use tower_http::compression::predicate::Predicate;

/// Custom compression predicate enforcing strict bypass for `text/event-stream`
/// while compressing JSON, HTML, CSS, JS, SVG, and font assets.
#[derive(Clone, Copy, Debug, Default)]
pub struct TransportCompressionPredicate;

impl TransportCompressionPredicate {
    fn is_compressible_mime(content_type: &str) -> bool {
        let base_type = content_type
            .split(';')
            .next()
            .unwrap_or(content_type)
            .trim();

        if base_type.ends_with("+json") {
            return true;
        }

        if base_type.starts_with("font/") {
            return true;
        }

        if base_type.starts_with("application/font-") {
            return true;
        }

        matches!(
            base_type,
            "application/json"
                | "text/json"
                | "text/html"
                | "text/css"
                | "text/javascript"
                | "application/javascript"
                | "application/x-javascript"
                | "text/plain"
                | "text/markdown"
                | "image/svg+xml"
                | "application/wasm"
        )
    }

    #[inline]
    fn should_compress_headers(headers: &HeaderMap) -> bool {
        // Do not double-compress if already encoded
        if headers.contains_key(header::CONTENT_ENCODING) {
            return false;
        }

        let Some(content_type) = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
        else {
            // If Content-Type is absent, do not compress by default
            return false;
        };

        let content_type = content_type.trim().to_ascii_lowercase();

        // STRICT BYPASS: Server-Sent Events (SSE) must never be buffered/compressed
        if content_type.starts_with("text/event-stream") {
            return false;
        }

        Self::is_compressible_mime(&content_type)
    }
}

impl Predicate for TransportCompressionPredicate {
    fn should_compress<B>(&self, response: &Response<B>) -> bool
    where
        B: Body,
    {
        Self::should_compress_headers(response.headers())
    }
}

/// Creates a [`CompressionLayer`] configured with [`TransportCompressionPredicate`].
pub fn transport_compression_layer() -> CompressionLayer<TransportCompressionPredicate> {
    CompressionLayer::new().compress_when(TransportCompressionPredicate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{Response, StatusCode};
    use bytes::Bytes;
    use http_body_util::Empty;

    #[test]
    fn test_sse_strict_bypass() {
        let predicate = TransportCompressionPredicate;
        let response = Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream; charset=utf-8")
            .body(Empty::<Bytes>::new())
            .unwrap();

        assert!(!predicate.should_compress(&response));
    }

    #[test]
    fn test_json_compressed() {
        let predicate = TransportCompressionPredicate;
        let response = Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Empty::<Bytes>::new())
            .unwrap();

        assert!(predicate.should_compress(&response));
    }

    #[test]
    fn test_static_assets_compressed() {
        let predicate = TransportCompressionPredicate;

        let mime_types = [
            "text/html; charset=utf-8",
            "text/css",
            "application/javascript",
            "text/javascript",
            "image/svg+xml",
            "application/wasm",
            "font/woff2",
        ];

        for mime in mime_types {
            let response = Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime)
                .body(Empty::<Bytes>::new())
                .unwrap();

            assert!(
                predicate.should_compress(&response),
                "Expected {mime} to be compressed"
            );
        }
    }

    #[test]
    fn test_already_encoded_bypass() {
        let predicate = TransportCompressionPredicate;
        let response = Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::CONTENT_ENCODING, "gzip")
            .body(Empty::<Bytes>::new())
            .unwrap();

        assert!(!predicate.should_compress(&response));
    }
}
