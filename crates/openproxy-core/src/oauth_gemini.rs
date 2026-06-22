//! Gemini CLI OAuth provider.
//!
//! Uses standard Authorization Code (no PKCE) against Google's OAuth2
//! endpoints, with a client_secret required for the code exchange.
//! After a successful token exchange the provider calls `loadCodeAssist`
//! to recover the user's project ID and stores it in
//! `accounts.oauth_provider_specific` as JSON: `{"projectId": "..."}`.
//!
//! Unlike Antigravity (which uses PKCE), Gemini CLI embeds its
//! client_secret in the public CLI binary — Google's documented pattern
//! for installed/desktop applications. On the server side the secret
//! is configured via `OPENPROXY_GEMINI_CLI_OAUTH_CLIENT_SECRET`.

use async_trait::async_trait;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};
use crate::ids::AccountId;
use crate::oauth::{OAuthFlow, OAuthProvider, TokenResponse};
use crate::secrets::MasterKey;
use crate::upstream::{CancellationToken, TimeoutProfile, UpstreamClient, UpstreamError, UpstreamRequest};
use std::sync::Arc;

/// Google OAuth client_id for Gemini CLI. Shipped with the Gemini CLI
/// binary (public credential — Google native-app convention). Override
/// via `OPENPROXY_GEMINI_CLI_OAUTH_CLIENT_ID` env var.
const DEFAULT_CLIENT_ID: &str = "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j";

/// Public client_secret shipped with the Gemini CLI binary. Override
/// via `OPENPROXY_GEMINI_CLI_OAUTH_CLIENT_SECRET` env var.
const DEFAULT_CLIENT_SECRET: &str = "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";

/// Google OAuth scopes for Gemini CLI (no `cclog` or `experimentsandconfigs`).
const SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
];

/// Google OAuth endpoints.
const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// LoadCodeAssist endpoint for Gemini CLI. Uses a different subdomain
/// (`cloudcode-pa.googleapis.com`) than Antigravity (`daily-cloudcode-pa`).
const LOAD_CODE_ASSIST_URL: &str =
    "https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist";

/// Provider metadata persisted in `accounts.oauth_provider_specific`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GeminiCliProviderMeta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
}

pub struct GeminiCliOAuthProvider;

impl GeminiCliOAuthProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Default for GeminiCliOAuthProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolve client_id from env var or default.
fn client_id() -> String {
    std::env::var("OPENPROXY_GEMINI_CLI_OAUTH_CLIENT_ID")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_CLIENT_ID.to_string())
}

/// Resolve client_secret from env var or default.
fn client_secret() -> String {
    std::env::var("OPENPROXY_GEMINI_CLI_OAUTH_CLIENT_SECRET")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_CLIENT_SECRET.to_string())
}

/// Build an `application/x-www-form-urlencoded` body from `(name, value)` pairs.
fn urlencoded_body(params: &[(&str, &str)]) -> bytes::Bytes {
    let mut s = String::new();
    for (i, (k, v)) in params.iter().enumerate() {
        if i > 0 {
            s.push('&');
        }
        s.push_str(&urlencoding::encode(k));
        s.push('=');
        s.push_str(&urlencoding::encode(v));
    }
    bytes::Bytes::from(s)
}

#[async_trait]
impl OAuthProvider for GeminiCliOAuthProvider {
    fn name(&self) -> &str {
        "gemini-cli"
    }

    fn flow(&self) -> OAuthFlow {
        OAuthFlow::AuthorizationCode
    }

    async fn build_auth_url(
        &self,
        redirect_uri: &str,
    ) -> Result<(String, String, String)> {
        let cid = client_id();
        if cid.is_empty() {
            return Err(CoreError::Validation(
                "Gemini CLI OAuth requires OPENPROXY_GEMINI_CLI_OAUTH_CLIENT_ID to be set".into(),
            ));
        }

        let scope = SCOPES.join(" ");
        let auth_url = format!(
            "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&access_type=offline&prompt=consent",
            AUTH_URL,
            urlencoding::encode(&cid),
            urlencoding::encode(redirect_uri),
            urlencoding::encode(&scope),
        );

        // No PKCE verifier/challenge — returns empty strings.
        Ok((auth_url, String::new(), String::new()))
    }

    async fn exchange_code(
        &self,
        code: &str,
        code_verifier: &str,
        upstream_client: &Arc<UpstreamClient>,
        redirect_uri: &str,
    ) -> Result<TokenResponse> {
        let cid = client_id();
        let cs = client_secret();
        if cid.is_empty() {
            return Err(CoreError::Validation(
                "Gemini CLI OAuth requires OPENPROXY_GEMINI_CLI_OAUTH_CLIENT_ID".into(),
            ));
        }
        if cs.is_empty() {
            return Err(CoreError::Validation(
                "Gemini CLI OAuth requires OPENPROXY_GEMINI_CLI_OAUTH_CLIENT_SECRET "
                    .to_string()
                    + "— standard authorization_code (non-PKCE) flow requires client_secret. "
                    + "See https://console.cloud.google.com/apis/credentials",
            ));
        }

        let mut params = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("client_id", &cid),
            ("client_secret", &cs),
            ("redirect_uri", redirect_uri),
        ];

