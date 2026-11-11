//! Signed autoclaw identity headers, bearer-token stripping and the inherent
//! `exchange_business_token` used by the zAI OAuth provider trait impl in
//! `zai.rs`.

use super::{
    AUTOCLAW_APP_ID, AUTOCLAW_APP_KEY, AUTOCLAW_APP_VERSION, ZAI_DEFAULT_BUSINESS_LOGIN_URL,
    ZaiOAuthProvider,
};
use crate::error::{CoreError, Result};
use openproxy_adapters::upstream::{UpstreamClient, UpstreamRequest};
use std::sync::Arc;

/// Product identity headers the AutoClaw identity gateway requires.
///
/// `X-Auth-Sign` is an MD5 over `appId & timestamp & appKey`, exactly as the
/// official client computes it. A stale timestamp or a wrong signature is
/// rejected with `400002 Middleware sign error`, so both are minted together
/// here and nowhere else.
pub(super) fn apply_autoclaw_identity_headers(
    req: &mut openproxy_adapters::upstream::UpstreamRequest,
) {
    let timestamp = openproxy_types::now_unix_secs_str();
    let sign = crate::oauth::minimax::md5::md5_hex(format!(
        "{AUTOCLAW_APP_ID}&{timestamp}&{AUTOCLAW_APP_KEY}"
    ));
    for (name, value) in [
        ("x-product", "autoclaw".to_string()),
        ("x-version", AUTOCLAW_APP_VERSION.to_string()),
        ("x-tm", "linux".to_string()),
        ("x-channel", "official".to_string()),
        ("x-auth-appid", AUTOCLAW_APP_ID.to_string()),
        ("x-auth-timestamp", timestamp),
        ("x-auth-sign", sign),
        ("x-lang", "zh-CN".to_string()),
    ] {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::try_from(name),
            http::HeaderValue::from_str(&value),
        ) {
            req.headers.insert(name, value);
        }
    }
}

/// Strip the `Bearer ` prefix the gateway stores its JWTs with.
pub(super) fn strip_bearer(token: &str) -> Option<&str> {
    let trimmed = token.trim();
    let stripped = trimmed
        .strip_prefix("Bearer ")
        .or_else(|| trimmed.strip_prefix("bearer "))
        .unwrap_or(trimmed)
        .trim();
    (!stripped.is_empty()).then_some(stripped)
}

impl ZaiOAuthProvider {
    pub(crate) async fn exchange_business_token(
        upstream_client: &Arc<UpstreamClient>,
        zai_access_token: &str,
    ) -> Result<String> {
        let body = serde_json::json!({
            "token": zai_access_token
        });
        let body_bytes =
            serde_json::to_vec(&body).map_err(|e| CoreError::Validation(e.to_string()))?;

        let mut req = UpstreamRequest::post_json(
            ZAI_DEFAULT_BUSINESS_LOGIN_URL,
            bytes::Bytes::from(body_bytes),
        )
        .with_accept_json();
        req.headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );

        let (status, resp_bytes) =
            crate::oauth::call_oauth(upstream_client, req, "zai business login").await?;

        if !status.is_success() {
            return Err(CoreError::UpstreamConnection(format!(
                "business login status: {}",
                status.as_u16()
            )));
        }

        let json: serde_json::Value = serde_json::from_slice(&resp_bytes)
            .map_err(|e| CoreError::Parse(format!("zai business login parse: {e}")))?;

        let tok = json
            .get("data")
            .and_then(|d| d.get("access_token").or_else(|| d.get("accessToken")))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| CoreError::Parse("missing business access_token".into()))?
            .trim()
            .to_string();

        Ok(tok)
    }
}
