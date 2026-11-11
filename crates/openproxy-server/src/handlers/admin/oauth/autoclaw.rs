use crate::handlers::admin::{ApiError, AppState, CoreError};

use axum::{Json, extract::State};

use super::get_oauth_redirect_uri;

pub async fn autoclaw_captcha_config(
    State(s): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    const APP_ID: &str = "100003";
    const APP_KEY: &str = "38d2391985e2369a5fb8227d8e6cd5e5";
    let now = openproxy_types::now_unix_secs_str();
    let sign = openproxy_oauth::oauth::minimax::md5::md5_hex(format!("{APP_ID}&{now}&{APP_KEY}"));

    let mut req = openproxy_adapters::upstream::UpstreamRequest::post_json(
        "https://autoglm-api.autoglm.ai/userapi/overseasv1/oauth-captcha-config",
        bytes::Bytes::from_static(b"{}"),
    )
    .with_accept_json();
    req.headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    req.headers.insert(
        http::header::USER_AGENT,
        http::HeaderValue::from_static("AutoClaw/2.0.3"),
    );
    req.headers.insert(
        http::header::HeaderName::from_static("x-product"),
        http::HeaderValue::from_static("autoclaw"),
    );
    req.headers.insert(
        http::header::HeaderName::from_static("x-version"),
        http::HeaderValue::from_static("2.0.3"),
    );
    req.headers.insert(
        http::header::HeaderName::from_static("x-tm"),
        http::HeaderValue::from_static("linux"),
    );
    req.headers.insert(
        http::header::HeaderName::from_static("x-channel"),
        http::HeaderValue::from_static("official"),
    );
    req.headers.insert(
        http::header::HeaderName::from_static("x-auth-appid"),
        http::HeaderValue::from_static(APP_ID),
    );
    if let Ok(val) = http::HeaderValue::from_str(&now) {
        req.headers.insert(
            http::header::HeaderName::from_static("x-auth-timestamp"),
            val,
        );
    }
    if let Ok(val) = http::HeaderValue::from_str(&sign) {
        req.headers
            .insert(http::header::HeaderName::from_static("x-auth-sign"), val);
    }
    req.headers.insert(
        http::header::HeaderName::from_static("x-lang"),
        http::HeaderValue::from_static("zh-CN"),
    );

    let (_status, resp_bytes) =
        openproxy_oauth::oauth::call_oauth(s.upstream_client(), req, "autoclaw captcha config")
            .await?;

    let json: serde_json::Value = serde_json::from_slice(&resp_bytes)
        .map_err(|e| ApiError(CoreError::Parse(e.to_string())))?;

    Ok(Json(json))
}

#[derive(serde::Deserialize)]
pub struct VerifyCaptchaInput {
    pub verify_param: serde_json::Value,
    pub state: Option<String>,
}

pub async fn autoclaw_verify_captcha(
    State(s): State<AppState>,
    Json(input): Json<VerifyCaptchaInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    const APP_ID: &str = "100003";
    const APP_KEY: &str = "38d2391985e2369a5fb8227d8e6cd5e5";
    let now = openproxy_types::now_unix_secs_str();
    let sign = openproxy_oauth::oauth::minimax::md5::md5_hex(format!("{APP_ID}&{now}&{APP_KEY}"));

    let payload = serde_json::json!({
        "navigate_uri": "http://localhost:18432/auth/callback-zai",
        "source_id": "autoclaw",
        "device_id": "b1b01c10-91dc-4a25-9d32-d17b5f5bbd9b",
        "ali_captcha_verify_param": input.verify_param
    });

    let body_bytes =
        serde_json::to_vec(&payload).map_err(|e| ApiError(CoreError::Parse(e.to_string())))?;

    let mut req = openproxy_adapters::upstream::UpstreamRequest::post_json(
        "https://autoglm-api.autoglm.ai/userapi/overseasv1/zai-oauth-url",
        bytes::Bytes::from(body_bytes),
    )
    .with_accept_json();
    req.headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    req.headers.insert(
        http::header::USER_AGENT,
        http::HeaderValue::from_static("AutoClaw/2.0.3"),
    );
    req.headers.insert(
        http::header::HeaderName::from_static("x-product"),
        http::HeaderValue::from_static("autoclaw"),
    );
    req.headers.insert(
        http::header::HeaderName::from_static("x-version"),
        http::HeaderValue::from_static("2.0.3"),
    );
    req.headers.insert(
        http::header::HeaderName::from_static("x-tm"),
        http::HeaderValue::from_static("linux"),
    );
    req.headers.insert(
        http::header::HeaderName::from_static("x-channel"),
        http::HeaderValue::from_static("official"),
    );
    req.headers.insert(
        http::header::HeaderName::from_static("x-auth-appid"),
        http::HeaderValue::from_static(APP_ID),
    );
    if let Ok(val) = http::HeaderValue::from_str(&now) {
        req.headers.insert(
            http::header::HeaderName::from_static("x-auth-timestamp"),
            val,
        );
    }
    if let Ok(val) = http::HeaderValue::from_str(&sign) {
        req.headers
            .insert(http::header::HeaderName::from_static("x-auth-sign"), val);
    }
    req.headers.insert(
        http::header::HeaderName::from_static("x-lang"),
        http::HeaderValue::from_static("zh-CN"),
    );

    let (_status, resp_bytes) =
        openproxy_oauth::oauth::call_oauth(s.upstream_client(), req, "autoclaw verify captcha")
            .await?;

    let json: serde_json::Value = serde_json::from_slice(&resp_bytes)
        .map_err(|e| ApiError(CoreError::Parse(e.to_string())))?;

    if let Some(state_val) = input.state {
        let redirect_uri = get_oauth_redirect_uri(&s);
        s.oauth_states()
            .insert("zai", &state_val, "", &redirect_uri);
    }

    Ok(Json(json))
}
