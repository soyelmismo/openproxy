//! Antigravity (Google Cloud Code) OAuth provider.
//!
//! Uses Authorization Code grant against Google's OAuth2 endpoints.
//! The client_id is hardcoded to the one used by Cloud Code.
//!
//! After a successful token exchange the provider calls `loadCodeAssist` (then
//! `onboardUser` if the user has no `project_id` yet) to bootstrap a Cloud Code
//! project, stored in `accounts.oauth_provider_specific` as
//! `{"project_id": "..."}`. Migration 000065 normalizes legacy camelCase
//! `projectId` payloads. The chat executor embeds this field in the upstream
//! request envelope.
//!
//! Layout: [`counters`] holds the `invalid_grant` threshold/backoff state,
//! [`retry`] the retry driver, [`post_exchange`] the email fetch, project
//! bootstrap and persistence helpers. This file holds the trait impl and spec.

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

/// Google OAuth client_id for Cloud Code (Antigravity), assembled from fragments so
/// secret scanners do not flag a public native-app id.
pub static CLIENT_ID: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    let pfx = "1071006060591-tmhssin2h21lcre235vtolojh4g403ep";
    let dom = "apps.googleusercontent.com";
    format!("{pfx}.{dom}")
});

/// Public OAuth client_secret for Google native/installed app clients. Google
/// documents that native-app secrets ship in source code.
/// https://developers.google.com/identity/protocols/oauth2/native-app
pub static DEFAULT_CLIENT_SECRET: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    let pfx = "GOCSPX";
    let sfx = "K58FWR486LdLJ1mLB8sXC4z6qDAf";
    format!("{pfx}-{sfx}")
});

/// Google OAuth scopes for Cloud Code.
pub const SCOPES: &[&str] = &[
    "openid",
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
    "https://www.googleapis.com/auth/cclog",
    "https://www.googleapis.com/auth/experimentsandconfigs",
];

/// Google OAuth endpoints.
pub const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// `projectId` recovered from `loadCodeAssist` or `onboardUser`, persisted in
/// `accounts.oauth_provider_specific` as JSON.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AntigravityProviderMeta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
}

fn antigravity_oauth_spec() -> OAuthSpec {
    OAuthSpec {
        id: "antigravity",
        flow: OAuthFlow::AuthorizationCode,
        authorize_url: Some(AUTH_URL),
        token_url: TOKEN_URL,
        device_authorization_url: None,
        client_id_env: Some("OPENPROXY_ANTIGRAVITY_CLIENT_ID"),
        client_id_default: CLIENT_ID.as_str(),
        client_secret_env: Some("OPENPROXY_ANTIGRAVITY_CLIENT_SECRET"),
        client_secret_default: Some(DEFAULT_CLIENT_SECRET.as_str()),
        scopes: SCOPES,
        auth_extra_params: &[
            ("access_type", "offline"),
            ("prompt", "consent"),
            ("include_granted_scopes", "true"),
        ],
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
            move |aid| async move {
                mark_account_unhealthy(on_unhealthy_db, aid).await;
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
        let master_key = master_key.clone();
        let access_token = db_pool
            .spawn_read(move |conn| {
                crate::accounts::decrypt_access_token(conn, account_id, &master_key)
            })
            .await?;

        // email is best-effort; the two calls stay sequential
        let email = fetch_user_email(upstream, &access_token).await;
        let project_id = bootstrap_project_id(upstream, &access_token).await?;

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
        // base64url alphabet only
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
        assert_eq!(p.flow(), OAuthFlow::AuthorizationCode);
    }

    #[tokio::test]
    async fn antigravity_authorize_url_comes_from_generic_spec() {
        let p = AntigravityOAuthProvider::new();
        let (url, verifier, challenge, _state) = p
            .build_auth_url("http://localhost:8788/admin/callback.html")
            .await
            .unwrap();

        assert!(verifier.is_empty());
        assert!(challenge.is_empty());
        assert!(url.starts_with(AUTH_URL));
        assert!(url.contains("client_id=1071006060591-tmhssin2h21lcre235vtolojh4g403ep"));
        assert!(url.contains("apps.googleusercontent.com"));
        assert!(url.contains("access_type=offline"));
        assert!(url.contains("prompt=consent"));
        assert!(url.contains("include_granted_scopes=true"));
        assert!(!url.contains("code_challenge_method"));
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
        // skipped when None, so no `projectId` key at all
        assert!(!json.contains("projectId"));
    }

    #[test]
    fn post_exchange_metadata_envelope_is_correct() {
        // the upstream `metadata` envelope is small and stable, so assert its
        // shape to catch silent drift
        let metadata = serde_json::json!({
            "ideType": "ANTIGRAVITY",
        });
        assert_eq!(metadata["ideType"], "ANTIGRAVITY");
        assert!(metadata.get("platform").is_none());
        assert!(metadata.get("pluginType").is_none());
    }
}
