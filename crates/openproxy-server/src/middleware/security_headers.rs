//! Browser security headers (CSP, `X-Frame-Options`, `nosniff`).
//!
//! Applied on the outermost layer so every response — public API, admin
//! API, SPA shell and static assets — carries the same policy.
//!
//! ## Content-Security-Policy rationale
//!
//! The dashboard renders LLM output through `unsafeHTML` (Markdown/LaTeX).
//! The renderer escapes everything (see `web/src/static/src/lib/markdown.ts`),
//! but CSP is the second line of defence if a future regression lets
//! markup through:
//!
//! * `script-src 'self'` — explicit (not just inherited from `default-src`)
//!   so inline handlers (`onerror=`) and `javascript:` never execute.
//! * `style-src-elem 'self'` — an injected `<style>` element is inert, which
//!   closes the CSS-only clickjacking/redressing vector. The SPA bundles
//!   every stylesheet (including uPlot's) into `/admin/dist/app.css`.
//! * `style-src-attr 'unsafe-inline'` — lit-html templates use hundreds of
//!   `style="…"` attributes; an attribute cannot load external resources or
//!   overlay other elements the way a `<style>` block can, so this stays
//!   allowed. `style-src` keeps `'unsafe-inline'` only as the fallback for
//!   browsers without `-elem`/`-attr` support.
//! * `connect-src 'self' ws://<host> wss://<host>` — the live-logs socket
//!   is same-origin. Listing the scheme+host explicitly (instead of bare
//!   `ws:`/`wss:`) means injected script could not exfiltrate to an
//!   attacker-controlled WebSocket endpoint. `<host>` is the request's
//!   `Host` header, validated to a conservative character set.
//! * `img-src` lists the two favicon fallbacks the provider cards use;
//!   Google's `s2/favicons` answers with a redirect to `*.gstatic.com`, so
//!   that host must be allowed too or the fallback never renders.
//! * `object-src 'none'`, `base-uri 'self'`, `frame-ancestors 'none'` —
//!   standard hardening; `frame-ancestors` mirrors `X-Frame-Options: DENY`.

use axum::{
    extract::{Request, State},
    http::{HeaderValue, header},
    middleware::Next,
    response::Response,
};

use crate::state::AppState;

/// Policy directives that do not depend on the request.
const CSP_STATIC: &str = "default-src 'self'; \
script-src 'self'; \
object-src 'none'; \
base-uri 'self'; \
frame-ancestors 'none'; \
img-src 'self' data: blob: https://www.google.com https://*.gstatic.com https://icons.duckduckgo.com; \
style-src 'self' 'unsafe-inline'; \
style-src-elem 'self'; \
style-src-attr 'unsafe-inline'; \
media-src 'self' blob:; \
connect-src 'self'";

/// Accept only `host[:port]` made of characters that are valid in a CSP
/// host-source. Anything else (spaces, `;`, quotes, …) is dropped so a
/// hostile `Host` header cannot inject directives.
fn sanitized_host(req: &Request) -> Option<&str> {
    let host = req.headers().get(header::HOST)?.to_str().ok()?.trim();
    let ok = !host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']'));
    ok.then_some(host)
}

/// Build the full CSP string for a request.
pub(crate) fn build_csp(host: Option<&str>) -> String {
    match host {
        Some(h) => format!("{CSP_STATIC} ws://{h} wss://{h};"),
        None => format!("{CSP_STATIC};"),
    }
}

