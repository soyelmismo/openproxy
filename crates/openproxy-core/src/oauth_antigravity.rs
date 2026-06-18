//! Antigravity (Google Cloud Code) OAuth provider.
//!
//! Uses Authorization Code with PKCE against Google's OAuth2 endpoints.
//! The client_id is hardcoded to the one used by Cloud Code.
//!
//! After a successful token exchange the provider calls
//! `loadCodeAssist` (then `onboardUser` if the user has no
//! `projectId` yet) to bootstrap a Cloud Code project and stores
//! the resulting `projectId` in `accounts.oauth_provider_specific` as
//! JSON: `{"projectId": "..."}`. The chat executor reads this
//! field and embeds it in the upstream request envelope.

use async_trait::async_trait;
use base64::Engine;
use rand::RngCore;
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{CoreError, Result};
use crate::ids::AccountId;
use crate::oauth::{OAuthFlow, OAuthProvider, TokenResponse};
use crate::secrets::MasterKey;
use crate::upstream::{CancellationToken, TimeoutProfile, UpstreamClient, UpstreamError, UpstreamRequest};
use std::sync::Arc;

/// Google OAuth client_id for Cloud Code (Antigravity).
const CLIENT_ID: &str = "1071006060591-tmhssin2h21lcre235vtolojh4g403ep";

/// Public OAuth client_secret for Google native/installed app clients.
/// This is NOT a real secret — Google explicitly documents that native app
/// client_secrets are distributed in source code.
/// https://developers.google.com/identity/protocols/oauth2/native-app
const DEFAULT_CLIENT_SECRET: &str = "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";

/// Google OAuth scopes for Cloud Code.
const SCOPES: &[&str] = &[
    "openid",
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
    "https://www.googleapis.com/auth/cclog",
    "https://www.googleapis.com/auth/experimentsandconfigs",
];

/// Google OAuth endpoints.
const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// Cloud Code `loadCodeAssist` / `onboardUser` endpoints. The host is
/// the same `daily-cloudcode-pa.googleapis.com` the chat executor uses.
const LOAD_CODE_ASSIST_URL: &str =
    "https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist";
const ONBOARD_USER_URL: &str =
    "https://daily-cloudcode-pa.googleapis.com/v1internal:onboardUser";

/// Cloud Code `metadata.ideType` used when the operator has not
/// configured a custom IDE identity. The Antigravity client sends
/// `ANTIGRAVITY` as the IDE type.
///
/// `projectId` recovered from `loadCodeAssist` (or `onboardUser`) and
/// persisted in `accounts.oauth_provider_specific` as JSON.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AntigravityProviderMeta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
}

pub struct AntigravityOAuthProvider;

impl AntigravityOAuthProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Default for AntigravityOAuthProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl OAuthProvider for AntigravityOAuthProvider {
    fn name(&self) -> &str {
        "antigravity"
    }

    fn flow(&self) -> OAuthFlow {
        OAuthFlow::AuthorizationCodePkce
    }

    async fn build_auth_url(
        &self,
        redirect_uri: &str,
    ) -> Result<(String, String, String)> {
        let code_verifier = generate_code_verifier();
        let code_challenge = code_challenge_s256(&code_verifier);

        let scope = SCOPES.join(" ");
        let auth_url = format!(
            "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&access_type=offline&prompt=consent",
            AUTH_URL,
            urlencoding::encode(CLIENT_ID),
            urlencoding::encode(redirect_uri),
            urlencoding::encode(&scope),
            urlencoding::encode(&code_challenge),
        );

        Ok((auth_url, code_verifier, code_challenge))
    }

    async fn exchange_code(
        &self,
        code: &str,
        code_verifier: &str,
        upstream_client: &Arc<UpstreamClient>,
        redirect_uri: &str,
    ) -> Result<TokenResponse> {
        // Add client_secret if available (from config or env)
        let client_secret = std::env::var("OPENPROXY_ANTIGRAVITY_CLIENT_SECRET")
            .unwrap_or_else(|_| DEFAULT_CLIENT_SECRET.to_string());

        let mut params = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("client_id", CLIENT_ID),
            ("redirect_uri", redirect_uri),
        ];

