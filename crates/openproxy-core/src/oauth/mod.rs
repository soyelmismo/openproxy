//! Generic OAuth 2.0 infrastructure for providers.
//!
//! This module provides:
//! - `OAuthFlow` enum distinguishing Device Code vs Authorization Code (PKCE).
//! - `OAuthProvider` trait that each OAuth provider implements.
//! - Encrypted token storage helpers (delegates to `accounts` module).
//! - A background refresh scheduler that proactively refreshes expiring tokens.

use crate::error::{CoreError, Result};
use crate::ids::AccountId;
use openproxy_adapters::upstream::UpstreamClient;
use openproxy_db::secrets::MasterKey;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

// Re-export account-level OAuth helpers for convenience.
pub use crate::accounts::{
    StoreOAuthTokensParams, decrypt_access_token, decrypt_refresh_token,
    list_expiring_oauth_accounts, store_oauth_tokens,
};

pub(crate) fn decode_jwt_payload(jwt: &str) -> Option<serde_json::Value> {
    use base64::Engine;
    let (_, rest) = jwt.split_once('.')?;
    let payload = rest.split_once('.').map_or(rest, |(p, _)| p);
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub(crate) fn map_upstream_err(
    e: openproxy_adapters::upstream::UpstreamError,
    ctx: &str,
) -> CoreError {
    if matches!(e, openproxy_adapters::upstream::UpstreamError::Cancel) {
        CoreError::Cancelled(openproxy_types::CancelReason::ClientDisconnected)
    } else {
        CoreError::UpstreamConnection(format!("{ctx}: {e}"))
    }
}

pub mod antigravity;
pub mod cline;
pub mod codex;
pub mod generic;
pub mod kiro;
pub mod tickets;

/// A reference to either a `DbPool` or a locked/lockable database `Connection`.
#[derive(Clone, Copy)]
pub enum DbRef<'a> {
    Pool(&'a openproxy_db::DbPool),
    Connection(&'a parking_lot::Mutex<rusqlite::Connection>),
}

impl DbRef<'_> {
    pub fn with_conn<R>(
        &self,
        f: impl FnOnce(&rusqlite::Connection) -> crate::error::Result<R>,
    ) -> crate::error::Result<R> {
        match self {
            DbRef::Pool(pool) => f(&pool.writer()),
            DbRef::Connection(mutex) => f(&mutex.lock()),
        }
    }
}

/// The OAuth flow used by a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OAuthFlow {
    /// RFC 8628 Device Authorization Grant (Kiro, etc.).
    DeviceCode,
    /// Authorization Code with PKCE (Antigravity, Google, etc.).
    AuthorizationCodePkce,
    /// Standard Authorization Code (no PKCE). Requires a client_secret
    /// to exchange the code. Used by providers like Gemini CLI which
    /// embed a secret in their binary (acceptable for server-side use).
    AuthorizationCode,
}

impl OAuthFlow {
    pub fn as_str(&self) -> &'static str {
        match self {
            OAuthFlow::DeviceCode => "device_code",
            OAuthFlow::AuthorizationCodePkce => "authorization_code_pkce",
            OAuthFlow::AuthorizationCode => "authorization_code",
        }
    }
}

pub use openproxy_types::oauth::{DeviceAuthorizationResponse, TokenResponse};

/// Metadata about the OAuth provider that the scheduler needs to refresh
/// tokens. Stored in `accounts.oauth_provider_specific` as JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthProviderMeta {
    /// The flow type used by this provider.
    pub flow: OAuthFlow,
    /// Provider-specific metadata (e.g. client_id, device_code).
    #[serde(default)]
    pub extra: serde_json::Value,
}

// =====================================================================
// OAuthProvider trait
// =====================================================================

/// Provider-specific OAuth logic.
///
/// Each concrete OAuth provider (Antigravity, Kiro, etc.) implements this
/// trait. The trait methods are `async` because they make HTTP calls to
/// the OAuth endpoints.
///
pub trait OAuthProvider: Send + Sync {
    /// Human-readable name for logging (e.g. "antigravity").
    fn name(&self) -> &str;

