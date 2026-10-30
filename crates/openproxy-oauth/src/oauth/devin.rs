//! Devin OAuth provider: Cognition's PKCE CLI login.
//!
//! Authorization runs against `app.devin.ai/auth/cli/continue` — the endpoint
//! is Devin's own CLI handshake, so there is no `client_id`, `response_type`
//! or `scope` on the wire. The returned code exchanges at
//! `api.devin.ai/auth/cli/token` for a `devin-session-token$…` session token.
//!
//! Session tokens are permanent: upstream's `RefreshLead` returns nil and
//! there is no refresh grant, so no `refresh_token` is stored (the scheduler
//! skips accounts without one) and `refresh_token` only explains that
//! re-authentication is the renewal path. `post_exchange` calls
//! `GetUserStatus` to fill profile fields and seed the initial quota
//! snapshot — the same RPC the quota refresh path uses later.

use serde::Deserialize;
use std::sync::Arc;

use crate::error::{CoreError, Result};
use crate::ids::AccountId;
use crate::oauth::{DbRef, DeviceAuthorizationResponse, OAuthFlow, OAuthProvider, TokenResponse};
use openproxy_adapters::upstream::{UpstreamClient, UpstreamRequest};
use openproxy_db::secrets::MasterKey;

/// User-facing OAuth app (authorization endpoint).
pub const DEVIN_APP_BASE_URL: &str = "https://app.devin.ai";
/// API backend (token exchange).
pub const DEVIN_API_BASE_URL: &str = "https://api.devin.ai";
/// Session tokens carry this mandatory prefix on the wire.
pub const DEVIN_TOKEN_PREFIX: &str = "devin-session-token$";

#[derive(Clone)]
pub struct DevinOAuthProvider {
    app: super::OAuthEndpointResolver,
    api: super::OAuthEndpointResolver,
}

impl DevinOAuthProvider {
    pub fn new() -> Self {
        Self {
            app: super::OAuthEndpointResolver::new(
                "OPENPROXY_DEVIN_APP_BASE_URL",
                DEVIN_APP_BASE_URL,
            ),
            api: super::OAuthEndpointResolver::new(
                "OPENPROXY_DEVIN_API_BASE_URL",
                DEVIN_API_BASE_URL,
            ),
        }
    }

    #[cfg(test)]
    fn with_base_urls(app: &str, api: &str) -> Self {
        Self {
            app: super::OAuthEndpointResolver::new(
                "OPENPROXY_DEVIN_APP_BASE_URL",
                DEVIN_APP_BASE_URL,
            )
            .with_custom_base(app),
            api: super::OAuthEndpointResolver::new(
                "OPENPROXY_DEVIN_API_BASE_URL",
                DEVIN_API_BASE_URL,
            )
            .with_custom_base(api),
        }
    }
}

impl Default for DevinOAuthProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// Upstream `FormatSessionToken`: keep an already-prefixed token, add the
/// prefix to a bare JWT (`eyJ…`), pass anything else through untouched.
fn format_session_token(raw: &str) -> String {
    let t = raw.trim();
    if t.starts_with(DEVIN_TOKEN_PREFIX) {
        t.to_string()
    } else if t.starts_with("eyJ") {
        format!("{DEVIN_TOKEN_PREFIX}{t}")
    } else {
        t.to_string()
    }
}

