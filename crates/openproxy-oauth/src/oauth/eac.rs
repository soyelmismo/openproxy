//! EAC Gateway OAuth 2.0 provider (Device Code flow).
//!
//! Implements device code authorization against EAC Gateway:
//! - Start: `GET https://ourfreemodel.beichenapi.com/eac/auth/github/start?link={link}`
//! - Poll: `GET https://ourfreemodel.beichenapi.com/eac/auth/poll?link={link}&retain=1`
//! - Ack: `POST https://ourfreemodel.beichenapi.com/eac/auth/ack?link={link}`

use crate::error::{CoreError, Result};
use crate::ids::AccountId;
use crate::oauth::{
    DbRef, DeviceAuthorizationResponse, OAuthFlow, OAuthProvider, TokenResponse, map_upstream_err,
};
use openproxy_adapters::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest,
};
use openproxy_db::secrets::MasterKey;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub const EAC_DEFAULT_AUTH_BASE: &str = "https://ourfreemodel.beichenapi.com/eac";
pub const EAC_DEFAULT_POLL_INTERVAL_SECS: u64 = 3;
pub const EAC_DEFAULT_EXPIRES_IN_SECS: u64 = 900;
pub const EAC_TOKEN_EXPIRES_IN_SECS: u64 = 31_536_000;
pub const EAC_USER_AGENT: &str = "dsh-our-free-model";

/// Strips trailing slashes and optional `/v1` suffix from base URL to obtain
/// the auth root.
pub fn clean_eac_auth_root(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    if let Some(stripped) = trimmed.strip_suffix("/v1") {
        stripped.trim_end_matches('/').to_string()
    } else {
        trimmed.to_string()
    }
}