        // code_verifier is empty for non-PKCE flows; if somehow provided,
        // include it (some providers accept both).
        if !code_verifier.is_empty() {
            params.push(("code_verifier", code_verifier));
        }

        let body = urlencoded_body(&params);
        let mut req = UpstreamRequest::post_json(TOKEN_URL, body);
        req.headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/x-www-form-urlencoded"),
        );
        req.headers.insert(
            http::header::USER_AGENT,
            http::HeaderValue::from_static("google-api-nodejs-client/10.3.0 (Gemini CLI)"),
        );

        let cancel = CancellationToken::new();
        let response = match upstream_client
            .call(req, TimeoutProfile::OAuth, cancel)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return Err(match e {
                    UpstreamError::Cancel => CoreError::ClientDisconnected,
                    other => CoreError::UpstreamConnection(format!(
                        "gemini token exchange: {other}"
                    )),
                });
            }
        };

        let status = response.status;
        let body_bytes = match response.collect().await {
            Ok(b) => b,
            Err(e) => {
                return Err(match e {
                    UpstreamError::Cancel => CoreError::ClientDisconnected,
                    other => CoreError::UpstreamConnection(format!(
                        "gemini token exchange body read: {other}"
                    )),
                });
            }
        };

        if !status.is_success() {
            let body_str = String::from_utf8_lossy(&body_bytes).to_string();
            return Err(CoreError::UpstreamError {
                status: status.as_u16(),
                provider: "gemini-cli".into(),
                model: "<oauth>".into(),
                body: body_str,
            });
        }

        serde_json::from_slice::<TokenResponse>(&body_bytes)
            .map_err(|e| CoreError::Parse(format!("gemini token response parse: {e}")))
    }

    async fn request_device_code(
        &self,
        _upstream_client: &Arc<UpstreamClient>,
    ) -> Result<crate::oauth::DeviceAuthorizationResponse> {
        Err(CoreError::Validation(
            "gemini-cli uses authorization code flow, not device code".into(),
        ))
    }

    async fn poll_device_token(
        &self,
        _device_code: &str,
        _upstream_client: &Arc<UpstreamClient>,
    ) -> Result<Option<TokenResponse>> {
        Err(CoreError::Validation(
            "gemini-cli uses authorization code flow, not device code".into(),
        ))
    }

    async fn refresh_token(
        &self,
        refresh_token: &str,
        upstream_client: &Arc<UpstreamClient>,
    ) -> Result<TokenResponse> {
        let cid = client_id();
        if cid.is_empty() {
            return Err(CoreError::Validation(
                "Gemini CLI OAuth requires OPENPROXY_GEMINI_CLI_OAUTH_CLIENT_ID".into(),
            ));
        }
        let cs = client_secret();
        if cs.is_empty() {
            return Err(CoreError::Validation(
                "Gemini CLI OAuth requires OPENPROXY_GEMINI_CLI_OAUTH_CLIENT_SECRET".into(),
            ));
        }

        let params = [
            ("grant_type", "refresh_token"),
            ("client_id", &cid),
            ("client_secret", &cs),
            ("refresh_token", refresh_token),
        ];

        let body = urlencoded_body(&params);
        let mut req = UpstreamRequest::post_json(TOKEN_URL, body);
        req.headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/x-www-form-urlencoded"),
        );

        let cancel = CancellationToken::new();
        let response = match upstream_client
            .call(req, TimeoutProfile::OAuth, cancel)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return Err(match e {
                    UpstreamError::Cancel => CoreError::ClientDisconnected,
                    other => CoreError::UpstreamConnection(format!(
                        "gemini token refresh: {other}"
                    )),
                });
            }
        };

        let status = response.status;
        let body_bytes = match response.collect().await {
            Ok(b) => b,
            Err(e) => {
                return Err(match e {
                    UpstreamError::Cancel => CoreError::ClientDisconnected,
                    other => CoreError::UpstreamConnection(format!(
                        "gemini token refresh body read: {other}"
                    )),
                });
            }
        };

        if !status.is_success() {
            let body_str = String::from_utf8_lossy(&body_bytes).to_string();
            return Err(CoreError::UpstreamError {
                status: status.as_u16(),
                provider: "gemini-cli".into(),
                model: "<oauth>".into(),
                body: body_str,
            });
        }

        serde_json::from_slice::<TokenResponse>(&body_bytes)
            .map_err(|e| CoreError::Parse(format!("gemini token refresh parse: {e}")))
    }

    async fn post_exchange(
        &self,
        account_id: AccountId,
        db_pool: &std::sync::Arc<crate::db::DbPool>,
        master_key: &MasterKey,
    ) -> Result<()> {
        // 1. Decrypt the access token we just stored.
        let access_token = {
            let conn = db_pool.writer();
            crate::accounts::decrypt_access_token(&conn, account_id, master_key)?
        };

        // 2. Fetch user info from Google.
        let email = {
            let user_info_url = "https://www.googleapis.com/oauth2/v1/userinfo?alt=json";
            let http_client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .map_err(|e| {
                    CoreError::UpstreamConnection(format!("gemini userinfo client: {e}"))
                })?;
            match http_client
                .get(user_info_url)
                .header("Authorization", format!("Bearer {access_token}"))
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    resp.json::<serde_json::Value>().await
                        .ok()
                        .and_then(|v| v.get("email").and_then(|e| e.as_str()).map(String::from))
                }
                _ => None,
            }
        };

        // 3. Call loadCodeAssist on cloudcode-pa.googleapis.com
        let metadata = serde_json::json!({
            "ideType": "GEMINI_CLI",
        });

        let project_id = load_code_assist(&access_token, &metadata).await?;

        // 4. Persist projectId on the account row.
        let meta = GeminiCliProviderMeta { project_id };
        let meta_json = serde_json::to_string(&meta).map_err(|e| {
            CoreError::Internal(format!("gemini meta serialize: {e}"))
        })?;
        {
            let conn = db_pool.writer();
            conn.execute(
                "UPDATE accounts SET oauth_provider_specific = ?1 WHERE id = ?2",
                rusqlite::params![meta_json, account_id.0],
            )
            .map_err(|e| CoreError::Database {
                message: format!(
                    "gemini post_exchange update project_id for account {}: {}",
                    account_id.0, e
                ),
                source: Some(Box::new(e)),
            })?;

            // 5. Update email if we fetched one.
            if let Some(ref email) = email {
                conn.execute(
                    "UPDATE accounts SET email = ?1 WHERE id = ?2",
                    rusqlite::params![email, account_id.0],
                )
                .map_err(|e| CoreError::Database {
                    message: format!(
                        "gemini post_exchange update email for account {}: {}",
                        account_id.0, e
                    ),
                    source: Some(Box::new(e)),
                })?;
            }
        }

        Ok(())
    }
}

