use crate::handlers::admin::{ApiError, AppState, CoreError};

use axum::{Json, extract::State};

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

    // Security (audit H-1): the loopback callback the user's browser is told
    // to redirect to (navigate_uri) must point at a listener THIS server
    // owns. The URI used to be a fixed port (18432) that any other local
    // process could squat ahead of us; the listener now binds an ephemeral
    // port first, the URI is derived from the port the OS actually gave us,
    // and the two can never drift apart.
    let (listener, loopback_port) = super::bind_ephemeral().await.map_err(|e| {
        ApiError(CoreError::Internal(format!(
            "zai loopback listener bind failed: {e}"
        )))
    })?;
    let navigate_uri = super::navigate_uri_for_port(loopback_port);

    let payload = serde_json::json!({
        "navigate_uri": navigate_uri,
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

    // Register the state and serve exactly one callback connection on the
    // ephemeral listener. The state is bound to the loopback URI actually
    // negotiated above, so the listener's exchange (which validates and
    // consumes it) uses the same redirect the upstream saw.
    if let Some(state_val) = input.state {
        s.oauth_states()
            .insert("zai", &state_val, "", &navigate_uri);
        super::spawn_single_shot_handler(s.clone(), listener, loopback_port, "zai".to_string());
    } else {
        // No dashboard-supplied state: nothing to bind the listener to, and
        // an unbound listener would accept any code. Drop it (fail closed).
        drop(listener);
    }

    Ok(Json(json))
}