/// Devin's authorization page strictly allows `http://127.0.0.1:<port>/callback`
/// redirects — http scheme, literal `127.0.0.1` host and `/callback` path.
/// Keep the dashboard listener's port (the only part of the server-supplied
/// redirect URI that is valid on this allowlist) so the browser lands on the
/// unauthenticated `GET /callback` bridge served by the admin router. Remote
/// browsers cannot reach `127.0.0.1` on their own machine and complete the
/// login through the manual-paste field instead, matching upstream.
fn devin_loopback_redirect(redirect_uri: &str) -> String {
    let authority = redirect_uri
        .split_once("://")
        .map(|(_, rest)| rest.split('/').next().unwrap_or_default())
        .unwrap_or_default();
    // `rsplit_once` keeps IPv6 authorities like `[::1]:8787` intact — the
    // host is irrelevant here, only the port is reused.
    let port = authority
        .rsplit_once(':')
        .map(|(_, p)| p)
        .filter(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        .map_or_else(
            || {
                if redirect_uri.starts_with("https://") {
                    "443".into()
                } else {
                    "80".into()
                }
            },
            str::to_string,
        );
    format!("http://127.0.0.1:{port}/callback")
}

impl OAuthProvider for DevinOAuthProvider {
    fn name(&self) -> &'static str {
        "devin"
    }

    fn flow(&self) -> OAuthFlow {
        OAuthFlow::AuthorizationCodePkce
    }

    fn build_auth_url(
        &self,
        redirect_uri: &str,
    ) -> impl std::future::Future<Output = Result<(String, String, String, String)>> + Send {
        // Query parameter order mirrors the official Devin CLI binary.
        let code_verifier = super::generic::generate_code_verifier();
        let code_challenge = super::generic::code_challenge_s256(&code_verifier);
        let state_buf: [u8; 16] = rand::random();
        let state = openproxy_types::to_hex_lower(&state_buf);
        let loopback_redirect = devin_loopback_redirect(redirect_uri);
        let url = format!(
            "{}/auth/cli/continue?redirect_uri={}&state={}&prompt=select_account\
             &code_challenge={}&code_challenge_method=S256",
            self.app.base_url().trim_end_matches('/'),
            urlencoding::encode(&loopback_redirect),
            state,
            code_challenge,
        );
        std::future::ready(Ok((url, code_verifier, loopback_redirect, state)))
    }

    async fn exchange_code(
        &self,
        code: &str,
        code_verifier: &str,
        upstream_client: &Arc<UpstreamClient>,
        _redirect_uri: &str,
    ) -> Result<TokenResponse> {
        let (actual_code, _) = crate::oauth::util::parse_oauth_callback_input(code);
        let body = serde_json::json!({
            "code": actual_code,
            "code_verifier": code_verifier,
        });
        let body_bytes =
            serde_json::to_vec(&body).map_err(|e| CoreError::Validation(e.to_string()))?;
        let token_url = format!(
            "{}/auth/cli/token",
            self.api.base_url().trim_end_matches('/')
        );
        let mut req = UpstreamRequest::post_json(token_url, bytes::Bytes::from(body_bytes));
        req.headers.insert(
            http::header::ACCEPT,
            http::HeaderValue::from_static("application/json"),
        );

        let resp: DevinTokenExchange =
            super::call_oauth_json(upstream_client, req, "devin", "devin exchange").await?;
        let token = format_session_token(&resp.token);
        if token.is_empty() {
            return Err(CoreError::Parse(
                "devin token exchange: empty token in response".into(),
            ));
        }

        Ok(TokenResponse {
            access_token: token,
            // Permanent session token: no refresh grant exists. Storing none
            // keeps the refresh scheduler off this account entirely.
            refresh_token: None,
            token_type: "Bearer".into(),
            expires_in: None,
            scope: None,
            id_token: None,
        })
    }

    fn request_device_code(
        &self,
        _upstream_client: &Arc<UpstreamClient>,
    ) -> impl std::future::Future<Output = Result<DeviceAuthorizationResponse>> + Send {
        std::future::ready(Err(CoreError::Validation(
            "devin uses the PKCE CLI flow, not device code".into(),
        )))
    }

    fn poll_device_token(
        &self,
        _device_code: &str,
        _upstream_client: &Arc<UpstreamClient>,
    ) -> impl std::future::Future<Output = Result<Option<TokenResponse>>> + Send {
        std::future::ready(Err(CoreError::Validation(
            "devin uses the PKCE CLI flow, not device code".into(),
        )))
    }

    fn refresh_token(
        &self,
        _refresh_token: &str,
        _upstream_client: &Arc<UpstreamClient>,
        _account_id: AccountId,
        _db: DbRef<'_>,
    ) -> impl std::future::Future<Output = Result<TokenResponse>> + Send {
        std::future::ready(Err(CoreError::Validation(
            "devin session tokens are permanent and cannot be refreshed; \
             re-authenticate via OAuth"
                .into(),
        )))
    }

    /// Session tokens are permanent credentials: the JWT carries an `exp`
    /// claim but the reasoning backend does not enforce it, and no refresh
    /// grant exists to renew one. Recording it would flag the account as
    /// eternally expiring.
    fn access_tokens_expire(&self) -> bool {
        false
    }

    fn provider_specific_from_token(&self, _token: &TokenResponse) -> Option<String> {
        // A fresh random seed per login anchors the wire device fingerprint,
        // matching upstream which stores one seed per auth record.
        let buf: [u8; 32] = rand::random();
        Some(serde_json::json!({ "device_seed": openproxy_types::to_hex_lower(&buf) }).to_string())
    }

    fn email_from_token(&self, token: &TokenResponse) -> Option<String> {
        // The `devin-session-token$` prefix sits on the first JWT segment and
        // is discarded by the payload decoder, so the shared claim extractor
        // works unmodified.
        super::extract_email_from_token(token)
    }

    async fn post_exchange(
        &self,
        account_id: AccountId,
        db_pool: &std::sync::Arc<openproxy_db::DbPool>,
        master_key: &MasterKey,
        upstream: &Arc<UpstreamClient>,
    ) -> Result<()> {
        let mk = master_key.clone();
        let access_token = db_pool
            .spawn_read(move |conn| crate::accounts::decrypt_access_token(conn, account_id, &mk))
            .await?;
        let mk = master_key.clone();
        let device_seed = db_pool
            .spawn_read(move |conn| {
                let meta: Option<serde_json::Value> =
                    crate::accounts::read_provider_meta(conn, Some(&mk), account_id.0)?;
                Ok(meta
                    .and_then(|m| {
                        m.get("device_seed")
                            .and_then(|v| v.as_str())
                            .map(ToString::to_string)
                    })
                    .unwrap_or_default())
            })
            .await?;

        // Best-effort profile + initial quota snapshot: a fetch failure must
        // not fail the login, so errors degrade to a skipped update.
        let Ok(status) = openproxy_adapters::adapters::devin::fetch_devin_user_status(
            upstream,
            openproxy_adapters::adapters::devin::DEVIN_DEFAULT_BASE_URL,
            &access_token,
            &device_seed,
            None,
        )
        .await
        else {
            return Ok(());
        };

        let quota = openproxy_adapters::adapters::devin::to_account_quota(&status);
        let mk = master_key.clone();
        db_pool
            .spawn_write(move |conn| {
                let mut meta: serde_json::Value =
                    crate::accounts::read_provider_meta(conn, Some(&mk), account_id.0)?
                        .unwrap_or_else(|| serde_json::json!({}));
                let obj = meta.as_object_mut().ok_or_else(|| {
                    CoreError::Internal("devin: provider_specific not a JSON object".into())
                })?;
                for (key, value) in [
                    ("user_name", &status.user_name),
                    ("user_id", &status.user_id),
                    ("org_id", &status.org_id),
                    ("org_name", &status.org_name),
                    ("plan", &status.plan_name),
                ] {
                    if !value.is_empty() {
                        obj.insert(key.into(), serde_json::Value::String(value.clone()));
                    }
                }
                let meta_encrypted =
                    crate::accounts::encrypt_oauth_provider_specific(&meta.to_string(), &mk)?;
                let email = (!status.email.is_empty()).then_some(status.email.clone());
                conn.execute(
                    "UPDATE accounts SET oauth_provider_specific = ?1, \
                     email = COALESCE(?2, email), label = COALESCE(NULLIF(label, ''), ?3) \
                     WHERE id = ?4",
                    rusqlite::params![meta_encrypted, email, status.user_name, account_id.0],
                )
                .map_err(openproxy_db::error::map_db_error_ctx(
                    "devin post_exchange update meta for account",
                ))?;
                openproxy_db::accounts::set_quota(conn, account_id, &quota)
            })
            .await
    }
}

