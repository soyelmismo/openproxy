//! Codex / ChatGPT OAuth provider.
//!
//! Uses OpenAI's custom device authorization flow.
//! The ChatGPT account id from `id_token` claims is stored as `{"workspaceId": "..."}` when available.

use serde::{Deserialize, Serialize};
use std::sync::Arc;

use super::generic::{GenericOAuthProvider, OAuthRequestEncoding, OAuthSpec};
use crate::error::{CoreError, Result};
use crate::oauth::{
    DeviceAuthorizationResponse, OAuthFlow, OAuthProvider, TokenResponse, map_upstream_err,
};
use openproxy_adapters::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest,
};

pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const DEVICE_USERCODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
pub const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
pub const VERIFICATION_URI: &str = "https://auth.openai.com/codex/device";
pub const REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
pub const SCOPES: &[&str] = &["openid", "profile", "email", "offline_access"];

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
    resolver: super::OAuthEndpointResolver,
}

impl CodexOAuthProvider {
    pub fn new() -> Self {
        Self {
            generic: GenericOAuthProvider::new(codex_oauth_spec()),
            resolver: super::OAuthEndpointResolver::new(
                "OPENPROXY_CODEX_AUTH_BASE_URL",
                "https://auth.openai.com",
            ),
        }
    }

    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            generic: GenericOAuthProvider::new(codex_oauth_spec()),
            resolver: super::OAuthEndpointResolver::new(
                "OPENPROXY_CODEX_AUTH_BASE_URL",
                "https://auth.openai.com",
            )
            .with_custom_base(base_url),
        }
    }

    pub fn usercode_url(&self) -> String {
        std::env::var("OPENPROXY_CODEX_DEVICE_USERCODE_URL").unwrap_or_else(|_| {
            self.resolver
                .url_with_path("api/accounts/deviceauth/usercode")
        })
    }

    pub fn device_token_url(&self) -> String {
        std::env::var("OPENPROXY_CODEX_DEVICE_TOKEN_URL")
            .unwrap_or_else(|_| self.resolver.url_with_path("api/accounts/deviceauth/token"))
    }

    pub fn token_url(&self) -> String {
        std::env::var("OPENPROXY_CODEX_TOKEN_URL")
            .unwrap_or_else(|_| self.resolver.url_with_path("oauth/token"))
    }
}

impl Default for CodexOAuthProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl OAuthProvider for CodexOAuthProvider {
    crate::delegate_oauth_to_generic!(name, flow);

    fn build_auth_url(
        &self,
        _redirect_uri: &str,
    ) -> impl std::future::Future<Output = Result<(String, String, String, String)>> + Send {
        std::future::ready(Err(CoreError::Validation(
            "codex uses device code flow, not PKCE".into(),
        )))
    }

    fn exchange_code(
        &self,
        _code: &str,
        _code_verifier: &str,
        _upstream_client: &Arc<UpstreamClient>,
        _redirect_uri: &str,
    ) -> impl std::future::Future<Output = Result<TokenResponse>> + Send {
        std::future::ready(Err(CoreError::Validation(
            "codex uses device code flow, not authorization code".into(),
        )))
    }

    async fn request_device_code(
        &self,
        upstream_client: &Arc<UpstreamClient>,
    ) -> Result<DeviceAuthorizationResponse> {
        let body = serde_json::json!({ "client_id": CLIENT_ID });
        let body_bytes =
            serde_json::to_vec(&body).map_err(|e| CoreError::Validation(e.to_string()))?;
        let mut req =
            UpstreamRequest::post_json(self.usercode_url(), bytes::Bytes::from(body_bytes));
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
            .map_err(|e| map_upstream_err(e, "codex deviceauth"))?;

        let status = response.status;
        let body = response
            .collect()
            .await
            .map_err(|e| map_upstream_err(e, "codex deviceauth body"))?;

        if status.as_u16() == 404 {
            return Err(CoreError::Validation(
                "Device code login is not enabled for this account. Enable it in ChatGPT security settings.".into()
            ));
        }

        super::check_oauth_status(status, "codex", &body)?;

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
        let Some((device_auth_id, user_code)) = device_code.split_once('|') else {
            return Err(CoreError::Validation(
                "Invalid codex composite device code".into(),
            ));
        };

        let body = serde_json::json!({
            "device_auth_id": device_auth_id,
            "user_code": user_code,
        });
        let body_bytes =
            serde_json::to_vec(&body).map_err(|e| CoreError::Validation(e.to_string()))?;
        let mut req =
            UpstreamRequest::post_json(self.device_token_url(), bytes::Bytes::from(body_bytes));
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
            .map_err(|e| map_upstream_err(e, "codex poll"))?;

        let status = response.status;
        let body = response
            .collect()
            .await
            .map_err(|e| map_upstream_err(e, "codex poll body"))?;

        if status.as_u16() == 403 || status.as_u16() == 404 {
            return Ok(None);
        }

        super::check_oauth_status(status, "codex", &body)?;

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
        let mut token_req = UpstreamRequest::post_json(self.token_url(), token_body);
        token_req.headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/x-www-form-urlencoded"),
        );

