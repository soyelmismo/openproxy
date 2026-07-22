//! Codex / ChatGPT OAuth provider.
//!
//! Uses OpenAI's custom device authorization flow.
//! The ChatGPT account id from `id_token` claims is stored as `{"workspaceId": "..."}` when available.

use base64::Engine;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::error::{CoreError, Result};
use crate::oauth::{DeviceAuthorizationResponse, OAuthFlow, OAuthProvider, TokenResponse};
use super::generic::{GenericOAuthProvider, OAuthRequestEncoding, OAuthSpec};
use openproxy_adapters::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamError, UpstreamRequest,
};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const DEVICE_USERCODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const VERIFICATION_URI: &str = "https://auth.openai.com/codex/device";
const REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const SCOPES: &[&str] = &["openid", "profile", "email", "offline_access"];

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CodexProviderMeta {
    #[serde(rename = "workspaceId", skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
}

fn codex_oauth_spec() -> OAuthSpec {
    OAuthSpec {
        id: "codex",
        flow: OAuthFlow::DeviceCode,
        authorize_url: None,
        token_url: TOKEN_URL,
        device_authorization_url: None,
        client_id_env: Some("OPENPROXY_CODEX_CLIENT_ID"),
        client_id_default: CLIENT_ID,
        client_secret_env: None,
        client_secret_default: None,
        scopes: SCOPES,
        auth_extra_params: &[],
        request_encoding: OAuthRequestEncoding::FormUrlEncoded,
        user_agent: Some(openproxy_adapters::adapters::codex::codex_user_agent),
    }
}

#[derive(Clone)]
pub struct CodexOAuthProvider {
    generic: GenericOAuthProvider,
}

impl CodexOAuthProvider {
    pub fn new() -> Self {
        Self {
            generic: GenericOAuthProvider::new(codex_oauth_spec()),
        }
    }
}

impl Default for CodexOAuthProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl OAuthProvider for CodexOAuthProvider {
    crate::delegate_oauth_to_generic!(name, flow);

    async fn build_auth_url(
        &self,
        _redirect_uri: &str,
    ) -> Result<(String, String, String, String)> {
        Err(CoreError::Validation(
            "codex uses device code flow, not PKCE".into(),
        ))
    }

    async fn exchange_code(
        &self,
        _code: &str,
        _code_verifier: &str,
        _upstream_client: &Arc<UpstreamClient>,
        _redirect_uri: &str,
    ) -> Result<TokenResponse> {
        Err(CoreError::Validation(
            "codex uses device code flow, not authorization code".into(),
        ))
    }

    async fn request_device_code(
        &self,
        upstream_client: &Arc<UpstreamClient>,
    ) -> Result<DeviceAuthorizationResponse> {
        let body = serde_json::json!({ "client_id": CLIENT_ID });
        let body_bytes = serde_json::to_vec(&body).unwrap();
        let mut req =
            UpstreamRequest::post_json(DEVICE_USERCODE_URL, bytes::Bytes::from(body_bytes));
        req.headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        req.headers.insert(
            http::header::ACCEPT,
            http::HeaderValue::from_static("application/json"),
        );

        let cancel = CancellationToken::new();
        let response = upstream_client
            .call(req, TimeoutProfile::OAuth, cancel)
            .await
            .map_err(|e| match e {
                UpstreamError::Cancel => CoreError::ClientDisconnected,
                other => CoreError::UpstreamConnection(format!("codex deviceauth: {other}")),
            })?;

        let status = response.status;
        let body = response.collect().await.map_err(|e| match e {
            UpstreamError::Cancel => CoreError::ClientDisconnected,
            other => CoreError::UpstreamConnection(format!("codex deviceauth body: {other}")),
        })?;

        if status.as_u16() == 404 {
            return Err(CoreError::Validation(
                "Device code login is not enabled for this account. Enable it in ChatGPT security settings.".into()
            ));
        }

        if !status.is_success() {
            return Err(CoreError::UpstreamError {
                status: status.as_u16(),
                provider: "codex".into(),
                model: "<oauth>".into(),
                body: String::from_utf8_lossy(&body).into(),
                is_proxy_rotated: false,
            });
        }

        #[derive(Deserialize)]
        struct UserCodeResp {
            device_auth_id: String,
            user_code: Option<String>,
            usercode: Option<String>,
            interval: Option<serde_json::Value>,
        }

        let resp: UserCodeResp = serde_json::from_slice(&body)
            .map_err(|e| CoreError::Parse(format!("codex usercode parse: {e}")))?;

        let user_code = resp
            .user_code
            .or(resp.usercode)
            .ok_or_else(|| CoreError::Parse("codex usercode missing user_code".into()))?;

        let combined_code = format!("{}|{}", resp.device_auth_id, user_code);

        let interval = resp
            .interval
            .and_then(|v| {
                if let Some(i) = v.as_u64() {
                    Some(i)
                } else if let Some(s) = v.as_str() {
                    s.parse::<u64>().ok()
                } else {
                    None
                }
            })
            .unwrap_or(5);

        Ok(DeviceAuthorizationResponse {
            device_code: combined_code,
            user_code,
            verification_uri: VERIFICATION_URI.into(),
            verification_uri_complete: None,
            expires_in: Some(15 * 60),
            interval: Some(interval),
        })
    }

