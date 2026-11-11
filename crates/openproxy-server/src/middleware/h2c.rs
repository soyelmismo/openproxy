//! h2c downgrade middleware for JetBrains Runtime 25 clients (JBR25).
//!
//! JetBrains IDEs (IntelliJ, PyCharm, WebStorm, AI Assistant) speak HTTP/2
//! cleartext and open plain-HTTP connections with an RFC 7540 §3.2 upgrade
//! preamble: `Upgrade: h2c` + `HTTP2-Settings` + `Connection` carrying both
//! tokens. A server that neither answers `101 Switching Protocols` nor strips
//! the upgrade hop-by-hop headers leaves the JBR client in a broken state —
//! the request stalls or dies instead of falling back to HTTP/1.1.
//!
//! We never speak h2c ourselves. The fix is the documented fallback path:
//! detect the preamble, strip the upgrade hop-by-hop headers, and force
//! `connection: close` so the exchange completes as plain HTTP/1.1 and the
//! client reconnects per request. Behavior mirrors 9router's custom-server
//! downgrade (parity source: quangdang46/openproxy `src/core/h2c.rs`, MIT).
//!
//! Scope guard: only the literal `h2c` token is touched. WebSocket handshakes
//! (`Upgrade: websocket`) and every other upgrade target pass through
//! untouched, so `/admin/ws` live logs keep working.

use axum::{extract::Request, middleware::Next, response::Response};

/// True when an `Upgrade` header value requests `h2c` (case-insensitive,
/// trimmed). JBR25 sends exactly one token; a comma list is still honored by
/// comparing each token.
pub fn is_h2c_upgrade(upgrade_headers: &[&str]) -> bool {
    upgrade_headers
        .iter()
        .flat_map(|value| value.split(','))
        .any(|token| token.trim().eq_ignore_ascii_case("h2c"))
}

/// Drop `upgrade` and `http2-settings` tokens from a `Connection` list.
///
/// Returns `None` when nothing remains (the header must be deleted); the
/// fallback always appends `close` — the RFC 7540 §3.2 downgrade contract is
/// that the exchange finishes on this response and the client reconnects, so
/// a lingering keep-alive on a half-upgraded connection is never offered.
pub fn cleaned_connection_tokens(value: &str) -> Option<String> {
    let kept: Vec<&str> = value
        .split(',')
        .map(str::trim)
        .filter(|token| {
            !token.is_empty()
                && !token.eq_ignore_ascii_case("upgrade")
                && !token.eq_ignore_ascii_case("http2-settings")
        })
        .collect();
    if kept.iter().any(|token| token.eq_ignore_ascii_case("close")) {
        return Some("close".to_string());
    }
    Some(if kept.is_empty() {
        "close".to_string()
    } else {
        format!("{}, close", kept.join(", "))
    })
}