#[derive(Deserialize)]
struct DevinTokenExchange {
    #[serde(default)]
    token: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use futures_util::FutureExt;

    #[test]
    fn build_auth_url_matches_cli_query_shape() {
        let provider = DevinOAuthProvider::with_base_urls(
            "https://app.example.test",
            "https://api.example.test",
        );
        // The dashboard callback URI is rewritten to the only shape Devin's
        // allowlist accepts: http://127.0.0.1:<port>/callback.
        let (url, verifier, redirect, state) = provider
            .build_auth_url("http://localhost:1455/admin/callback.html")
            .now_or_never()
            .expect("build_auth_url is ready")
            .expect("build_auth_url ok");

        let expected_challenge = super::super::generic::code_challenge_s256(&verifier);
        let want = format!(
            "https://app.example.test/auth/cli/continue?redirect_uri=\
             http%3A%2F%2F127.0.0.1%3A1455%2Fcallback&state={state}\
             &prompt=select_account&code_challenge={expected_challenge}\
             &code_challenge_method=S256"
        );
        assert_eq!(url, want);
        assert_eq!(redirect, "http://127.0.0.1:1455/callback");
        assert!((43..=128).contains(&verifier.len()));
        assert!(
            verifier
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
        assert_eq!(state.len(), 32);
    }

    #[test]
    fn loopback_redirect_keeps_only_the_port() {
        assert_eq!(
            devin_loopback_redirect("http://localhost:8787/admin/callback.html"),
            "http://127.0.0.1:8787/callback"
        );
        assert_eq!(
            devin_loopback_redirect("http://127.0.0.1:9999/callback"),
            "http://127.0.0.1:9999/callback"
        );
        // No explicit port: scheme defaults apply.
        assert_eq!(
            devin_loopback_redirect("http://dashboard.example/admin/callback.html"),
            "http://127.0.0.1:80/callback"
        );
        assert_eq!(
            devin_loopback_redirect("https://dashboard.example/admin/callback.html"),
            "http://127.0.0.1:443/callback"
        );
    }

    #[test]
    fn format_session_token_prefixes_bare_jwt() {
        assert_eq!(
            format_session_token("eyJhbGciOiJ.test.sig"),
            "devin-session-token$eyJhbGciOiJ.test.sig"
        );
        assert_eq!(
            format_session_token("devin-session-token$eyJhbGciOiJ.test.sig"),
            "devin-session-token$eyJhbGciOiJ.test.sig"
        );
        assert_eq!(format_session_token("  plain-token  "), "plain-token");
    }

    #[test]
    fn provider_specific_carries_device_seed() {
        let provider = DevinOAuthProvider::new();
        let token = TokenResponse {
            access_token: "devin-session-token$x".into(),
            token_type: "Bearer".into(),
            expires_in: None,
            refresh_token: None,
            scope: None,
            id_token: None,
        };
        let meta = provider
            .provider_specific_from_token(&token)
            .expect("device_seed meta");
        let parsed: serde_json::Value = serde_json::from_str(&meta).unwrap();
        let seed = parsed["device_seed"].as_str().unwrap();
        assert_eq!(seed.len(), 64);
        assert!(seed.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn email_from_prefixed_session_token() {
        let payload = serde_json::json!({ "email": "dev@example.com" }).to_string();
        let jwt = format!(
            "e30.{}.sig",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload)
        );
        let token = TokenResponse {
            access_token: format!("devin-session-token${jwt}"),
            token_type: "Bearer".into(),
            expires_in: None,
            refresh_token: None,
            scope: None,
            id_token: None,
        };
        let provider = DevinOAuthProvider::new();
        assert_eq!(
            provider.email_from_token(&token).as_deref(),
            Some("dev@example.com")
        );
    }

    #[test]
    fn refresh_and_device_flows_are_rejected() {
        let provider = DevinOAuthProvider::new();
        assert_eq!(provider.flow(), OAuthFlow::AuthorizationCodePkce);
        assert_eq!(provider.name(), "devin");
        // Permanent session tokens: no deadline is recorded for them.
        assert!(!provider.access_tokens_expire());
    }
}
