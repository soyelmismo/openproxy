use axum::{
    extract::Request,
    middleware::Next,
    response::Response,
};
use axum::http::header;

pub async fn security_headers(req: Request, next: Next) -> Response {
    let mut response = next.run(req).await;

    let headers = response.headers_mut();

    // Strict-Transport-Security (HSTS)
    headers.insert(
        header::STRICT_TRANSPORT_SECURITY,
        header::HeaderValue::from_static("max-age=31536000; includeSubDomains"),
    );

    // Content-Security-Policy (CSP)
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        header::HeaderValue::from_static("default-src 'self'"),
    );

    // X-Frame-Options
    headers.insert(
        header::X_FRAME_OPTIONS,
        header::HeaderValue::from_static("DENY"),
    );

    // Cache-Control
    headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store, max-age=0"),
    );

    response
}