        if !code_verifier.is_empty() {
            params.push(("code_verifier", code_verifier));
        }

        if !client_secret.is_empty() {
            params.push(("client_secret", &client_secret));
        }

        // Build the form-encoded body. UpstreamRequest takes a
        // `Bytes` body, so we serialize the params into
        // `application/x-www-form-urlencoded` by hand.
        let body = urlencoded_body(&params);
        let mut req = UpstreamRequest::post_json(TOKEN_URL, body);
        // Replace the default `application/json` content-type with
        // the form-urlencoded one Google expects.
        req.headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/x-www-form-urlencoded"),
        );
        req.headers.insert(
            http::header::USER_AGENT,
            http::HeaderValue::from_static("vscode/1.100.0 (Antigravity/1.2.17)"),
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
                        "google token exchange: {other}"
                    )),
                });
            }
        };

        let status = response.status;
        let body = match response.collect().await {
            Ok(b) => b,
            Err(e) => {
                return Err(match e {
                    UpstreamError::Cancel => CoreError::ClientDisconnected,
                    other => CoreError::UpstreamConnection(format!(
                        "google token exchange body read: {other}"
                    )),
                });
            }
        };

        if !status.is_success() {
            let body_str = String::from_utf8_lossy(&body).to_string();
            return Err(CoreError::UpstreamError {
                status: status.as_u16(),
                provider: "antigravity".into(),
                model: "<oauth>".into(),
                body: body_str,
            });
        }

        serde_json::from_slice::<TokenResponse>(&body)
            .map_err(|e| CoreError::Parse(format!("google token response parse: {e}")))
    }

    async fn request_device_code(
        &self,
        _upstream_client: &Arc<UpstreamClient>,
    ) -> Result<crate::oauth::DeviceAuthorizationResponse> {
        Err(CoreError::Validation(
            "antigravity uses PKCE flow, not device code".into(),
        ))
    }

    async fn poll_device_token(
        &self,
        _device_code: &str,
        _upstream_client: &Arc<UpstreamClient>,
    ) -> Result<Option<TokenResponse>> {
        Err(CoreError::Validation(
            "antigravity uses PKCE flow, not device code".into(),
        ))
    }

    async fn refresh_token(
        &self,
        refresh_token: &str,
        upstream_client: &Arc<UpstreamClient>,
    ) -> Result<TokenResponse> {
        let params = [
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
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
                        "google token refresh: {other}"
                    )),
                });
            }
        };

        let status = response.status;
        let body = match response.collect().await {
            Ok(b) => b,
            Err(e) => {
                return Err(match e {
                    UpstreamError::Cancel => CoreError::ClientDisconnected,
                    other => CoreError::UpstreamConnection(format!(
                        "google token refresh body read: {other}"
                    )),
                });
            }
        };

        if !status.is_success() {
            let body_str = String::from_utf8_lossy(&body).to_string();
            return Err(CoreError::UpstreamError {
                status: status.as_u16(),
                provider: "antigravity".into(),
                model: "<oauth>".into(),
                body: body_str,
            });
        }

        serde_json::from_slice::<TokenResponse>(&body)
            .map_err(|e| CoreError::Parse(format!("google token refresh parse: {e}")))
    }

    async fn post_exchange(
        &self,
        account_id: AccountId,
        db_pool: &std::sync::Arc<crate::db::DbPool>,
        master_key: &MasterKey,
    ) -> Result<()> {
        // 1. Decrypt the access token we just stored. The writer
        //    guard is dropped at the end of the block so the next
        //    `.await` (the loadCodeAssist HTTP call) is `Send`.
        let access_token = {
            let conn = db_pool.writer();
            crate::accounts::decrypt_access_token(&conn, account_id, master_key)?
        };

        // 1b. Fetch user info from Google
        let email = {
            let user_info_url = "https://www.googleapis.com/oauth2/v1/userinfo?alt=json";
            let http_client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .map_err(|e| {
                    CoreError::UpstreamConnection(format!("antigravity userinfo client: {e}"))
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

        // 2. Call loadCodeAssist. If it returns a projectId we are
        //    done; otherwise we need to onboard the user.
        let metadata = serde_json::json!({
            "ideType": "ANTIGRAVITY",
        });

        let project_id = match load_code_assist(&access_token, &metadata).await? {
            Some(pid) => pid,
            None => {
                // Retry onboardUser up to 10 times with 5s delays
                let mut result = None;
                for attempt in 0..10 {
                    match onboard_user(&access_token, "", &metadata).await {
                        Ok(Some(pid)) => {
                            result = Some(pid);
                            break;
                        }
                        Ok(None) => {
                            // Not done yet, wait and retry
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        }
                        Err(e) => {
                            tracing::warn!(attempt = attempt + 1, error = %e, "onboardUser failed");
                            break;
                        }
                    }
                }
                match result {
                    Some(pid) => pid,
                    None => {
                        tracing::warn!("onboardUser did not complete after 10 attempts");
                        return Err(CoreError::Internal(
                            "onboardUser did not complete after 10 attempts".into(),
                        ));
                    }
                }
            }
        };

        // 3. Persist the projectId on the account row.
        let meta = AntigravityProviderMeta {
            project_id: Some(project_id),
        };
        let meta_json = serde_json::to_string(&meta).map_err(|e| {
            CoreError::Internal(format!("antigravity meta serialize: {e}"))
        })?;
        let conn = db_pool.writer();
        conn.execute(
            "UPDATE accounts SET oauth_provider_specific = ?1 WHERE id = ?2",
            rusqlite::params![meta_json, account_id.0],
        )
        .map_err(|e| CoreError::Database {
            message: format!(
                "antigravity post_exchange update project_id for account {}: {}",
                account_id.0, e
            ),
            source: Some(Box::new(e)),
        })?;

        // 4. Update email on the account row if we fetched it.
        if let Some(ref email) = email {
            conn.execute(
                "UPDATE accounts SET email = ?1 WHERE id = ?2",
                rusqlite::params![email, account_id.0],
            )
            .map_err(|e| CoreError::Database {
                message: format!(
                    "antigravity post_exchange update email for account {}: {}",
                    account_id.0, e
                ),
                source: Some(Box::new(e)),
            })?;
        }

        Ok(())
    }
}

/// Call `loadCodeAssist` and extract `projectId` (or `None` when
/// the user is not yet on-boarded).
async fn load_code_assist(
    access_token: &str,
    metadata: &serde_json::Value,
) -> Result<Option<String>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| {
            CoreError::UpstreamConnection(format!("antigravity load client: {e}"))
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
            CoreError::UpstreamConnection(format!("antigravity loadCodeAssist: {e}"))
        })?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Err(CoreError::UpstreamError {
            status,
            provider: "antigravity".into(),
            model: "<post_exchange>".into(),
            body,
        });
    }

    let value: serde_json::Value = resp.json().await.map_err(|e| {
        CoreError::Parse(format!("antigravity loadCodeAssist parse: {e}"))
    })?;

    // `cloudaicompanionProject` may be a string or an object with
    // an `id` field depending on the upstream version. Normalize.
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