    /// Provider aliases (e.g. `["antigravity-cli"]`).
    fn aliases(&self) -> &'static [&'static str] {
        &[]
    }

    /// The OAuth flow this provider uses.
    fn flow(&self) -> OAuthFlow;

    /// Build the authorization URL.
    ///
    /// `redirect_uri` is the OAuth callback URL (dynamic, based on how
    /// the user accessed the dashboard).
    ///
    /// Returns `(auth_url, code_verifier, code_challenge)` where:
    /// - `auth_url` is the URL to redirect the user to.
    /// - `code_verifier` is the PKCE code verifier (must be stored for
    ///   exchange), or empty string for non-PKCE flows.
    /// - `code_challenge` is the S256 challenge to include in the auth
    ///   URL, or empty string for non-PKCE flows.
    /// - `state` is a random value included in the authorization URL
    ///   to prevent CSRF attacks on the callback.
    ///
    /// Returns `Err` if the provider uses Device Code flow.
    fn build_auth_url(
        &self,
        _redirect_uri: &str,
    ) -> impl std::future::Future<Output = Result<(String, String, String, String)>> + Send {
        std::future::ready(Err(CoreError::Validation(format!(
            "provider '{}' does not support authorization URL",
            self.name()
        ))))
    }

    /// Exchange an authorization code for tokens (PKCE flow).
    ///
    /// `code` is the authorization code from the callback.
    /// `code_verifier` is the PKCE verifier stored during `build_auth_url`.
    /// `redirect_uri` must match the one used in `build_auth_url`.
    fn exchange_code(
        &self,
        code: &str,
        code_verifier: &str,
        upstream_client: &Arc<UpstreamClient>,
        redirect_uri: &str,
    ) -> impl std::future::Future<Output = Result<TokenResponse>> + Send;

    /// Request a device code and user code (Device Code flow).
    fn request_device_code(
        &self,
        upstream_client: &Arc<UpstreamClient>,
    ) -> impl std::future::Future<Output = Result<DeviceAuthorizationResponse>> + Send;

    /// Poll the token endpoint with a device code (Device Code flow).
    ///
    /// Returns `Ok(Some(token))` on success, `Ok(None)` if the authorization
    /// is still pending (caller should retry after `interval` seconds).
    fn poll_device_token(
        &self,
        device_code: &str,
        upstream_client: &Arc<UpstreamClient>,
    ) -> impl std::future::Future<Output = Result<Option<TokenResponse>>> + Send;

    /// Refresh an access token using a refresh token.
    fn refresh_token(
        &self,
        refresh_token: &str,
        upstream_client: &Arc<UpstreamClient>,
        account_id: AccountId,
        db: DbRef<'_>,
    ) -> impl std::future::Future<Output = Result<TokenResponse>> + Send;

    /// Optional metadata extracted directly from a token response.
    ///
    /// Providers that receive useful non-secret claims in `id_token` can
    /// persist them here before `post_exchange` runs. The value is stored as
    /// plaintext JSON in `accounts.oauth_provider_specific`.
    fn provider_specific_from_token(&self, _token: &TokenResponse) -> Option<String> {
        None
    }

    /// Optional email extracted directly from a token response.
    fn email_from_token(&self, _token: &TokenResponse) -> Option<String> {
        None
    }

    /// Post-exchange hook. Called after tokens are stored. Providers can
    /// use this for additional setup (e.g. fetching user info).
    ///
    /// The `db_pool` is an `Arc<DbPool>` (not a `&Connection`) so the
    /// async body can drop the SQLite guard before the first `.await`
    /// — SQLite connections are not `Send`, so holding a
    /// `&Connection` across an await would fail to compile. The
    /// contract is: every SQLite read/write happens synchronously,
    /// the guard is released, and only the HTTP call to the
    /// provider is awaited.
    fn post_exchange(
        &self,
        _account_id: AccountId,
        _db_pool: &std::sync::Arc<openproxy_db::DbPool>,
        _master_key: &MasterKey,
        _upstream: &Arc<UpstreamClient>,
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        async move { Ok(()) }
    }
}

// =====================================================================
// OAuth provider registry — a generic HashMap-based registry that
// makes it easy to add new OAuth providers without modifying match
// statements. Used by the pipeline (for on-demand refresh during chat
// requests), the background scheduler, and the admin handlers.
// =====================================================================

