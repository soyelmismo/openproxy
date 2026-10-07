//! Claude Code OAuth provider (Anthropic CLI).
//!
//! Uses Authorization Code grant with PKCE (Proof Key for Code Exchange)
//! against Anthropic's OAuth endpoints using the public Claude Code client ID.
//!
//! After a successful token exchange, the provider queries `GET /api/oauth/profile`
//! to resolve the account identity (email, account UUID, organization UUID) and persists
//! these details in `accounts.email` and `accounts.oauth_provider_specific`.

use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::error::{CoreError, Result};
use crate::ids::AccountId;
use crate::oauth::generic::{code_challenge_s256, generate_code_verifier};
use crate::oauth::util::{check_oauth_status, parse_oauth_callback_input};
use crate::oauth::{DbRef, OAuthEndpointResolver, OAuthFlow, OAuthProvider, TokenResponse, map_upstream_err};
use openproxy_adapters::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest,
};
use openproxy_db::secrets::MasterKey;

pub const CLAUDE_CODE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const CLAUDE_CODE_DEFAULT_AUTH_URL: &str = "https://claude.ai/oauth/authorize";
pub const CLAUDE_CODE_DEFAULT_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub const CLAUDE_CODE_DEFAULT_PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
pub const CLAUDE_CODE_DEFAULT_REDIRECT_URI: &str = "https://platform.claude.com/oauth/code/callback";
pub const CLAUDE_CODE_SCOPES: &[&str] = &["org:create_api_key", "user:profile", "user:inference"];
pub const CLAUDE_CODE_USER_AGENT: &str = "claude-code/0.2.29";
pub const CLAUDE_CODE_AXIOS_USER_AGENT: &str = "axios/1.15.2";

#[derive(Serialize)]
struct ClaudeCodeExchangePayload<'a> {
    grant_type: &'a str,
    code: &'a str,
    redirect_uri: &'a str,
    client_id: &'a str,
    code_verifier: &'a str,
    state: &'a str,
}

#[derive(Serialize)]
struct ClaudeCodeRefreshPayload<'a> {
    client_id: &'a str,
    grant_type: &'a str,
    refresh_token: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<&'a str>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ClaudeCodeProviderMeta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_uuid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organization_uuid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scopes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscription_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit_tier: Option<String>,
}

fn parse_scopes(scope: Option<&str>) -> Vec<String> {
    match scope {
        Some(s) => s.split_whitespace().map(String::from).collect(),
        None => CLAUDE_CODE_SCOPES.iter().map(|s| s.to_string()).collect(),
    }
}

#[derive(Clone)]
pub struct ClaudeCodeOAuthProvider {
    auth_resolver: OAuthEndpointResolver,
    token_resolver: OAuthEndpointResolver,
    profile_resolver: OAuthEndpointResolver,
}

impl ClaudeCodeOAuthProvider {
    pub fn new() -> Self {
        Self {
            auth_resolver: OAuthEndpointResolver::new(
                "OPENPROXY_CLAUDE_CODE_AUTH_URL",
                CLAUDE_CODE_DEFAULT_AUTH_URL,
            ),
            token_resolver: OAuthEndpointResolver::new(
                "OPENPROXY_CLAUDE_CODE_TOKEN_URL",
                CLAUDE_CODE_DEFAULT_TOKEN_URL,
            ),
            profile_resolver: OAuthEndpointResolver::new(
                "OPENPROXY_CLAUDE_CODE_PROFILE_URL",
                CLAUDE_CODE_DEFAULT_PROFILE_URL,
            ),
        }
    }

    pub fn with_endpoints(
        auth_url: impl Into<String>,
        token_url: impl Into<String>,
        profile_url: impl Into<String>,
    ) -> Self {
        Self {
            auth_resolver: OAuthEndpointResolver::new(
                "OPENPROXY_CLAUDE_CODE_AUTH_URL",
                CLAUDE_CODE_DEFAULT_AUTH_URL,
            )
            .with_custom_base(auth_url),
            token_resolver: OAuthEndpointResolver::new(
                "OPENPROXY_CLAUDE_CODE_TOKEN_URL",
                CLAUDE_CODE_DEFAULT_TOKEN_URL,
            )
            .with_custom_base(token_url),
            profile_resolver: OAuthEndpointResolver::new(
                "OPENPROXY_CLAUDE_CODE_PROFILE_URL",
                CLAUDE_CODE_DEFAULT_PROFILE_URL,
            )
            .with_custom_base(profile_url),
        }
    }