/// Call `loadCodeAssist` and extract `projectId`.
async fn load_code_assist(
    access_token: &str,
    metadata: &serde_json::Value,
) -> Result<Option<String>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| {
            CoreError::UpstreamConnection(format!("gemini load client: {e}"))
        })?;

    let body = serde_json::json!({ "metadata": metadata });

    let resp = client
        .post(LOAD_CODE_ASSIST_URL)
        .bearer_auth(access_token)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| {
            CoreError::UpstreamConnection(format!("gemini loadCodeAssist: {e}"))
        })?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        return Err(CoreError::UpstreamError {
            status,
            provider: "gemini-cli".into(),
            model: "<post_exchange>".into(),
            body: text,
        });
    }

    let value: serde_json::Value = resp.json().await.map_err(|e| {
        CoreError::Parse(format!("gemini loadCodeAssist parse: {e}"))
    })?;

    // `cloudaicompanionProject` may be a string or an object with `id`.
    let project_id = value
        .get("cloudaicompanionProject")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            value
                .get("cloudaicompanionProject")
                .and_then(|v| v.get("id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        });

    Ok(project_id)
}

/// Read the `projectId` stored on the account row by `post_exchange`.
pub fn read_project_id(conn: &rusqlite::Connection, account_id: AccountId) -> Result<Option<String>> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT oauth_provider_specific FROM accounts WHERE id = ?1",
            rusqlite::params![account_id.0],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| CoreError::Database {
            message: format!(
                "gemini_cli read_project_id for account {}: {}",
                account_id.0, e
            ),
            source: Some(Box::new(e)),
        })?;

    let Some(raw) = raw else { return Ok(None) };
    let meta: GeminiCliProviderMeta = serde_json::from_str(&raw).map_err(|e| {
        CoreError::Parse(format!("gemini meta parse: {e}"))
    })?;
    Ok(meta.project_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_and_flow() {
        let p = GeminiCliOAuthProvider::new();
        assert_eq!(p.name(), "gemini-cli");
        assert_eq!(p.flow(), OAuthFlow::AuthorizationCode);
    }

    #[test]
    fn build_auth_url_uses_default_client_id() {
        // The default client_id is hardcoded, so the URL should be built
        // successfully even without the env var.
        // SAFETY: tests are single-threaded; no other thread reads this env var.
        unsafe { std::env::remove_var("OPENPROXY_GEMINI_CLI_OAUTH_CLIENT_ID"); }
        let p = GeminiCliOAuthProvider::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(p.build_auth_url("http://localhost:8788/callback.html"));
        assert!(result.is_ok());
        let (url, verifier, challenge) = result.unwrap();
        assert!(url.contains("client_id=681255809395"));
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A8788%2Fcallback.html"));
        assert!(url.contains("access_type=offline"));
        // Non-PKCE: verifier and challenge are empty.
        assert_eq!(verifier, "");
        assert_eq!(challenge, "");
    }
}