        let token_response = upstream_client
            .call(token_req, TimeoutProfile::OAuth, CancellationToken::new())
            .await
            .map_err(|e| map_upstream_err(e, "codex exchange"))?;

        let token_status = token_response.status;
        let token_body_bytes = token_response
            .collect()
            .await
            .map_err(|e| map_upstream_err(e, "codex exchange body"))?;

        super::check_oauth_status(token_status, "codex", &token_body_bytes)?;

        let token: TokenResponse = serde_json::from_slice(&token_body_bytes)
            .map_err(|e| CoreError::Parse(format!("codex token parse: {e}")))?;

        Ok(Some(token))
    }

    async fn refresh_token(
        &self,
        refresh_token: &str,
        upstream_client: &Arc<UpstreamClient>,
        account_id: crate::ids::AccountId,
        db: crate::oauth::DbRef<'_>,
    ) -> Result<TokenResponse> {
        if !self.resolver.has_custom_base() {
            return self
                .generic
                .refresh_token(refresh_token, upstream_client, account_id, db)
                .await;
        }

        let params = [
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", refresh_token),
        ];
        let body = crate::oauth::generic::urlencoded_body(&params);
        let mut req = UpstreamRequest::post_json(self.token_url(), body);
        req.headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/x-www-form-urlencoded"),
        );
        req.headers.insert(
            http::header::USER_AGENT,
            http::HeaderValue::from_str(&openproxy_adapters::adapters::codex::codex_user_agent())
                .unwrap_or_else(|_| http::HeaderValue::from_static("codex")),
        );

        let cancel = CancellationToken::new();
        let resp = upstream_client
            .call(req, TimeoutProfile::OAuth, cancel)
            .await
            .map_err(|e| map_upstream_err(e, "codex token refresh"))?;

        let status = resp.status;
        let bytes = resp
            .collect()
            .await
            .map_err(|e| map_upstream_err(e, "codex token refresh body"))?;

        super::check_oauth_status(status, "codex", &bytes)?;

        serde_json::from_slice(&bytes)
            .map_err(|e| CoreError::Parse(format!("codex token refresh parse: {e}")))
    }

    fn provider_specific_from_token(&self, token: &TokenResponse) -> Option<String> {
        let claims = token
            .id_token
            .as_deref()
            .and_then(super::decode_jwt_payload)?;
        let workspace_id = extract_workspace_id(&claims)?;
        serde_json::to_string(&CodexProviderMeta {
            workspace_id: Some(workspace_id),
        })
        .ok()
    }

    fn email_from_token(&self, token: &TokenResponse) -> Option<String> {
        super::extract_email_from_token(token)
    }
}

