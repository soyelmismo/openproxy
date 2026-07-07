//! Codex / ChatGPT OAuth provider.
//!
//! Uses OpenAI's Auth0-backed PKCE flow and stores the ChatGPT account id
//! from `id_token` claims as `{"workspaceId": "..."}` when available.

use async_trait::async_trait;
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::error::Result;
use crate::ids::AccountId;
use crate::oauth::{DbRef, DeviceAuthorizationResponse, OAuthFlow, OAuthProvider, TokenResponse};
use crate::oauth_generic::{GenericOAuthProvider, OAuthRequestEncoding, OAuthSpec};
use crate::upstream::UpstreamClient;

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTH_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const SCOPES: &[&str] = &["openid", "profile", "email", "offline_access"];

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CodexProviderMeta {
    #[serde(rename = "workspaceId", skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
}

fn codex_oauth_spec() -> OAuthSpec {
    OAuthSpec {
        id: "codex",
        flow: OAuthFlow::AuthorizationCodePkce,
        authorize_url: Some(AUTH_URL),
        token_url: TOKEN_URL,
        device_authorization_url: None,
        client_id_env: Some("OPENPROXY_CODEX_CLIENT_ID"),
        client_id_default: CLIENT_ID,
        client_secret_env: None,
        client_secret_default: None,
        scopes: SCOPES,
        auth_extra_params: &[
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
            ("originator", "codex_cli_rs"),
            ("prompt", "login"),
        ],
        request_encoding: OAuthRequestEncoding::FormUrlEncoded,
        user_agent: Some(crate::executor_codex::codex_user_agent),
    }
}

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

#[async_trait]
impl OAuthProvider for CodexOAuthProvider {
    fn name(&self) -> &str {
        self.generic.name()
    }

    fn flow(&self) -> OAuthFlow {
        self.generic.flow()
    }

    async fn build_auth_url(&self, redirect_uri: &str) -> Result<(String, String, String)> {
        self.generic.build_auth_url(redirect_uri).await
    }

    async fn exchange_code(
        &self,
        code: &str,
        code_verifier: &str,
        upstream_client: &Arc<UpstreamClient>,
        redirect_uri: &str,
    ) -> Result<TokenResponse> {
        self.generic
            .exchange_code(code, code_verifier, upstream_client, redirect_uri)
            .await
    }

    async fn request_device_code(
        &self,
        upstream_client: &Arc<UpstreamClient>,
    ) -> Result<DeviceAuthorizationResponse> {
        self.generic.request_device_code(upstream_client).await
    }

    async fn poll_device_token(
        &self,
        device_code: &str,
        upstream_client: &Arc<UpstreamClient>,
    ) -> Result<Option<TokenResponse>> {
        self.generic
            .poll_device_token(device_code, upstream_client)
            .await
    }

    async fn refresh_token(
        &self,
        refresh_token: &str,
        upstream_client: &Arc<UpstreamClient>,
        account_id: AccountId,
        db: DbRef<'_>,
    ) -> Result<TokenResponse> {
        self.generic
            .refresh_token(refresh_token, upstream_client, account_id, db)
            .await
    }

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

    #[tokio::test]
    async fn codex_authorize_url_includes_cli_params() {
        let p = CodexOAuthProvider::new();
        let (url, verifier, challenge) = p
            .build_auth_url("http://localhost:8788/admin/callback.html")
            .await
            .unwrap();

        assert!(!verifier.is_empty());
        assert_eq!(
            challenge,
            crate::oauth_generic::code_challenge_s256(&verifier)
        );
        assert!(url.starts_with(AUTH_URL));
        assert!(url.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
        assert!(url.contains("scope=openid%20profile%20email%20offline_access"));
        assert!(url.contains("originator=codex_cli_rs"));
        assert!(url.contains("codex_cli_simplified_flow=true"));
    }

    #[test]
    fn extracts_workspace_id_from_claims() {
        let claims = serde_json::json!({
            "https://api.openai.com/auth.chatgpt_account_id/account_id": "acc_123",
        });
        assert_eq!(extract_workspace_id(&claims).as_deref(), Some("acc_123"));
    }
}