    pub fn client_id(&self) -> String {
        std::env::var("OPENPROXY_CLAUDE_CODE_CLIENT_ID")
            .unwrap_or_else(|_| CLAUDE_CODE_CLIENT_ID.to_string())
    }

    pub fn auth_url(&self) -> String {
        self.auth_resolver.base_url()
    }

    pub fn token_url(&self) -> String {
        self.token_resolver.base_url()
    }

    pub fn profile_url(&self) -> String {
        self.profile_resolver.base_url()
    }
}

impl Default for ClaudeCodeOAuthProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl OAuthProvider for ClaudeCodeOAuthProvider {
    fn name(&self) -> &'static str {
        "claude-code"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["claude"]
    }

    fn flow(&self) -> OAuthFlow {
        OAuthFlow::AuthorizationCodePkce
    }

    fn build_auth_url(
        &self,
        redirect_uri: &str,
    ) -> impl std::future::Future<Output = Result<(String, String, String, String)>> + Send {
        let code_verifier = generate_code_verifier();
        let code_challenge = code_challenge_s256(&code_verifier);
        let client_id = self.client_id();
        let authorize_url = self.auth_url();

        let state_bytes: [u8; 16] = rand::random();
        let state = hex::encode(state_bytes);

        // Anthropic Claude Code client expects the registered callback URI
        let effective_redirect = if redirect_uri.contains("localhost") || redirect_uri.contains("127.0.0.1") {
            // Keep local dashboard callback when running locally with proxy redirect,
            // or use the standard Claude Code callback URL.
            CLAUDE_CODE_DEFAULT_REDIRECT_URI
        } else if !redirect_uri.is_empty() {
            redirect_uri
        } else {
            CLAUDE_CODE_DEFAULT_REDIRECT_URI
        };

        let scopes = CLAUDE_CODE_SCOPES.join(" ");
        let params = [
            ("response_type", "code"),
            ("client_id", client_id.as_str()),
            ("redirect_uri", effective_redirect),
            ("scope", scopes.as_str()),
            ("code_challenge", code_challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("state", state.as_str()),
        ];

        let query = crate::oauth::generic::urlencoded_string(&params);
        let full_auth_url = format!("{authorize_url}?{query}");

        std::future::ready(Ok((
            full_auth_url,
            code_verifier,
            effective_redirect.to_string(),
            state,
        )))
    }

    async fn exchange_code(
        &self,
        code: &str,
        code_verifier: &str,
        upstream_client: &Arc<UpstreamClient>,
        redirect_uri: &str,
    ) -> Result<TokenResponse> {
        let (actual_code, state_opt) = parse_oauth_callback_input(code);
        let state_val = state_opt.ok_or_else(|| {
            CoreError::Validation(
                "claude-code requires 'state' parameter for token exchange".into(),
            )
        })?;
        let client_id = self.client_id();
        let token_url = self.token_url();

        let effective_redirect = if redirect_uri.is_empty() {
            CLAUDE_CODE_DEFAULT_REDIRECT_URI
        } else {
            redirect_uri
        };

        let payload = ClaudeCodeExchangePayload {
            grant_type: "authorization_code",
            code: &actual_code,
            redirect_uri: effective_redirect,
            client_id: &client_id,
            code_verifier,
            state: &state_val,
        };

        let body_bytes = serde_json::to_vec(&payload)
            .map_err(|e| CoreError::Parse(format!("serialize claude-code exchange payload: {e}")))?;

        let mut req = UpstreamRequest::post_json(token_url, body_bytes.into());
        req.headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        req.headers.insert(
            http::header::ACCEPT,
            http::HeaderValue::from_static("application/json, text/plain, */*"),
        );
        req.headers.insert(
            http::header::USER_AGENT,
            http::HeaderValue::from_static(CLAUDE_CODE_AXIOS_USER_AGENT),
        );

        let cancel = CancellationToken::new();
        let resp = upstream_client
            .call(req, TimeoutProfile::OAuth, cancel)
            .await
            .map_err(|e| map_upstream_err(e, "claude-code token exchange"))?;

        let status = resp.status;
        let resp_bytes = resp
            .collect()
            .await
            .map_err(|e| map_upstream_err(e, "claude-code token exchange body"))?;

        check_oauth_status(status, "claude-code", &resp_bytes)?;

        let val: serde_json::Value = serde_json::from_slice(&resp_bytes)
            .map_err(|e| CoreError::Parse(format!("claude-code token parse error: {e}")))?;

        let access_token = val
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CoreError::Parse("missing access_token in token response".into()))?
            .to_string();

        let refresh_token = val
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .map(ToString::to_string);

        let expires_in = val.get("expires_in").and_then(|v| v.as_u64());
        let token_type = val
            .get("token_type")
            .and_then(|v| v.as_str())
            .unwrap_or("Bearer")
            .to_string();
        let scope = val.get("scope").and_then(|v| v.as_str()).map(ToString::to_string);

        let email = val
            .get("account")
            .and_then(|a| a.get("email_address").or_else(|| a.get("email")))
            .and_then(|e| e.as_str())
            .map(ToString::to_string);

        let account_uuid = val
            .get("account")
            .and_then(|a| a.get("uuid"))
            .and_then(|u| u.as_str())
            .map(ToString::to_string);

        let organization_uuid = val
            .get("organization")
            .and_then(|o| o.get("uuid"))
            .and_then(|u| u.as_str())
            .map(ToString::to_string);

        let scopes_vec = parse_scopes(scope.as_deref());

        let id_token_data = serde_json::json!({
            "email": email,
            "account_uuid": account_uuid,
            "organization_uuid": organization_uuid,
            "scopes": scopes_vec,
        });
        let id_token = serde_json::to_string(&id_token_data).ok();

        Ok(TokenResponse {
            access_token,
            token_type,
            expires_in,
            refresh_token,
            scope,
            id_token,
        })
    }

    async fn request_device_code(
        &self,
        _upstream_client: &Arc<UpstreamClient>,
    ) -> Result<crate::oauth::DeviceAuthorizationResponse> {
        Err(CoreError::Validation(
            "claude-code does not support device code flow".into(),
        ))
    }

    async fn poll_device_token(
        &self,
        _device_code: &str,
        _upstream_client: &Arc<UpstreamClient>,
    ) -> Result<Option<TokenResponse>> {
        Err(CoreError::Validation(
            "claude-code does not support device code polling".into(),
        ))
    }

    async fn refresh_token(
        &self,
        refresh_token: &str,
        upstream_client: &Arc<UpstreamClient>,
        _account_id: AccountId,
        _db: DbRef<'_>,
    ) -> Result<TokenResponse> {
        let client_id = self.client_id();
        let token_url = self.token_url();

        let payload = ClaudeCodeRefreshPayload {
            client_id: &client_id,
            grant_type: "refresh_token",
            refresh_token,
            scope: None,
        };

        let body_bytes = serde_json::to_vec(&payload)
            .map_err(|e| CoreError::Parse(format!("serialize claude-code refresh payload: {e}")))?;

        let mut req = UpstreamRequest::post_json(token_url, body_bytes.into());
        req.headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        req.headers.insert(
            http::header::ACCEPT,
            http::HeaderValue::from_static("application/json, text/plain, */*"),
        );
        req.headers.insert(
            http::header::USER_AGENT,
            http::HeaderValue::from_static(CLAUDE_CODE_AXIOS_USER_AGENT),
        );

        let cancel = CancellationToken::new();
        let resp = upstream_client
            .call(req, TimeoutProfile::OAuth, cancel)
            .await
            .map_err(|e| map_upstream_err(e, "claude-code token refresh"))?;

        let status = resp.status;
        let resp_bytes = resp
            .collect()
            .await
            .map_err(|e| map_upstream_err(e, "claude-code token refresh body"))?;

        check_oauth_status(status, "claude-code", &resp_bytes)?;

        let val: serde_json::Value = serde_json::from_slice(&resp_bytes)
            .map_err(|e| CoreError::Parse(format!("claude-code refresh parse error: {e}")))?;

        let access_token = val
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CoreError::Parse("missing access_token in refresh response".into()))?
            .to_string();

        let new_refresh = val
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .map(ToString::to_string)
            .or_else(|| Some(refresh_token.to_string()));

        let expires_in = val.get("expires_in").and_then(|v| v.as_u64());
        let token_type = val
            .get("token_type")
            .and_then(|v| v.as_str())
            .unwrap_or("Bearer")
            .to_string();
        let scope = val.get("scope").and_then(|v| v.as_str()).map(ToString::to_string);

        let email = val
            .get("account")
            .and_then(|a| a.get("email_address").or_else(|| a.get("email")))
            .and_then(|e| e.as_str())
            .map(ToString::to_string);

        let account_uuid = val
            .get("account")
            .and_then(|a| a.get("uuid"))
            .and_then(|u| u.as_str())
            .map(ToString::to_string);

        let organization_uuid = val
            .get("organization")
            .and_then(|o| o.get("uuid"))
            .and_then(|u| u.as_str())
            .map(ToString::to_string);

        let scopes_vec = parse_scopes(scope.as_deref());

        let id_token_data = serde_json::json!({
            "email": email,
            "account_uuid": account_uuid,
            "organization_uuid": organization_uuid,
            "scopes": scopes_vec,
        });
        let id_token = serde_json::to_string(&id_token_data).ok();

        Ok(TokenResponse {
            access_token,
            token_type,
            expires_in,
            refresh_token: new_refresh,
            scope,
            id_token,
        })
    }

    fn provider_specific_from_token(&self, token: &TokenResponse) -> Option<String> {
        let raw = token.id_token.as_ref()?;
        let val: serde_json::Value = serde_json::from_str(raw).ok()?;
        let meta = ClaudeCodeProviderMeta {
            account_uuid: val.get("account_uuid").and_then(|v| v.as_str()).map(ToString::to_string),
            organization_uuid: val.get("organization_uuid").and_then(|v| v.as_str()).map(ToString::to_string),
            scopes: val.get("scopes").and_then(|v| serde_json::from_value(v.clone()).ok()),
            subscription_type: val.get("subscription_type").and_then(|v| v.as_str()).map(ToString::to_string),
            rate_limit_tier: val.get("rate_limit_tier").and_then(|v| v.as_str()).map(ToString::to_string),
        };
        serde_json::to_string(&meta).ok()
    }

    fn email_from_token(&self, token: &TokenResponse) -> Option<String> {
        let raw = token.id_token.as_ref()?;
        let val: serde_json::Value = serde_json::from_str(raw).ok()?;
        val.get("email")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
    }

    async fn post_exchange(
        &self,
        account_id: AccountId,
        db_pool: &Arc<openproxy_db::DbPool>,
        master_key: &MasterKey,
        upstream: &Arc<UpstreamClient>,
    ) -> Result<()> {
        let (access_token, existing_email) = {
            let pool = Arc::clone(db_pool);
            let master = master_key.clone();
            tokio::task::spawn_blocking(move || -> Result<(String, Option<String>)> {
                let r = pool.reader();
                let token = crate::oauth::decrypt_access_token(&r, account_id, &master)?;
                let acc = openproxy_db::accounts::get(&r, account_id, &master)?
                    .ok_or_else(|| CoreError::AccountNotFound(account_id.0))?;
                Ok((token, acc.email.map(|e| e.to_string())))
            })
            .await
            .map_err(|e| CoreError::Internal(format!("spawn_blocking join: {e}")))?
            .map_err(|e| CoreError::Internal(format!("read account tokens: {e}")))?
        };

        let profile_url = self.profile_url();
        let mut req = UpstreamRequest::get(profile_url);
        req.headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_str(&format!("Bearer {access_token}")).map_err(|e| {
                CoreError::Validation(format!("invalid authorization header: {e}"))
            })?,
        );
        req.headers.insert(
            http::header::ACCEPT,
            http::HeaderValue::from_static("application/json, text/plain, */*"),
        );
        req.headers.insert(
            http::header::USER_AGENT,
            http::HeaderValue::from_static(CLAUDE_CODE_AXIOS_USER_AGENT),
        );

        let cancel = CancellationToken::new();
        let profile_res = upstream.call(req, TimeoutProfile::OAuth, cancel).await;

        let mut fetched_email = None;
        let mut account_uuid = None;
        let mut org_uuid = None;
        let mut subscription_type = None;
        let mut rate_limit_tier = None;

        if let Ok(resp) = profile_res
            && resp.status.is_success()
            && let Ok(body) = resp.collect().await
            && let Ok(data) = serde_json::from_slice::<serde_json::Value>(&body)
        {
            if let Some(acc) = data.get("account").and_then(|v| v.as_object()) {
                fetched_email = acc
                    .get("email")
                    .or_else(|| acc.get("email_address"))
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string);
                account_uuid = acc.get("uuid").and_then(|v| v.as_str()).map(ToString::to_string);
                if acc.get("has_claude_max").and_then(|v| v.as_bool()) == Some(true) {
                    subscription_type = Some("max".to_string());
                } else if acc.get("has_claude_pro").and_then(|v| v.as_bool()) == Some(true) {
                    subscription_type = Some("pro".to_string());
                }
            }
            if let Some(org) = data.get("organization").and_then(|v| v.as_object()) {
                org_uuid = org.get("uuid").and_then(|v| v.as_str()).map(ToString::to_string);
                rate_limit_tier = org.get("rate_limit_tier").and_then(|v| v.as_str()).map(ToString::to_string);
                if subscription_type.is_none() {
                    subscription_type = org.get("organization_type").and_then(|v| v.as_str()).map(|t| {
                        match t {
                            "claude_max" => "max".to_string(),
                            "claude_team" => "team".to_string(),
                            "claude_enterprise" => "enterprise".to_string(),
                            _ => "pro".to_string(),
                        }
                    });
                }
            }
        }

        let effective_email = fetched_email.or(existing_email);
        let meta = ClaudeCodeProviderMeta {
            account_uuid,
            organization_uuid: org_uuid,
            scopes: Some(CLAUDE_CODE_SCOPES.iter().map(|s| s.to_string()).collect()),
            subscription_type,
            rate_limit_tier,
        };
        let meta_json = serde_json::to_string(&meta).ok();

        let pool = Arc::clone(db_pool);
        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = pool.writer();
            if let Some(ref email) = effective_email {
                let default_label = format!("claude-code@{email}");
                conn.execute(
                    "UPDATE accounts SET email = ?1, label = COALESCE(NULLIF(label, ''), ?2), \
                     oauth_provider_specific = COALESCE(?3, oauth_provider_specific) WHERE id = ?4",
                    rusqlite::params![email, default_label, meta_json, account_id.0],
                )
                .map_err(|e| CoreError::Database {
                    message: e.to_string(),
                    source: None,
                })?;
            } else if let Some(ref mj) = meta_json {
                conn.execute(
                    "UPDATE accounts SET oauth_provider_specific = ?1 WHERE id = ?2",
                    rusqlite::params![mj, account_id.0],
                )
                .map_err(|e| CoreError::Database {
                    message: e.to_string(),
                    source: None,
                })?;
            }
            Ok(())
        })
        .await
        .map_err(|e| CoreError::Internal(format!("spawn_blocking update: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_claude_code_provider_defaults() {
        let provider = ClaudeCodeOAuthProvider::new();
        assert_eq!(provider.name(), "claude-code");
        assert_eq!(provider.aliases(), &["claude"]);
        assert_eq!(provider.flow(), OAuthFlow::AuthorizationCodePkce);
        assert_eq!(provider.client_id(), CLAUDE_CODE_CLIENT_ID);
    }

    #[tokio::test]
    async fn test_claude_code_build_auth_url() {
        let provider = ClaudeCodeOAuthProvider::new();
        let (url, verifier, redirect, state) = provider
            .build_auth_url("http://localhost:8787/admin/callback.html")
            .await
            .expect("build auth url");

        assert!(url.starts_with("https://claude.ai/oauth/authorize?"));
        assert!(url.contains("client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e"));
        assert!(url.contains("code_challenge="));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(!verifier.is_empty());
        assert!(!state.is_empty());
        assert_eq!(redirect, CLAUDE_CODE_DEFAULT_REDIRECT_URI);
    }

    #[test]
    fn test_claude_code_email_and_provider_specific_from_token() {
        let provider = ClaudeCodeOAuthProvider::new();
        let id_token_json = serde_json::json!({
            "email": "user@example.com",
            "account_uuid": "acc-12345",
            "organization_uuid": "org-67890",
            "scopes": ["org:create_api_key", "user:profile", "user:inference"],
        })
        .to_string();

        let token = TokenResponse {
            access_token: "sk-ant-access".into(),
            token_type: "Bearer".into(),
            expires_in: Some(3600),
            refresh_token: Some("sk-ant-refresh".into()),
            scope: Some("org:create_api_key user:profile user:inference".into()),
            id_token: Some(id_token_json),
        };

        let email = provider.email_from_token(&token);
        assert_eq!(email.as_deref(), Some("user@example.com"));

        let specific = provider.provider_specific_from_token(&token);
        assert!(specific.is_some());
        let meta: ClaudeCodeProviderMeta =
            serde_json::from_str(&specific.unwrap()).expect("parse meta");
        assert_eq!(meta.account_uuid.as_deref(), Some("acc-12345"));
        assert_eq!(meta.organization_uuid.as_deref(), Some("org-67890"));
        assert_eq!(
            meta.scopes.as_deref(),
            Some(&["org:create_api_key".to_string(), "user:profile".to_string(), "user:inference".to_string()][..])
        );
    }

    #[test]
    fn test_claude_code_exchange_payload_wire_format() {
        let payload = ClaudeCodeExchangePayload {
            grant_type: "authorization_code",
            code: "auth-code-123",
            redirect_uri: "https://platform.claude.com/oauth/code/callback",
            client_id: CLAUDE_CODE_CLIENT_ID,
            code_verifier: "pkce-verifier-abc",
            state: "state-xyz",
        };
        let serialized = serde_json::to_string(&payload).expect("serialize exchange payload");
        assert_eq!(
            serialized,
            r#"{"grant_type":"authorization_code","code":"auth-code-123","redirect_uri":"https://platform.claude.com/oauth/code/callback","client_id":"9d1c250a-e61b-44d9-88ed-5944d1962f5e","code_verifier":"pkce-verifier-abc","state":"state-xyz"}"#
        );
    }

    #[test]
    fn test_claude_code_refresh_payload_wire_format() {
        let payload = ClaudeCodeRefreshPayload {
            client_id: CLAUDE_CODE_CLIENT_ID,
            grant_type: "refresh_token",
            refresh_token: "refresh-tok-789",
            scope: None,
        };
        let serialized = serde_json::to_string(&payload).expect("serialize refresh payload");
        assert_eq!(
            serialized,
            r#"{"client_id":"9d1c250a-e61b-44d9-88ed-5944d1962f5e","grant_type":"refresh_token","refresh_token":"refresh-tok-789"}"#
        );

        let payload_with_scope = ClaudeCodeRefreshPayload {
            client_id: CLAUDE_CODE_CLIENT_ID,
            grant_type: "refresh_token",
            refresh_token: "refresh-tok-789",
            scope: Some("user:inference user:profile"),
        };
        let serialized_with_scope =
            serde_json::to_string(&payload_with_scope).expect("serialize refresh payload with scope");
        assert_eq!(
            serialized_with_scope,
            r#"{"client_id":"9d1c250a-e61b-44d9-88ed-5944d1962f5e","grant_type":"refresh_token","refresh_token":"refresh-tok-789","scope":"user:inference user:profile"}"#
        );
    }
}