/// Axum middleware fn: see module docs.
pub async fn h2c_downgrade(mut req: Request, next: Next) -> Response {
    let upgrade_values: Vec<&str> = req
        .headers()
        .get_all(axum::http::header::UPGRADE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect();

    if is_h2c_upgrade(&upgrade_values) {
        let headers = req.headers_mut();
        headers.remove(axum::http::header::UPGRADE);
        headers.remove("http2-settings");
        match headers
            .get(axum::http::header::CONNECTION)
            .and_then(|v| v.to_str().ok())
        {
            Some(value) => {
                if let Some(cleaned) = cleaned_connection_tokens(value)
                    && let Ok(header_value) = axum::http::HeaderValue::from_str(&cleaned)
                {
                    headers.insert(axum::http::header::CONNECTION, header_value);
                }
            }
            None => {
                headers.insert(
                    axum::http::header::CONNECTION,
                    axum::http::HeaderValue::from_static("close"),
                );
            }
        }
    }

    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue};

    #[test]
    fn h2c_detected_case_insensitively() {
        assert!(is_h2c_upgrade(&["h2c"]));
        assert!(is_h2c_upgrade(&["H2C"]));
        assert!(is_h2c_upgrade(&["  h2c  "]));
        assert!(is_h2c_upgrade(&["websocket, h2c"]));
    }

    #[test]
    fn websocket_and_other_targets_not_detected() {
        assert!(!is_h2c_upgrade(&["websocket"]));
        assert!(!is_h2c_upgrade(&["HTTP/2.0"]));
        assert!(!is_h2c_upgrade(&[]));
    }

    #[test]
    fn connection_tokens_downgraded_to_close() {
        assert_eq!(
            cleaned_connection_tokens("Upgrade, HTTP2-Settings").as_deref(),
            Some("close")
        );
        assert_eq!(
            cleaned_connection_tokens("keep-alive").as_deref(),
            Some("keep-alive, close")
        );
        assert_eq!(
            cleaned_connection_tokens("Upgrade").as_deref(),
            Some("close")
        );
        assert_eq!(cleaned_connection_tokens("close").as_deref(), Some("close"));
    }

    /// Router wired exactly like production (`from_fn` layer) whose handler
    /// echoes the upgrade-relevant request headers back as response headers,
    /// so `oneshot` assertions see the post-middleware state.
    fn echo_router() -> axum::Router {
        axum::Router::new()
            .route(
                "/echo",
                axum::routing::any(|req: Request| async move {
                    let mut response = axum::response::Response::new(axum::body::Body::empty());
                    for (name, value) in req.headers() {
                        if matches!(name.as_str(), "upgrade" | "connection" | "http2-settings")
                            && let Ok(value) = axum::http::HeaderValue::from_bytes(value.as_bytes())
                        {
                            response.headers_mut().append(name, value);
                        }
                    }
                    response
                }),
            )
            .layer(axum::middleware::from_fn(super::h2c_downgrade))
    }

    async fn echoed_headers(request: axum::http::Request<axum::body::Body>) -> HeaderMap {
        use tower::ServiceExt as _;
        echo_router()
            .oneshot(request)
            .await
            .unwrap()
            .headers()
            .clone()
    }

    #[tokio::test]
    async fn h2c_request_has_upgrade_headers_stripped() {
        let seen = echoed_headers(
            Request::builder()
                .method(axum::http::Method::POST)
                .uri("/echo")
                .header("upgrade", "h2c")
                .header("http2-settings", "AAMAAABkAARAAAAAAAIAAAAA")
                .header("connection", "Upgrade, HTTP2-Settings")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;

        assert!(!seen.contains_key(axum::http::header::UPGRADE));
        assert!(!seen.contains_key("http2-settings"));
        assert_eq!(
            seen.get(axum::http::header::CONNECTION).unwrap(),
            HeaderValue::from_static("close")
        );
    }

    #[tokio::test]
    async fn h2c_connection_keeps_other_tokens_and_appends_close() {
        let seen = echoed_headers(
            Request::builder()
                .uri("/echo")
                .header("upgrade", "h2c")
                .header("connection", "keep-alive")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;

        assert!(!seen.contains_key(axum::http::header::UPGRADE));
        assert_eq!(
            seen.get(axum::http::header::CONNECTION).unwrap(),
            HeaderValue::from_static("keep-alive, close")
        );
    }

    #[tokio::test]
    async fn websocket_upgrade_passes_through_untouched() {
        let seen = echoed_headers(
            Request::builder()
                .uri("/echo")
                .header("upgrade", "websocket")
                .header("connection", "Upgrade")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(
            seen.get(axum::http::header::UPGRADE).unwrap(),
            HeaderValue::from_static("websocket")
        );
        assert_eq!(
            seen.get(axum::http::header::CONNECTION).unwrap(),
            HeaderValue::from_static("Upgrade")
        );
    }

    #[tokio::test]
    async fn plain_request_untouched() {
        let seen = echoed_headers(
            Request::builder()
                .uri("/echo")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;

        assert!(!seen.contains_key(axum::http::header::UPGRADE));
        assert!(!seen.contains_key(axum::http::header::CONNECTION));
    }
}