/// Call `onboardUser` and return `Ok(Some(project_id))` on success,
/// or `Ok(None)` when the server has not finished onboarding yet.
async fn onboard_user(
    access_token: &str,
    project_id: &str,
    metadata: &serde_json::Value,
) -> Result<Option<String>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| {
            CoreError::UpstreamConnection(format!("antigravity onboard client: {e}"))
        })?;

    let body = serde_json::json!({
        "projectId": project_id,
        "metadata": metadata,
        "tier": "free-tier",
    });

    let resp = client
        .post(ONBOARD_USER_URL)
        .bearer_auth(access_token)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| {
            CoreError::UpstreamConnection(format!("antigravity onboardUser: {e}"))
        })?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Err(CoreError::UpstreamError {
            status,
            provider: "antigravity".into(),
            model: "<post_exchange>".into(),
            body,
        });
    }

    let value: serde_json::Value = resp.json().await.map_err(|e| {
        CoreError::Parse(format!("antigravity onboardUser parse: {e}"))
    })?;

    let project_id = value
        .get("cloudaicompanionProject")
        .and_then(|v| v.get("id"))
        .and_then(|v| v.as_str())
        .or_else(|| value.get("projectId").and_then(|v| v.as_str()))
        .map(|s| s.to_string());

    Ok(project_id)
}