/// Resolves the EAC auth root from env or default constant.
pub fn resolve_eac_auth_root() -> String {
    if let Ok(env) = std::env::var("OPENPROXY_EAC_BASE_URL")
        && !env.trim().is_empty()
    {
        return clean_eac_auth_root(env.trim());
    }
    if let Ok(env) = std::env::var("EAC_BASE_URL")
        && !env.trim().is_empty()
    {
        return clean_eac_auth_root(env.trim());
    }
    clean_eac_auth_root(EAC_DEFAULT_AUTH_BASE)
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct EacAccountMeta {
    #[serde(default)]
    pub login: String,
    #[serde(default)]
    pub avatar: String,
    #[serde(default = "default_auth_provider")]
    pub auth_provider: String,
}

fn default_auth_provider() -> String {
    "github".to_string()
}

#[derive(Clone, Default)]
pub struct EacOAuthProvider;

impl EacOAuthProvider {
    pub fn new() -> Self {
        Self
    }

    /// Sends confirmation ACK to EAC gateway after token acquisition.
    /// If network or timeout errors occur, the failure is ignored so as not to
    /// invalidate the token already acquired.
    pub async fn send_ack(
        upstream_client: &Arc<UpstreamClient>,
        auth_root: &str,
        device_code: &str,
        user_token: &str,
    ) {
        let encoded_link = urlencoding::encode(device_code);
        let ack_url = format!("{auth_root}/auth/ack?link={encoded_link}");
        let mut req = UpstreamRequest::post_json(&ack_url, bytes::Bytes::new());
        req.headers.insert(
            http::header::ACCEPT,
            http::HeaderValue::from_static("application/json"),
        );
        req.headers.insert(
            http::header::USER_AGENT,
            http::HeaderValue::from_static(EAC_USER_AGENT),
        );
        if let Ok(val) = http::HeaderValue::from_str(user_token) {
            req.headers.insert(http::header::HeaderName::from_static("x-ofm-user"), val);
        }

        let cancel = CancellationToken::new();
        if let Ok(resp) = upstream_client.call(req, TimeoutProfile::OAuth, cancel).await {
            let _ = resp.collect().await;
        }
    }
}

impl OAuthProvider for EacOAuthProvider {
    fn name(&self) -> &'static str {
        "eac"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["eac-gateway", "ofm"]
    }

    fn flow(&self) -> OAuthFlow {
        OAuthFlow::DeviceCode
    }

    fn build_auth_url(
        &self,
        _redirect_uri: &str,
    ) -> impl std::future::Future<Output = Result<(String, String, String, String)>> + Send {
        std::future::ready(Err(CoreError::Validation(
            "EAC Gateway uses device code flow, not authorization code".into(),
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
            "EAC Gateway uses device code flow, not authorization code".into(),
        )))
    }

    async fn request_device_code(
        &self,
        upstream_client: &Arc<UpstreamClient>,
    ) -> Result<DeviceAuthorizationResponse> {
        let link = uuid::Uuid::new_v4().simple().to_string();
        let user_code: String = link.chars().take(8).collect::<String>().to_uppercase();
        let auth_root = resolve_eac_auth_root();
        let start_url = format!("{auth_root}/auth/github/start?link={link}");

        let mut req = UpstreamRequest::get(&start_url);
        req.headers.insert(
            http::header::ACCEPT,
            http::HeaderValue::from_static("application/json, text/html, */*"),
        );
        req.headers.insert(
            http::header::USER_AGENT,
            http::HeaderValue::from_static(EAC_USER_AGENT),
        );

        let cancel = CancellationToken::new();
        let resp = upstream_client
            .call(req, TimeoutProfile::OAuth, cancel)
            .await
            .map_err(|e| map_upstream_err(e, "eac request_device_code start"))?;

        let status = resp.status;
        let _ = resp.collect().await;

        if !status.is_success() && !status.is_redirection() {
            return Err(CoreError::upstream_error(
                status.as_u16(),
                "eac",
                "<oauth_start>",
                format!("EAC start endpoint returned HTTP {status}"),
                false,
            ));
        }

        Ok(DeviceAuthorizationResponse {
            device_code: link,
            user_code,
            verification_uri: start_url.clone(),
            verification_uri_complete: Some(start_url),
            expires_in: Some(EAC_DEFAULT_EXPIRES_IN_SECS),
            interval: Some(EAC_DEFAULT_POLL_INTERVAL_SECS),
        })
    }

    async fn poll_device_token(
        &self,
        device_code: &str,
        upstream_client: &Arc<UpstreamClient>,
    ) -> Result<Option<TokenResponse>> {
        let auth_root = resolve_eac_auth_root();
        let encoded_link = urlencoding::encode(device_code.trim());
        let poll_url = format!("{auth_root}/auth/poll?link={encoded_link}&retain=1");

        let mut req = UpstreamRequest::get(&poll_url);
        req.headers.insert(
            http::header::ACCEPT,
            http::HeaderValue::from_static("application/json"),
        );
        req.headers.insert(
            http::header::USER_AGENT,
            http::HeaderValue::from_static(EAC_USER_AGENT),
        );

        let cancel = CancellationToken::new();
        let response = upstream_client
            .call(req, TimeoutProfile::OAuth, cancel)
            .await
            .map_err(|e| map_upstream_err(e, "eac poll_device_token"))?;

        let status = response.status;
        let body = response
            .collect()
            .await
            .map_err(|e| map_upstream_err(e, "eac poll_device_token body read"))?;

        if !status.is_success() {
            let body_str = String::from_utf8_lossy(&body).to_string();
            return Err(CoreError::upstream_error(
                status.as_u16(),
                "eac",
                "<oauth_poll>",
                body_str,
                false,
            ));
        }

        let json: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|e| CoreError::Parse(format!("eac poll response json parse: {e}")))?;

        let poll_status = json
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");

        match poll_status {
            "pending" | "unstarred" => Ok(None),
            "expired" => Err(CoreError::Validation("device code expired".into())),
            "ok" => {
                let token = json
                    .get("token")
                    .and_then(serde_json::Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| CoreError::Parse("eac poll response missing token".into()))?
                    .trim()
                    .to_string();

                let login = json
                    .get("login")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string();

                let avatar = json
                    .get("avatar")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string();

                let ack_required = json
                    .get("ackRequired")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(true);

                if ack_required {
                    Self::send_ack(upstream_client, &auth_root, device_code.trim(), &token).await;
                }

                let meta = EacAccountMeta {
                    login,
                    avatar,
                    auth_provider: "github".to_string(),
                };
                let id_token = serde_json::to_string(&meta).ok();

                Ok(Some(TokenResponse {
                    access_token: token,
                    token_type: "Bearer".into(),
                    expires_in: Some(EAC_TOKEN_EXPIRES_IN_SECS),
                    refresh_token: None,
                    scope: None,
                    id_token,
                }))
            }
            other => Err(CoreError::Validation(format!(
                "unexpected EAC poll status '{other}'"
            ))),
        }
    }

    fn refresh_token(
        &self,
        _refresh_token: &str,
        _upstream_client: &Arc<UpstreamClient>,
        _account_id: AccountId,
        _db: DbRef<'_>,
    ) -> impl std::future::Future<Output = Result<TokenResponse>> + Send {
        std::future::ready(Err(CoreError::Validation(
            "EAC Gateway does not use refresh tokens; re-authentication required if token expires".into(),
        )))
    }

    fn provider_specific_from_token(&self, token: &TokenResponse) -> Option<String> {
        token.id_token.clone()
    }

    fn email_from_token(&self, token: &TokenResponse) -> Option<String> {
        token
            .id_token
            .as_deref()
            .and_then(|raw| serde_json::from_str::<EacAccountMeta>(raw).ok())
            .map(|m| {
                if !m.login.trim().is_empty() {
                    format!("{}@github.com", m.login.trim())
                } else {
                    String::new()
                }
            })
            .filter(|s| !s.is_empty())
    }

    async fn post_exchange(
        &self,
        account_id: AccountId,
        db_pool: &Arc<openproxy_db::DbPool>,
        _master_key: &MasterKey,
        _upstream: &Arc<UpstreamClient>,
    ) -> Result<()> {
        const WRITER_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
        let pool = Arc::clone(db_pool);

        tokio::task::spawn_blocking(move || {
            let conn = pool.try_writer_for(WRITER_LOCK_TIMEOUT).ok_or_else(|| {
                CoreError::Internal(format!(
                    "eac post_exchange: writer lock not acquired within {WRITER_LOCK_TIMEOUT:?}"
                ))
            })?;

            let meta_raw: Option<String> = conn
                .query_row(
                    "SELECT oauth_provider_specific FROM accounts WHERE id = ?1",
                    rusqlite::params![account_id.0],
                    |row| row.get(0),
                )
                .ok()
                .flatten();

            if let Some(raw) = meta_raw
                && let Ok(meta) = serde_json::from_str::<EacAccountMeta>(&raw)
            {
                let login = meta.login.trim();
                if !login.is_empty() {
                    let label = format!("EAC ({login})");
                    let _ = conn.execute(
                        "UPDATE accounts SET label = COALESCE(NULLIF(label, ''), ?1) WHERE id = ?2",
                        rusqlite::params![label, account_id.0],
                    );
                }
            }

            Ok::<(), CoreError>(())
        })
        .await
        .map_err(|e| CoreError::Internal(format!("eac post_exchange spawn join error: {e}")))??;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clean_eac_auth_root() {
        assert_eq!(
            clean_eac_auth_root("https://ourfreemodel.beichenapi.com/eac/v1"),
            "https://ourfreemodel.beichenapi.com/eac"
        );
        assert_eq!(
            clean_eac_auth_root("https://ourfreemodel.beichenapi.com/eac/v1/"),
            "https://ourfreemodel.beichenapi.com/eac"
        );
        assert_eq!(
            clean_eac_auth_root("https://ourfreemodel.beichenapi.com/eac"),
            "https://ourfreemodel.beichenapi.com/eac"
        );
        assert_eq!(
            clean_eac_auth_root("https://ourfreemodel.beichenapi.com/eac/"),
            "https://ourfreemodel.beichenapi.com/eac"
        );
    }

    #[test]
    fn test_eac_provider_basics() {
        let provider = EacOAuthProvider::new();
        assert_eq!(provider.name(), "eac");
        assert_eq!(provider.aliases(), &["eac-gateway", "ofm"]);
        assert_eq!(provider.flow(), OAuthFlow::DeviceCode);
    }

    #[test]
    fn test_eac_metadata_and_email_extraction() {
        let provider = EacOAuthProvider::new();
        let meta = EacAccountMeta {
            login: "octocat".into(),
            avatar: "https://github.com/images/octocat.png".into(),
            auth_provider: "github".into(),
        };

        let id_token = serde_json::to_string(&meta).unwrap();
        let token = TokenResponse {
            access_token: "user-token-xyz".into(),
            token_type: "Bearer".into(),
            expires_in: Some(31_536_000),
            refresh_token: None,
            scope: None,
            id_token: Some(id_token.clone()),
        };

        assert_eq!(
            provider.provider_specific_from_token(&token).as_deref(),
            Some(id_token.as_str())
        );
        assert_eq!(
            provider.email_from_token(&token).as_deref(),
            Some("octocat@github.com")
        );
    }

    #[test]
    fn test_safe_user_code_derivation() {
        let link = "0123456789abcdef0123456789abcdef";
        let user_code: String = link.chars().take(8).collect::<String>().to_uppercase();
        assert_eq!(user_code, "01234567");
    }

    #[test]
    fn test_parse_poll_json_ok() {
        let json_raw = r#"{
            "status": "ok",
            "token": "tok_12345",
            "login": "dev_user",
            "avatar": "https://avatar.url",
            "ackRequired": true
        }"#;

        let json: serde_json::Value = serde_json::from_str(json_raw).unwrap();
        assert_eq!(json.get("status").and_then(|v| v.as_str()), Some("ok"));
        assert_eq!(json.get("token").and_then(|v| v.as_str()), Some("tok_12345"));
        assert_eq!(json.get("login").and_then(|v| v.as_str()), Some("dev_user"));
    }
}