    async fn poll_device_token(
        &self,
        device_code: &str,
        upstream_client: &Arc<UpstreamClient>,
    ) -> Result<Option<TokenResponse>> {
        let parts: Vec<&str> = device_code.splitn(2, '|').collect();
        if parts.len() != 2 {
            return Err(CoreError::Validation(
                "Invalid codex composite device code".into(),
            ));
        }
        let device_auth_id = parts[0];
        let user_code = parts[1];

        let body = serde_json::json!({
            "device_auth_id": device_auth_id,
            "user_code": user_code,
        });
        let body_bytes = serde_json::to_vec(&body).unwrap();
        let mut req = UpstreamRequest::post_json(DEVICE_TOKEN_URL, bytes::Bytes::from(body_bytes));
        req.headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        req.headers.insert(
            http::header::ACCEPT,
            http::HeaderValue::from_static("application/json"),
        );

        let cancel = CancellationToken::new();
        let response = upstream_client
            .call(req, TimeoutProfile::OAuth, cancel.clone())
            .await
            .map_err(|e| match e {
                UpstreamError::Cancel => CoreError::ClientDisconnected,
                other => CoreError::UpstreamConnection(format!("codex poll: {other}")),
            })?;

        let status = response.status;
        let body = response.collect().await.map_err(|e| match e {
            UpstreamError::Cancel => CoreError::ClientDisconnected,
            other => CoreError::UpstreamConnection(format!("codex poll body: {other}")),
        })?;

        if status.as_u16() == 403 || status.as_u16() == 404 {
            return Ok(None);
        }

        if !status.is_success() {
            return Err(CoreError::UpstreamError {
                status: status.as_u16(),
                provider: "codex".into(),
                model: "<oauth>".into(),
                body: String::from_utf8_lossy(&body).into(),
                is_proxy_rotated: false,
            });
        }

        #[derive(Deserialize)]
        struct PollResp {
            authorization_code: String,
            code_verifier: String,
        }

        let poll_resp: PollResp = serde_json::from_slice(&body)
            .map_err(|e| CoreError::Parse(format!("codex poll parse: {e}")))?;

        let params = vec![
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", poll_resp.authorization_code.as_str()),
            ("code_verifier", poll_resp.code_verifier.as_str()),
            ("redirect_uri", REDIRECT_URI),
        ];

        let token_body = crate::oauth::generic::urlencoded_body(&params);
        let mut token_req = UpstreamRequest::post_json(TOKEN_URL, token_body);
        token_req.headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/x-www-form-urlencoded"),
        );

        let token_response = upstream_client
            .call(token_req, TimeoutProfile::OAuth, cancel)
            .await
            .map_err(|e| match e {
                UpstreamError::Cancel => CoreError::ClientDisconnected,
                other => CoreError::UpstreamConnection(format!("codex exchange: {other}")),
            })?;

        let token_status = token_response.status;
        let token_body_bytes = token_response.collect().await.map_err(|e| match e {
            UpstreamError::Cancel => CoreError::ClientDisconnected,
            other => CoreError::UpstreamConnection(format!("codex exchange body: {other}")),
        })?;

        if !token_status.is_success() {
            return Err(CoreError::UpstreamError {
                status: token_status.as_u16(),
                provider: "codex".into(),
                model: "<oauth>".into(),
                body: String::from_utf8_lossy(&token_body_bytes).into(),
                is_proxy_rotated: false,
            });
        }

        let token: TokenResponse = serde_json::from_slice(&token_body_bytes)
            .map_err(|e| CoreError::Parse(format!("codex token parse: {e}")))?;

        Ok(Some(token))
    }

    crate::delegate_oauth_to_generic!(refresh_token);

    fn provider_specific_from_token(&self, token: &TokenResponse) -> Option<String> {
        let claims = token.id_token.as_deref().and_then(decode_jwt_payload)?;
        let workspace_id = extract_workspace_id(&claims)?;
        serde_json::to_string(&CodexProviderMeta {
            workspace_id: Some(workspace_id),
        })
        .ok()
    }

    fn email_from_token(&self, token: &TokenResponse) -> Option<String> {
        let claims = token.id_token.as_deref().and_then(decode_jwt_payload)?;
        claims
            .get("email")
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
            .map(ToString::to_string)
    }
}

fn decode_jwt_payload(jwt: &str) -> Option<serde_json::Value> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn extract_workspace_id(claims: &serde_json::Value) -> Option<String> {
    let keys = [
        "https://api.openai.com/auth.chatgpt_account_id/account_id",
        "chatgpt_account_id",
        "account_id",
    ];
    for key in keys {
        if let Some(value) = claims.get(key).and_then(|v| v.as_str())
            && !value.is_empty()
        {
            return Some(value.to_string());
        }
    }
    claims
        .get("https://api.openai.com/auth.chatgpt_account_id")
        .and_then(|v| v.get("account_id"))
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty())
        .map(ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_workspace_id_from_claims() {
        let claims = serde_json::json!({
            "https://api.openai.com/auth.chatgpt_account_id/account_id": "acc_123",
        });
        assert_eq!(extract_workspace_id(&claims).as_deref(), Some("acc_123"));
    }
}