#[derive(Deserialize)]
struct UserCodeResp {
    device_auth_id: String,
    user_code: Option<String>,
    usercode: Option<String>,
    interval: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct PollResp {
    authorization_code: String,
    code_verifier: String,
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
    use base64::Engine;

    #[test]
    fn test_codex_provider_metadata() {
        let provider = CodexOAuthProvider::new();
        assert_eq!(provider.name(), "codex");
        assert_eq!(provider.flow(), OAuthFlow::DeviceCode);
        assert!(provider.aliases().is_empty());
        assert_eq!(CLIENT_ID, "app_EMoamEEZ73f0CkXaXp7hrann");
        assert_eq!(TOKEN_URL, "https://auth.openai.com/oauth/token");
        assert_eq!(
            DEVICE_USERCODE_URL,
            "https://auth.openai.com/api/accounts/deviceauth/usercode"
        );
        assert_eq!(
            DEVICE_TOKEN_URL,
            "https://auth.openai.com/api/accounts/deviceauth/token"
        );
        assert_eq!(VERIFICATION_URI, "https://auth.openai.com/codex/device");
        assert_eq!(REDIRECT_URI, "https://auth.openai.com/deviceauth/callback");
        assert_eq!(SCOPES, &["openid", "profile", "email", "offline_access"]);
    }

    #[test]
    fn extracts_workspace_id_from_claims() {
        let claims1 = serde_json::json!({
            "https://api.openai.com/auth.chatgpt_account_id/account_id": "acc_123",
        });
        assert_eq!(extract_workspace_id(&claims1).as_deref(), Some("acc_123"));

        let claims2 = serde_json::json!({
            "chatgpt_account_id": "acc_456",
        });
        assert_eq!(extract_workspace_id(&claims2).as_deref(), Some("acc_456"));

        let claims3 = serde_json::json!({
            "account_id": "acc_789",
        });
        assert_eq!(extract_workspace_id(&claims3).as_deref(), Some("acc_789"));

        let claims4 = serde_json::json!({
            "https://api.openai.com/auth.chatgpt_account_id": {
                "account_id": "acc_nested"
            }
        });
        assert_eq!(
            extract_workspace_id(&claims4).as_deref(),
            Some("acc_nested")
        );

        let empty = serde_json::json!({ "account_id": "" });
        assert_eq!(extract_workspace_id(&empty), None);
    }

    #[test]
    fn test_codex_provider_meta_serde() {
        let meta = CodexProviderMeta {
            workspace_id: Some("ws-alpha".into()),
        };
        let json = serde_json::to_string(&meta).expect("serialize");
        assert_eq!(json, r#"{"workspaceId":"ws-alpha"}"#);
        let back: CodexProviderMeta = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.workspace_id.as_deref(), Some("ws-alpha"));

        let empty = CodexProviderMeta::default();
        let json_empty = serde_json::to_string(&empty).expect("serialize empty");
        assert_eq!(json_empty, "{}");
    }

    #[test]
    fn test_usercode_response_deserialization() {
        let json1 = r#"{"device_auth_id":"da_1","user_code":"UC-123","interval":10}"#;
        let resp1: UserCodeResp = serde_json::from_str(json1).expect("parse usercode 1");
        assert_eq!(resp1.device_auth_id, "da_1");
        assert_eq!(resp1.user_code.as_deref(), Some("UC-123"));
        assert_eq!(resp1.interval.and_then(|v| v.as_u64()), Some(10));

        let json2 = r#"{"device_auth_id":"da_2","usercode":"UC-456","interval":"15"}"#;
        let resp2: UserCodeResp = serde_json::from_str(json2).expect("parse usercode 2");
        assert_eq!(resp2.device_auth_id, "da_2");
        assert_eq!(resp2.usercode.as_deref(), Some("UC-456"));

        let json_poll = r#"{"authorization_code":"ac_999","code_verifier":"cv_888"}"#;
        let poll: PollResp = serde_json::from_str(json_poll).expect("parse poll");
        assert_eq!(poll.authorization_code, "ac_999");
        assert_eq!(poll.code_verifier, "cv_888");
    }

    #[test]
    fn test_codex_claims_and_email_from_token() {
        let provider = CodexOAuthProvider::new();

        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"alg":"HS256"}"#);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"email":"codex_user@example.com","chatgpt_account_id":"acc_chatgpt_777"}"#);
        let id_token_jwt = format!("{header}.{payload}.sig");

        let token = TokenResponse {
            access_token: "mock-access-token".into(),
            token_type: "Bearer".into(),
            expires_in: Some(3600),
            refresh_token: Some("mock-refresh-token".into()),
            scope: None,
            id_token: Some(id_token_jwt),
        };

        assert_eq!(
            provider.email_from_token(&token).as_deref(),
            Some("codex_user@example.com")
        );
        let meta_json = provider
            .provider_specific_from_token(&token)
            .expect("workspaceId meta json");
        let meta: CodexProviderMeta = serde_json::from_str(&meta_json).expect("parse meta");
        assert_eq!(meta.workspace_id.as_deref(), Some("acc_chatgpt_777"));
    }

    #[tokio::test]
    async fn test_codex_unsupported_flows_and_invalid_device_code() {
        let provider = CodexOAuthProvider::new();
        let client = Arc::new(UpstreamClient::new());

        assert!(provider.build_auth_url("http://loc/cb").await.is_err());
        assert!(
            provider
                .exchange_code("code", "verifier", &client, "http://loc/cb")
                .await
                .is_err()
        );

        // Invalid composite device code (missing pipe)
        let invalid_poll = provider
            .poll_device_token("invalid_code_no_pipe", &client)
            .await;
        assert!(invalid_poll.is_err());
    }

    #[test]
    fn test_codex_custom_base_url() {
        let provider = CodexOAuthProvider::with_base_url("http://127.0.0.1:7777");
        assert_eq!(
            provider.usercode_url(),
            "http://127.0.0.1:7777/api/accounts/deviceauth/usercode"
        );
        assert_eq!(
            provider.device_token_url(),
            "http://127.0.0.1:7777/api/accounts/deviceauth/token"
        );
        assert_eq!(provider.token_url(), "http://127.0.0.1:7777/oauth/token");
    }
}
