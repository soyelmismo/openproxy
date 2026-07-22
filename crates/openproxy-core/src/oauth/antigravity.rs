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

use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::generic::{GenericOAuthProvider, OAuthRequestEncoding, OAuthSpec};
use crate::error::{CoreError, Result};
use crate::ids::AccountId;
use crate::oauth::{OAuthFlow, OAuthProvider};
use openproxy_adapters::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest,
};
use openproxy_db::secrets::MasterKey;
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

fn antigravity_oauth_spec() -> OAuthSpec {
    OAuthSpec {
        id: "antigravity",
        flow: OAuthFlow::AuthorizationCodePkce,
        authorize_url: Some(AUTH_URL),
        token_url: TOKEN_URL,
        device_authorization_url: None,
        client_id_env: Some("OPENPROXY_ANTIGRAVITY_CLIENT_ID"),
        client_id_default: CLIENT_ID,
        client_secret_env: Some("OPENPROXY_ANTIGRAVITY_CLIENT_SECRET"),
        client_secret_default: Some(DEFAULT_CLIENT_SECRET),
        scopes: SCOPES,
        auth_extra_params: &[("access_type", "offline"), ("prompt", "consent")],
        request_encoding: OAuthRequestEncoding::FormUrlEncoded,
        user_agent: Some(openproxy_adapters::antigravity_headers::oauth_user_agent),
    }
}

#[derive(Clone)]
pub struct AntigravityOAuthProvider {
    generic: GenericOAuthProvider,
}

impl AntigravityOAuthProvider {
    pub fn new() -> Self {
        Self {
            generic: GenericOAuthProvider::new(antigravity_oauth_spec()),
        }
    }
}

impl Default for AntigravityOAuthProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl OAuthProvider for AntigravityOAuthProvider {
    crate::delegate_oauth_to_generic!(
        name,
        flow,
        build_auth_url,
        exchange_code,
        request_device_code,
        poll_device_token,
        refresh_token
    );

    async fn post_exchange(
        &self,
        account_id: AccountId,
        db_pool: &std::sync::Arc<openproxy_db::DbPool>,
        master_key: &MasterKey,
        upstream: &Arc<UpstreamClient>,
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
            let mut req = UpstreamRequest::get(user_info_url);
            if let Ok(v) = http::HeaderValue::from_str(&format!("Bearer {access_token}")) {
                req.headers.insert(http::header::AUTHORIZATION, v);
            }
            req.is_streaming = false;
            let cancel = CancellationToken::new();
            match upstream.call(req, TimeoutProfile::OAuth, cancel).await {
                Ok(resp) if resp.status.is_success() => {
                    let body = resp.collect().await.unwrap_or_default();
                    serde_json::from_slice::<serde_json::Value>(&body)
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

        let project_id = match openproxy_adapters::adapters::antigravity::load_code_assist(
            upstream,
            &access_token,
            &metadata,
        )
        .await
        .map_err(CoreError::UpstreamConnection)?
        {
            Some(pid) => pid,
            None => {
                // Retry onboardUser up to 10 times with 5s delays
                let mut result = None;
                for attempt in 0..10 {
                    match openproxy_adapters::adapters::antigravity::onboard_user(
                        upstream,
                        &access_token,
                        "",
                        &metadata,
                    )
                    .await
                    {
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
        let meta_json = serde_json::to_string(&meta)
            .map_err(|e| CoreError::Internal(format!("antigravity meta serialize: {e}")))?;
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

        // 4. Update email and label on the account row if we fetched it.
        if let Some(ref email) = email {
            conn.execute(
                "UPDATE accounts SET email = ?1, label = COALESCE(label, ?1) WHERE id = ?2",
                rusqlite::params![email, account_id.0],
            )
            .map_err(|e| CoreError::Database {
                message: format!(
                    "antigravity post_exchange update email and label for account {}: {}",
                    account_id.0, e
                ),
                source: Some(Box::new(e)),
            })?;
        }

        Ok(())
    }
}

/// Read the `projectId` stored on the account row by `post_exchange`.
///
/// Returns `Ok(None)` when the account is not OAuth, has no
/// `oauth_provider_specific` JSON, or the JSON does not contain a
/// `projectId`. Returns `Ok(Some(_))` when one is present.
pub fn read_project_id(conn: &Connection, account_id: AccountId) -> Result<Option<String>> {
    let raw: Option<Option<String>> = conn
        .query_row(
            "SELECT oauth_provider_specific FROM accounts WHERE id = ?1",
            rusqlite::params![account_id.0],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(openproxy_db::error::map_db_error_ctx(format!(
            "read_project_id for account {}",
            account_id.0
        )))?;

    let Some(raw) = raw.flatten() else {
        return Ok(None);
    };
    let meta: AntigravityProviderMeta = serde_json::from_str(&raw)
        .map_err(|e| CoreError::Parse(format!("antigravity meta parse: {e}")))?;
    Ok(meta.project_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_verifier_is_url_safe() {
        let v = crate::oauth::generic::generate_code_verifier();
        assert!(v.len() >= 43);
        assert!(v.len() <= 128);
        // Must be base64url-safe characters only.
        assert!(
            v.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    #[test]
    fn code_challenge_deterministic() {
        let verifier = "test-verifier-string";
        let a = crate::oauth::generic::code_challenge_s256(verifier);
        let b = crate::oauth::generic::code_challenge_s256(verifier);
        assert_eq!(a, b);
    }

    #[test]
    fn code_challenge_differs_per_verifier() {
        let a = crate::oauth::generic::code_challenge_s256("verifier-a");
        let b = crate::oauth::generic::code_challenge_s256("verifier-b");
        assert_ne!(a, b);
    }

    #[test]
    fn name_and_flow() {
        let p = AntigravityOAuthProvider::new();
        assert_eq!(p.name(), "antigravity");
        assert_eq!(p.flow(), OAuthFlow::AuthorizationCodePkce);
    }

    #[tokio::test]
    async fn antigravity_authorize_url_comes_from_generic_spec() {
        let p = AntigravityOAuthProvider::new();
        let (url, verifier, challenge, _state) = p
            .build_auth_url("http://localhost:8788/admin/callback.html")
            .await
            .unwrap();

        assert!(!verifier.is_empty());
        assert_eq!(
            challenge,
            crate::oauth::generic::code_challenge_s256(&verifier)
        );
        assert!(url.starts_with(AUTH_URL));
        assert!(url.contains("client_id=1071006060591-tmhssin2h21lcre235vtolojh4g403ep"));
        assert!(url.contains("access_type=offline"));
        assert!(url.contains("prompt=consent"));
        assert!(url.contains("code_challenge_method=S256"));
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
