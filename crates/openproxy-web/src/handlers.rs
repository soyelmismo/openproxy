use axum::{
    body::Body,
    extract::State,
    http::{HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Json, Response},
};
use std::path::PathBuf;

use crate::WebState;

pub async fn index_html() -> Html<&'static str> {
    Html(include_str!("static/index.html"))
}

pub async fn web_health(State(s): State<WebState>) -> impl IntoResponse {
    match s.core_client.health().await {
        Ok(v) => Json(serde_json::json!({"status": "ok", "core": v})).into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "core unreachable");
            Json(serde_json::json!({"status": "degraded", "error": e.to_string()})).into_response()
        }
    }
}

pub async fn callback_html() -> Html<&'static str> {
    Html(include_str!("static/callback.html"))
}

/// Static fallback for the refactored frontend. Maps a request
/// path to `crates/openproxy-web/src/static/...` and streams the
/// file with the appropriate Content-Type. This is the minimal
/// addition needed so the new ES-modules entrypoint under
/// `/src/app.js` and the CSS bundle under `/styles/index.css`
/// can be served without a bundler. The `static/` directory is
/// the same folder the legacy `app.js` / `styles.css` lived in,
/// so old code paths keep working until they are deleted.
pub async fn serve_static(uri: axum::http::Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    // Block path traversal — anything with ".." gets rejected.
    if path.is_empty() || path.contains("..") || path.starts_with('/') {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    let fs_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("static")
        .join(path);
    match tokio::fs::read(&fs_path).await {
        Ok(bytes) => {
            let mime = match fs_path.extension().and_then(|s| s.to_str()) {
                Some("js") | Some("mjs") => "application/javascript; charset=utf-8",
                Some("css") => "text/css; charset=utf-8",
                Some("html") => "text/html; charset=utf-8",
                Some("json") => "application/json; charset=utf-8",
                Some("svg") => "image/svg+xml",
                Some("png") => "image/png",
                Some("ico") => "image/x-icon",
                _ => "application/octet-stream",
            };
            let cache = HeaderValue::from_static("no-cache, no-store, must-revalidate");
            let h_no_cache = [
                (header::CONTENT_TYPE, HeaderValue::from_static(mime)),
                (header::CACHE_CONTROL, cache),
            ];
            let h_default = [(header::CONTENT_TYPE, HeaderValue::from_static(mime))];
            // The dashboard frontend is a multi-step refactor
            // (TypeScript strict migration, then the per-attempt
            // stage-isolation gate). The browser was caching old
            // `dist/views/logs.js` / `dist/components/log-row.js`
            // payloads across deploys and re-introducing a fixed
            // bug; the user-facing symptom was the "retry
            // duplicates counters" regression returning after a
            // confirmed fix. Force a strong no-cache for the JS
            // and CSS bundles so the next page load always picks
            // up the latest build. HTML responses keep the
            // browser default (the `index.html` is hand-managed).
            let response: Response = if matches!(
                fs_path.extension().and_then(|s| s.to_str()),
                Some("js") | Some("mjs") | Some("css")
            ) {
                (StatusCode::OK, h_no_cache, Body::from(bytes)).into_response()
            } else {
                (StatusCode::OK, h_default, Body::from(bytes)).into_response()
            };
            response
        }
        Err(_) => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}