/// Axum middleware fn: see module docs.
pub async fn security_headers(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let csp = build_csp(sanitized_host(&req));
    // Security (OP-21): HSTS is only meaningful when the deployment actually
    // serves TLS. The binary itself is plain HTTP, so emit the header only
    // when the request demonstrably arrived over HTTPS through a TRUSTED
    // reverse proxy (`X-Forwarded-Proto: https` from a trusted peer) — an
    // untrusted client must not be able to pin HSTS for other users.
    let served_over_tls = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0)
        .is_some_and(|peer| {
            crate::client_ip::is_trusted_proxy(
                peer.ip(),
                &state.config().server.trusted_proxies,
            )
        })
        && req
            .headers()
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("https"));
    let mut response = next.run(req).await;
    let headers = response.headers_mut();
    if served_over_tls {
        headers.insert(
            header::STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static("max-age=31536000; includeSubDomains"),
        );
    }
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    // `build_csp` only interpolates a host that passed `sanitized_host`,
    // so the value is always visible ASCII; fall back to the static
    // policy rather than emitting no CSP at all.
    let csp_value = HeaderValue::from_str(&csp).unwrap_or_else(|_| {
        HeaderValue::from_str(&build_csp(None))
            .unwrap_or_else(|_| HeaderValue::from_static("default-src 'self'"))
    });
    headers.insert(header::CONTENT_SECURITY_POLICY, csp_value);
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req_with_host(host: Option<&str>) -> Request {
        let mut b = Request::builder().uri("/admin");
        if let Some(h) = host {
            b = b.header(header::HOST, h);
        }
        b.body(axum::body::Body::empty()).unwrap()
    }

    #[test]
    fn csp_pins_websocket_origin_to_request_host() {
        let csp = build_csp(sanitized_host(&req_with_host(Some("listo.click"))));
        assert!(csp.ends_with("connect-src 'self' ws://listo.click wss://listo.click;"));
        assert!(csp.contains("script-src 'self';"));
        assert!(csp.contains("style-src-elem 'self';"));
        assert!(csp.contains("style-src-attr 'unsafe-inline';"));
        assert!(csp.contains("object-src 'none';"));
        assert!(csp.contains("frame-ancestors 'none';"));
        assert!(!csp.contains(" ws: "), "bare scheme sources must be gone");
    }

    #[test]
    fn csp_keeps_port_and_ipv6_hosts() {
        assert!(
            build_csp(sanitized_host(&req_with_host(Some("127.0.0.1:8787"))))
                .contains("wss://127.0.0.1:8787;")
        );
        assert!(
            build_csp(sanitized_host(&req_with_host(Some("[::1]:8787"))))
                .contains("ws://[::1]:8787 ")
        );
    }

    #[test]
    fn hostile_host_header_cannot_inject_directives() {
        for bad in [
            "evil.com; script-src 'unsafe-inline'",
            "a b",
            "x'y",
            "",
            "   ",
        ] {
            let csp = build_csp(sanitized_host(&req_with_host(Some(bad))));
            assert!(
                csp.ends_with("connect-src 'self';"),
                "host {bad:?} must be dropped, got {csp}"
            );
        }
        assert!(build_csp(sanitized_host(&req_with_host(None))).ends_with("connect-src 'self';"));
    }

    #[tokio::test]
    async fn hsts_only_when_tls_via_trusted_proxy() {
        use axum::body::Body;
        use axum::http::StatusCode;
        use tower::ServiceExt;

        async fn ok_handler() -> &'static str {
            "ok"
        }
        let app = axum::Router::new()
            .route("/x", axum::routing::get(ok_handler))
            .layer(axum::middleware::from_fn(security_headers_test_shim));

        // Plain HTTP (no X-Forwarded-Proto): no HSTS.
        let resp = app
            .clone()
            .oneshot(Request::builder().uri("/x").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            resp.headers().get("strict-transport-security").is_none(),
            "no HSTS on plain HTTP"
        );

        // x-forwarded-proto: https from a loopback peer (trusted): HSTS.
        let mut req = Request::builder()
            .uri("/x")
            .header("x-forwarded-proto", "https")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(
            axum::extract::ConnectInfo(std::net::SocketAddr::from(([127, 0, 0, 1], 44444))),
        );
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.headers().get("strict-transport-security").unwrap(),
            "max-age=31536000; includeSubDomains"
        );
    }

    /// Test shim standing in for the state-aware middleware: same logic,
    /// fixed empty trusted-proxies list (loopback trusted by default).
    async fn security_headers_test_shim(req: Request, next: Next) -> Response {
        let served_over_tls = req
            .extensions()
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            .map(|ci| ci.0)
            .is_some_and(|peer| {
                crate::client_ip::is_trusted_proxy(peer.ip(), &["127.0.0.1".into()])
            })
            && req
                .headers()
                .get("x-forwarded-proto")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.eq_ignore_ascii_case("https"));
        let mut response = next.run(req).await;
        if served_over_tls {
            response.headers_mut().insert(
                header::STRICT_TRANSPORT_SECURITY,
                HeaderValue::from_static("max-age=31536000; includeSubDomains"),
            );
        }
        response
    }
}