/// A generic, extensible registry of OAuth providers.
///
/// Providers are looked up by their `name()` string. Built-in providers
/// are registered at startup; custom providers can be added at any time
/// via `register()`. Internally stores `Arc` so cloning the registry
/// is cheap and providers don't need to implement `Clone`.
#[macro_export]
macro_rules! define_oauth_provider {
    (
        $(#[$meta:meta])*
        pub enum OAuthProviderEnum {
            builtins {
                $(
                    $(#[$b_varmeta:meta])*
                    $b_variant:ident($b_inner:ty)
                ),+ $(,)?
            }
            custom {
                $(
                    $(#[$c_varmeta:meta])*
                    $c_variant:ident($c_inner:ty)
                ),+ $(,)?
            }
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone)]
        pub enum OAuthProviderEnum {
            $(
                $(#[$b_varmeta])*
                $b_variant($b_inner),
            )+
            $(
                $(#[$c_varmeta])*
                $c_variant($c_inner)
            ),+
        }

        impl OAuthProviderEnum {
            pub fn builtin_providers() -> Vec<OAuthProviderEnum> {
                vec![
                    $( $(#[$b_varmeta])* OAuthProviderEnum::$b_variant(<$b_inner>::new()) ),+
                ]
            }
        }

        impl OAuthProvider for OAuthProviderEnum {
            fn name(&self) -> &str {
                match self {
                    $( Self::$b_variant(inner) => inner.name(), )+
                    $( Self::$c_variant(inner) => inner.name(), )+
                }
            }
            fn aliases(&self) -> &'static [&'static str] {
                match self {
                    $( Self::$b_variant(inner) => inner.aliases(), )+
                    $( Self::$c_variant(inner) => inner.aliases(), )+
                }
            }
            fn flow(&self) -> OAuthFlow {
                match self {
                    $( Self::$b_variant(inner) => inner.flow(), )+
                    $( Self::$c_variant(inner) => inner.flow(), )+
                }
            }
            async fn build_auth_url(&self, redirect_uri: &str) -> Result<(String, String, String, String)> {
                match self {
                    $( Self::$b_variant(inner) => inner.build_auth_url(redirect_uri).await, )+
                    $( Self::$c_variant(inner) => inner.build_auth_url(redirect_uri).await, )+
                }
            }
            async fn exchange_code(
                &self,
                code: &str,
                code_verifier: &str,
                upstream_client: &std::sync::Arc<openproxy_adapters::upstream::UpstreamClient>,
                redirect_uri: &str,
            ) -> Result<TokenResponse> {
                match self {
                    $( Self::$b_variant(inner) => inner.exchange_code(code, code_verifier, upstream_client, redirect_uri).await, )+
                    $( Self::$c_variant(inner) => inner.exchange_code(code, code_verifier, upstream_client, redirect_uri).await, )+
                }
            }
            async fn request_device_code(
                &self,
                upstream_client: &std::sync::Arc<openproxy_adapters::upstream::UpstreamClient>,
            ) -> Result<DeviceAuthorizationResponse> {
                match self {
                    $( Self::$b_variant(inner) => inner.request_device_code(upstream_client).await, )+
                    $( Self::$c_variant(inner) => inner.request_device_code(upstream_client).await, )+
                }
            }
            async fn poll_device_token(
                &self,
                device_code: &str,
                upstream_client: &std::sync::Arc<openproxy_adapters::upstream::UpstreamClient>,
            ) -> Result<Option<TokenResponse>> {
                match self {
                    $( Self::$b_variant(inner) => inner.poll_device_token(device_code, upstream_client).await, )+
                    $( Self::$c_variant(inner) => inner.poll_device_token(device_code, upstream_client).await, )+
                }
            }
            async fn refresh_token(
                &self,
                refresh_token: &str,
                upstream_client: &std::sync::Arc<openproxy_adapters::upstream::UpstreamClient>,
                account_id: AccountId,
                db: DbRef<'_>,
            ) -> Result<TokenResponse> {
                match self {
                    $( Self::$b_variant(inner) => inner.refresh_token(refresh_token, upstream_client, account_id, db).await, )+
                    $( Self::$c_variant(inner) => inner.refresh_token(refresh_token, upstream_client, account_id, db).await, )+
                }
            }
            fn provider_specific_from_token(&self, token: &TokenResponse) -> Option<String> {
                match self {
                    $( Self::$b_variant(inner) => inner.provider_specific_from_token(token), )+
                    $( Self::$c_variant(inner) => inner.provider_specific_from_token(token), )+
                }
            }
            fn email_from_token(&self, token: &TokenResponse) -> Option<String> {
                match self {
                    $( Self::$b_variant(inner) => inner.email_from_token(token), )+
                    $( Self::$c_variant(inner) => inner.email_from_token(token), )+
                }
            }
            async fn post_exchange(
                &self,
                account_id: AccountId,
                db_pool: &std::sync::Arc<openproxy_db::DbPool>,
                master_key: &MasterKey,
                upstream: &std::sync::Arc<openproxy_adapters::upstream::UpstreamClient>,
            ) -> Result<()> {
                match self {
                    $( Self::$b_variant(inner) => inner.post_exchange(account_id, db_pool, master_key, upstream).await, )+
                    $( Self::$c_variant(inner) => inner.post_exchange(account_id, db_pool, master_key, upstream).await, )+
                }
            }
        }
    }
}

define_oauth_provider! {
    pub enum OAuthProviderEnum {
        builtins {
            Antigravity(self::antigravity::AntigravityOAuthProvider),
            Codex(self::codex::CodexOAuthProvider),
            Cline(self::cline::ClineOAuthProvider),
            Kiro(self::kiro::KiroOAuthProvider),
        }
        custom {
            Generic(self::generic::GenericOAuthProvider),
        }
    }
}

pub mod refresh;
pub mod registry;

#[cfg(test)]
mod tests;

pub use refresh::*;
#[cfg(test)]
pub(crate) use refresh::{BASE_BACKOFF_SECS, MAX_BACKOFF_SECS, backoff_seconds};
pub use registry::*;
