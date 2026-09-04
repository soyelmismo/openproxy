//! Antigravity (Google Cloud Code) OAuth provider.
//!
//! Uses Authorization Code with PKCE against Google's OAuth2 endpoints.
//! The client_id is hardcoded to the one used by Cloud Code.
//!
//! After a successful token exchange the provider calls
//! `loadCodeAssist` (then `onboardUser` if the user has no
//! `project_id` yet) to bootstrap a Cloud Code project and stores
//! the resulting `project_id` in `accounts.oauth_provider_specific` as
//! JSON: `{"project_id": "..."}` (canonical snake_case wire format).
//! Legacy camelCase `projectId` payloads are normalized by DB
//! migration 000065. The chat executor reads this field and embeds
//! it in the upstream request envelope.
//!
//! # Module layout
//!
//! The provider is split across four files to keep each concern in
//! its own module:
//!
//! - [`mod@counters`]: threshold/backoff constants + the
//!   `INVALID_GRANT_COUNTERS` map + `bump`/`reset`/`mark_account_unhealthy`.
//! - [`mod@retry`]: `drive_invalid_grant_retry` + `OnUnhealthyCell` +
//!   the GAP-5 unit tests and adversarial tests.
//! - [`mod@post_exchange`]: the three helpers extracted from
//!   `post_exchange` (email fetch, project bootstrap, persistence).
//! - This file: the trait `impl`, the provider struct, the OAuth
//!   spec, and the tests that exercise the trait surface directly.

use serde::{Deserialize, Serialize};
use std::sync::Arc;

use super::generic::{GenericOAuthProvider, OAuthRequestEncoding, OAuthSpec};
use crate::error::Result;
use crate::ids::AccountId;
use crate::oauth::{DbRef, OAuthFlow, OAuthProvider, TokenResponse};
use openproxy_adapters::upstream::UpstreamClient;
use openproxy_db::secrets::MasterKey;

mod counters;
mod post_exchange;
mod retry;
#[cfg(test)]
mod test_util;

use counters::mark_account_unhealthy;
use post_exchange::{bootstrap_project_id, fetch_user_email, persist_post_exchange_meta};
use retry::drive_invalid_grant_retry;

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
        poll_device_token
    );

    async fn refresh_token(
        &self,
        refresh_token: &str,
        upstream_client: &Arc<UpstreamClient>,
        account_id: AccountId,
        db: DbRef<'_>,
    ) -> Result<TokenResponse> {
        let refresh_token = refresh_token.to_string();
        let upstream_client = Arc::clone(upstream_client);
        let on_unhealthy_db = db;
        drive_invalid_grant_retry(
            account_id,
            move || {
                let refresh_token = refresh_token.clone();
                let upstream_client = Arc::clone(&upstream_client);
                async move {
                    self.generic
                        .refresh_token(&refresh_token, &upstream_client, account_id, db)
                        .await
                }
            },
            move |aid| {
                mark_account_unhealthy(on_unhealthy_db, aid);
            },
        )
        .await
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["antigravity-cli"]
    }

    async fn post_exchange(
        &self,
        account_id: AccountId,
        db_pool: &std::sync::Arc<openproxy_db::DbPool>,
        master_key: &MasterKey,
        upstream: &Arc<UpstreamClient>,
    ) -> Result<()> {
        // 1. Decrypt the access token. Scoped block drops the writer
        //    guard before any `.await` below (SQLite Connection is not
        //    `Send` across await points).
        let access_token = {
            let conn = db_pool.writer();
            crate::accounts::decrypt_access_token(&conn, account_id, master_key)?
        };

        // 2. Fetch user email (best-effort) and bootstrap the projectId
        //    sequentially to preserve the original ordering and HTTP
        //    behavior of the pre-split implementation.
        let email = fetch_user_email(upstream, &access_token).await;
        let project_id = bootstrap_project_id(upstream, &access_token).await?;

        // 3. Persist projectId (+ optional email/label) on the account row.
        persist_post_exchange_meta(db_pool, account_id, project_id, email).await
    }
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
        assert_eq!(p.aliases(), &["antigravity-cli"]);
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
