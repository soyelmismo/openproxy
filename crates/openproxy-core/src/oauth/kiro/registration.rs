use openproxy_adapters::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest,
};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::error::{CoreError, Result};
use crate::oauth::map_upstream_err;

pub const REGISTER_URL: &str = "https://oidc.us-east-1.amazonaws.com/client/register";
pub const DEVICE_AUTH_URL: &str = "https://oidc.us-east-1.amazonaws.com/device_authorization";
pub const TOKEN_URL: &str = "https://oidc.us-east-1.amazonaws.com/token";

pub fn kiro_oidc_base_url(region: Option<&str>) -> String {
    if let Ok(base) = std::env::var("OPENPROXY_KIRO_OIDC_BASE_URL")
        && !base.trim().is_empty()
    {
        return base.trim().trim_end_matches('/').to_string();
    }
    let reg = region
        .filter(|r| !r.is_empty())
        .map_or_else(super::profiles::default_region, std::string::ToString::to_string);
    format!("https://oidc.{reg}.amazonaws.com")
}

pub fn kiro_register_url(region: Option<&str>) -> String {
    if let Ok(url) = std::env::var("OPENPROXY_KIRO_REGISTER_URL")
        && !url.trim().is_empty()
    {
        return url;
    }
    format!("{}/client/register", kiro_oidc_base_url(region))
}

pub fn kiro_device_auth_url(region: Option<&str>) -> String {
    if let Ok(url) = std::env::var("OPENPROXY_KIRO_DEVICE_AUTH_URL")
        && !url.trim().is_empty()
    {
        return url;
    }
    format!("{}/device_authorization", kiro_oidc_base_url(region))
}

pub fn kiro_token_url(region: Option<&str>) -> String {
    if let Ok(url) = std::env::var("OPENPROXY_KIRO_TOKEN_URL")
        && !url.trim().is_empty()
    {
        return url;
    }
    format!("{}/token", kiro_oidc_base_url(region))
}

pub fn kiro_social_token_url() -> String {
    std::env::var("OPENPROXY_KIRO_SOCIAL_TOKEN_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "https://prod.us-east-1.auth.desktop.kiro.dev/refreshToken".to_string())
}

pub const SCOPES: &[&str] = &[
    "codewhisperer:completions",
    "codewhisperer:analysis",
    "codewhisperer:conversations",
];

#[derive(Debug, Serialize)]
pub struct RegisterClientRequest {
    #[serde(rename = "clientName")]
    pub client_name: String,
    #[serde(rename = "clientType")]
    pub client_type: String,
    pub scopes: Vec<String>,
    #[serde(rename = "grantTypes")]
    pub grant_types: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct RegisterClientResponse {
    #[serde(rename = "clientId")]
    pub client_id: String,
    #[serde(rename = "clientSecret")]
    pub client_secret: String,
}

pub(crate) struct LastKiroClient {
    pub(crate) client_id: String,
    pub(crate) client_secret: String,
    pub(crate) stored_at: Instant,
}

pub(crate) static LAST_KIRO_CLIENT: LazyLock<Mutex<Option<LastKiroClient>>> =
    LazyLock::new(|| Mutex::new(None));

pub const LAST_KIRO_CLIENT_TTL: Duration = Duration::from_mins(10);

pub fn take_last_client() -> Option<(String, String)> {
    let mut slot = LAST_KIRO_CLIENT.lock().ok()?;
    let entry = slot.take()?;
    if entry.stored_at.elapsed() > LAST_KIRO_CLIENT_TTL {
        return None;
    }
    Some((entry.client_id, entry.client_secret))
}

pub fn peek_last_client() -> Option<(String, String)> {
    let slot = LAST_KIRO_CLIENT.lock().ok()?;
    let entry = slot.as_ref()?;
    if entry.stored_at.elapsed() > LAST_KIRO_CLIENT_TTL {
        return None;
    }
    Some((entry.client_id.clone(), entry.client_secret.clone()))
}

pub(crate) async fn register_oidc_client(
    upstream_client: &Arc<UpstreamClient>,
    region: &str,
) -> Result<(String, String)> {
    let register_body = serde_json::to_vec(&RegisterClientRequest {
        client_name: "openproxy-kiro".into(),
        client_type: "public".into(),
        scopes: SCOPES
            .iter()
            .map(std::string::ToString::to_string)
            .collect(),
        grant_types: vec![
            "urn:ietf:params:oauth:grant-type:device_code".into(),
            "refresh_token".into(),
        ],
    })
    .map_err(|e| CoreError::Parse(format!("kiro register serialize: {e}")))?;
    let register_url = kiro_register_url(Some(region));
    let register_req = UpstreamRequest::post_json(&register_url, bytes::Bytes::from(register_body));

    let cancel = CancellationToken::new();
    let register_response = upstream_client
        .call(register_req, TimeoutProfile::OAuth, cancel)
        .await
        .map_err(|e| map_upstream_err(e, "kiro client register"))?;

    let register_status = register_response.status;
    let register_body = register_response
        .collect()
        .await
        .map_err(|e| map_upstream_err(e, "kiro register body read"))?;

    if !register_status.is_success() {
        let body_str = String::from_utf8_lossy(&register_body).to_string();
        return Err(CoreError::upstream_error(
            register_status.as_u16(),
            "kiro",
            "<oauth_register>",
            body_str,
            false,
        ));
    }

    let client: RegisterClientResponse = serde_json::from_slice(&register_body)
        .map_err(|e| CoreError::Parse(format!("kiro register response parse: {e}")))?;

    Ok((client.client_id, client.client_secret))
}
