use axum::{
    extract::State,
    http::{header, HeaderValue},
    response::{Html, IntoResponse, Json},
};

use crate::WebState;

pub async fn index_html() -> Html<&'static str> {
    Html(include_str!("static/index.html"))
}

pub async fn app_js() -> (
    [(header::HeaderName, HeaderValue); 1],
    &'static str,
) {
    let h = [(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/javascript; charset=utf-8"),
    )];
    (h, include_str!("static/app.js"))
}

pub async fn styles_css() -> (
    [(header::HeaderName, HeaderValue); 1],
    &'static str,
) {
    let h = [(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/css; charset=utf-8"),
    )];
    (h, include_str!("static/styles.css"))
}

pub async fn web_health(State(s): State<WebState>) -> impl IntoResponse {
    match s.core_client.health().await {
        Ok(v) => Json(serde_json::json!({"status": "ok", "core": v})).into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "core unreachable");
            Json(serde_json::json!({"status": "degraded", "error": e.to_string()}))
                .into_response()
        }
    }
}

pub async fn callback_html() -> Html<&'static str> {
    Html(include_str!("static/callback.html"))
}