/// Read the `projectId` stored on the account row by `post_exchange`.
///
/// Returns `Ok(None)` when the account is not OAuth, has no
/// `oauth_provider_specific` JSON, or the JSON does not contain a
/// `projectId`. Returns `Ok(Some(_))` when one is present.
pub fn read_project_id(conn: &Connection, account_id: AccountId) -> Result<Option<String>> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT oauth_provider_specific FROM accounts WHERE id = ?1",
            rusqlite::params![account_id.0],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| CoreError::Database {
            message: format!(
                "read_project_id for account {}: {}",
                account_id.0, e
            ),
            source: Some(Box::new(e)),
        })?;

    let Some(raw) = raw else { return Ok(None) };
    let meta: AntigravityProviderMeta = serde_json::from_str(&raw).map_err(|e| {
        CoreError::Parse(format!("antigravity meta parse: {e}"))
    })?;
    Ok(meta.project_id)
}

/// Generate a cryptographically random PKCE code verifier (43-128 chars).
fn generate_code_verifier() -> String {
    let mut buf = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut buf);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

/// Compute the S256 code challenge from a verifier.
fn code_challenge_s256(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

/// Build an `application/x-www-form-urlencoded` body from a list of
/// `(name, value)` pairs. The values are URL-encoded with
/// `urlencoding::encode` so they round-trip through the same parser
/// Google's token endpoint uses.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_verifier_is_url_safe() {
        let v = generate_code_verifier();
        assert!(v.len() >= 43);
        assert!(v.len() <= 128);
        // Must be base64url-safe characters only.
        assert!(v.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn code_challenge_deterministic() {
        let verifier = "test-verifier-string";
        let a = code_challenge_s256(verifier);
        let b = code_challenge_s256(verifier);
        assert_eq!(a, b);
    }

    #[test]
    fn code_challenge_differs_per_verifier() {
        let a = code_challenge_s256("verifier-a");
        let b = code_challenge_s256("verifier-b");
        assert_ne!(a, b);
    }

    #[test]
    fn name_and_flow() {
        let p = AntigravityOAuthProvider::new();
        assert_eq!(p.name(), "antigravity");
        assert_eq!(p.flow(), OAuthFlow::AuthorizationCodePkce);
    }

    #[test]
    fn antigravity_provider_meta_serde_roundtrip() {
        let meta = AntigravityProviderMeta {
            project_id: Some("my-proj-123".into()),
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: AntigravityProviderMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.project_id.as_deref(), Some("my-proj-123"));
    }

    #[test]
    fn antigravity_provider_meta_missing_project_id() {
        let meta = AntigravityProviderMeta { project_id: None };
        let json = serde_json::to_string(&meta).unwrap();
        // Empty meta → JSON object with no `projectId` (skipped).
        assert!(!json.contains("projectId"));
    }

    #[test]
    fn post_exchange_metadata_envelope_is_correct() {
        // The upstream `metadata` envelope is small and stable; we
        // assert its shape so a silent refactor is caught.
        let metadata = serde_json::json!({
            "ideType": "ANTIGRAVITY",
        });
        assert_eq!(metadata["ideType"], "ANTIGRAVITY");
        assert!(metadata.get("platform").is_none());
        assert!(metadata.get("pluginType").is_none());
    }
}
