//! Generic OAuth 2.0 infrastructure for providers.
//!
//! This module provides:
//! - `OAuthFlow` enum distinguishing Device Code vs Authorization Code (PKCE).
//! - `OAuthProvider` trait that each OAuth provider implements.
//! - Encrypted token storage helpers (delegates to `accounts` module).
//! - A background refresh scheduler that proactively refreshes expiring tokens.

use crate::accounts::HealthStatus;
use crate::error::{CoreError, Result};
use crate::ids::AccountId;
use once_cell::sync::Lazy;
use openproxy_adapters::upstream::UpstreamClient;
use openproxy_db::secrets::MasterKey;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

// Re-export account-level OAuth helpers for convenience.
pub use crate::accounts::{
    decrypt_access_token, decrypt_refresh_token, list_expiring_oauth_accounts, store_oauth_tokens,
};

/// A reference to either a `DbPool` or a locked/lockable database `Connection`.
pub enum DbRef<'a> {
    Pool(&'a openproxy_db::DbPool),
    Connection(&'a parking_lot::Mutex<rusqlite::Connection>),
}

impl<'a> DbRef<'a> {
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

/// Standard OAuth2 token response from the token endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenResponse {
    #[serde(rename = "access_token", alias = "accessToken")]
    pub access_token: String,
    #[serde(default, rename = "token_type", alias = "tokenType")]
    pub token_type: String,
    #[serde(default, rename = "expires_in", alias = "expiresIn")]
    pub expires_in: Option<u64>,
    #[serde(default, rename = "refresh_token", alias = "refreshToken")]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default, rename = "id_token", alias = "idToken")]
    pub id_token: Option<String>,
}

/// Device Authorization Response (RFC 8628 §3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceAuthorizationResponse {
    #[serde(rename = "deviceCode", alias = "device_code")]
    pub device_code: String,
    #[serde(rename = "userCode", alias = "user_code")]
    pub user_code: String,
    #[serde(rename = "verificationUri", alias = "verification_uri")]
    pub verification_uri: String,
    #[serde(
        default,
        rename = "verificationUriComplete",
        alias = "verification_uri_complete"
    )]
    pub verification_uri_complete: Option<String>,
    #[serde(default, rename = "expiresIn", alias = "expires_in")]
    pub expires_in: Option<u64>,
    #[serde(default)]
    pub interval: Option<u64>,
}

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
        redirect_uri: &str,
    ) -> impl std::future::Future<Output = Result<(String, String, String, String)>> + Send {
        let redirect_uri_clone = redirect_uri.to_string();
        async move {
            let _ = redirect_uri_clone;
            Err(CoreError::Validation(format!(
                "provider '{}' does not support authorization URL",
                self.name()
            )))
        }
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
            $(
                $(#[$varmeta:meta])*
                $variant:ident($inner:ty)
            ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone)]
        pub enum OAuthProviderEnum {
            $(
                $(#[$varmeta])*
                $variant($inner)
            ),+
        }

        impl OAuthProvider for OAuthProviderEnum {
            fn name(&self) -> &str {
                match self { $( Self::$variant(inner) => inner.name(), )+ }
            }
            fn flow(&self) -> OAuthFlow {
                match self { $( Self::$variant(inner) => inner.flow(), )+ }
            }
            async fn build_auth_url(&self, redirect_uri: &str) -> Result<(String, String, String, String)> {
                match self { $( Self::$variant(inner) => inner.build_auth_url(redirect_uri).await, )+ }
            }
            async fn exchange_code(
                &self,
                code: &str,
                code_verifier: &str,
                upstream_client: &std::sync::Arc<openproxy_adapters::upstream::UpstreamClient>,
                redirect_uri: &str,
            ) -> Result<TokenResponse> {
                match self { $( Self::$variant(inner) => inner.exchange_code(code, code_verifier, upstream_client, redirect_uri).await, )+ }
            }
            async fn request_device_code(
                &self,
                upstream_client: &std::sync::Arc<openproxy_adapters::upstream::UpstreamClient>,
            ) -> Result<DeviceAuthorizationResponse> {
                match self { $( Self::$variant(inner) => inner.request_device_code(upstream_client).await, )+ }
            }
            async fn poll_device_token(
                &self,
                device_code: &str,
                upstream_client: &std::sync::Arc<openproxy_adapters::upstream::UpstreamClient>,
            ) -> Result<Option<TokenResponse>> {
                match self { $( Self::$variant(inner) => inner.poll_device_token(device_code, upstream_client).await, )+ }
            }
            async fn refresh_token(
                &self,
                refresh_token: &str,
                upstream_client: &std::sync::Arc<openproxy_adapters::upstream::UpstreamClient>,
                account_id: AccountId,
                db: DbRef<'_>,
            ) -> Result<TokenResponse> {
                match self { $( Self::$variant(inner) => inner.refresh_token(refresh_token, upstream_client, account_id, db).await, )+ }
            }
            fn provider_specific_from_token(&self, token: &TokenResponse) -> Option<String> {
                match self { $( Self::$variant(inner) => inner.provider_specific_from_token(token), )+ }
            }
            fn email_from_token(&self, token: &TokenResponse) -> Option<String> {
                match self { $( Self::$variant(inner) => inner.email_from_token(token), )+ }
            }
            async fn post_exchange(
                &self,
                account_id: AccountId,
                db_pool: &std::sync::Arc<openproxy_db::DbPool>,
                master_key: &MasterKey,
                upstream: &std::sync::Arc<openproxy_adapters::upstream::UpstreamClient>,
            ) -> Result<()> {
                match self { $( Self::$variant(inner) => inner.post_exchange(account_id, db_pool, master_key, upstream).await, )+ }
            }
        }
    }
}

define_oauth_provider! {
    pub enum OAuthProviderEnum {
        Antigravity(crate::oauth_antigravity::AntigravityOAuthProvider),
        Codex(crate::oauth_codex::CodexOAuthProvider),
        Generic(crate::oauth_generic::GenericOAuthProvider),
        Kiro(crate::oauth_kiro::KiroOAuthProvider),
    }
}

pub struct OAuthProviderRegistry {
    inner: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, OAuthProviderEnum>>>,
}

impl Default for OAuthProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl OAuthProviderRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            inner: std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Create a registry pre-populated with the built-in OAuth providers.
    pub fn builtin() -> Self {
        let reg = Self::new();
        // Antigravity (Cloud Code) — registered under both `antigravity`
        // and `antigravity-cli` since they share the same OAuth flow.
        let antigravity = crate::oauth_antigravity::AntigravityOAuthProvider::new();
        reg.register_arc_with_name("antigravity", OAuthProviderEnum::Antigravity(antigravity));
        reg.register_arc(OAuthProviderEnum::Codex(
            crate::oauth_codex::CodexOAuthProvider::new(),
        ));
        reg.register_arc(OAuthProviderEnum::Kiro(
            crate::oauth_kiro::KiroOAuthProvider::new(),
        ));
        reg
    }

    /// Register a new OAuth provider by `Arc`, keyed on the
    /// provider's own `name()`. If a provider with the same name
    /// already exists, it is replaced. This allows custom providers
    /// to override built-in ones at runtime.
    pub fn register_arc(&self, provider: OAuthProviderEnum) {
        let name = provider.name().to_string();
        let mut guard = self.inner.lock().unwrap();
        guard.insert(name, provider);
    }

    /// Register an OAuth provider `Arc` under an explicit key
    /// (useful for aliases like `antigravity-cli` → same impl as
    /// `antigravity`). If a provider with the same key already
    /// exists, it is replaced.
    pub fn register_arc_with_name(&self, name: &str, provider: OAuthProviderEnum) {
        let mut guard = self.inner.lock().unwrap();
        guard.insert(name.to_string(), provider);
    }

    /// Register a new OAuth provider by `Box`. Convenience wrapper
    /// around `register_arc`.
    pub fn register(&self, provider: OAuthProviderEnum) {
        self.register_arc(provider);
    }

    /// Look up an OAuth provider by name. Returns `None` if no provider
    /// is registered with that name.
    pub fn get(&self, name: &str) -> Option<OAuthProviderEnum> {
        let guard = self.inner.lock().unwrap();
        guard.get(name).cloned()
    }
}

impl openproxy_pipeline::oauth::PipelineOAuthRegistry for OAuthProviderRegistry {
    fn refresh_and_store<'a>(
        &'a self,
        provider_id: &'a str,
        refresh_token: &'a str,
        upstream_client: &'a Arc<openproxy_adapters::upstream::UpstreamClient>,
        account_id: AccountId,
        conn: &'a parking_lot::Mutex<rusqlite::Connection>,
        master_key: &'a MasterKey,
    ) -> futures_util::future::BoxFuture<
        'a,
        std::result::Result<openproxy_pipeline::oauth::TokenResponse, CoreError>,
    > {
        use futures_util::FutureExt;
        async move {
            let provider = self
                .get(provider_id)
                .ok_or_else(|| CoreError::ProviderNotFound(provider_id.to_string()))?;
            let token = TokenRefreshCoordinator::global()
                .refresh_and_store(
                    provider_id,
                    provider,
                    refresh_token,
                    upstream_client,
                    account_id,
                    DbRef::Connection(conn),
                    master_key,
                )
                .await?;
            Ok(openproxy_pipeline::oauth::TokenResponse {
                access_token: token.access_token,
                token_type: token.token_type,
                expires_in: token.expires_in,
                refresh_token: token.refresh_token,
                scope: token.scope,
                id_token: token.id_token,
            })
        }
        .boxed()
    }
}

/// Coordinates OAuth refresh calls so every runtime path uses the same
/// serialization and persistence behavior.
///
/// The lock is provider-scoped. That is intentionally conservative: several
/// OAuth backends rotate refresh tokens and can react badly to bursty parallel
/// refreshes for sibling accounts under the same public client.
#[derive(Default)]
pub struct TokenRefreshCoordinator {
    provider_mutexes: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl TokenRefreshCoordinator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn global() -> &'static Self {
        static COORDINATOR: Lazy<TokenRefreshCoordinator> = Lazy::new(TokenRefreshCoordinator::new);
        &COORDINATOR
    }

    async fn mutex_for_provider(&self, provider_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self.provider_mutexes.lock().await;
        map.entry(provider_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    pub async fn refresh_and_store(
        &self,
        provider_id: &str,
        provider: OAuthProviderEnum,
        refresh_token: &str,
        upstream_client: &Arc<UpstreamClient>,
        account_id: AccountId,
        db: DbRef<'_>,
        master_key: &MasterKey,
    ) -> Result<TokenResponse> {
        let mutex = self.mutex_for_provider(provider_id).await;
        let _guard = mutex.lock().await;

        match db {
            DbRef::Pool(pool) => {
                let token = provider
                    .refresh_token(
                        refresh_token,
                        upstream_client,
                        account_id,
                        DbRef::Pool(pool),
                    )
                    .await?;
                let expires_at = token_expires_at(token.expires_in);
                let pool = pool.clone();
                let master_key = master_key.clone();
                let access_token = token.access_token.clone();
                let refresh_token = token.refresh_token.clone();
                let token_type = token.token_type.clone();
                let scope = token.scope.clone();
                let provider_specific = provider.provider_specific_from_token(&token);
                let email = provider.email_from_token(&token);
                tokio::task::spawn_blocking(move || {
                    let conn = pool.writer();
                    store_oauth_tokens(
                        &conn,
                        account_id,
                        &access_token,
                        refresh_token.as_deref(),
                        &master_key,
                        &token_type,
                        expires_at.as_deref(),
                        scope.as_deref(),
                        provider_specific.as_deref(),
                        email.as_deref(),
                    )
                })
                .await
                .map_err(|e| CoreError::Internal(e.to_string()))??;
                Ok(token)
            }
            DbRef::Connection(conn_mutex) => {
                let token = provider
                    .refresh_token(
                        refresh_token,
                        upstream_client,
                        account_id,
                        DbRef::Connection(conn_mutex),
                    )
                    .await?;
                let expires_at = token_expires_at(token.expires_in);
                tokio::task::block_in_place(|| {
                    let conn = conn_mutex.lock();
                    store_oauth_tokens(
                        &conn,
                        account_id,
                        &token.access_token,
                        token.refresh_token.as_deref(),
                        master_key,
                        &token.token_type,
                        expires_at.as_deref(),
                        token.scope.as_deref(),
                        provider.provider_specific_from_token(&token).as_deref(),
                        provider.email_from_token(&token).as_deref(),
                    )
                })?;
                Ok(token)
            }
        }
    }
}

pub fn token_expires_at(expires_in: Option<u64>) -> Option<String> {
    expires_in.map(|secs| {
        (chrono::Utc::now() + chrono::Duration::seconds(secs as i64))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string()
    })
}

/// Resolve an OAuth access token for an account, refreshing it if
/// it is expiring soon.
///
/// Steps:
/// 1. Decrypt the current access token from the DB.
/// 2. Check `oauth_expires_soon()` — if the token is still fresh,
///    return it immediately.
/// 3. If expiring: decrypt the refresh token, find the provider in
///    the registry, call `refresh_token()` (async), store the new
///    tokens, return the new access token.
///
/// The function manages its own database connections from `db_pool`
/// to avoid holding a SQLite connection across `.await`.
pub async fn resolve_oauth_token(
    db_pool: &openproxy_db::DbPool,
    account: &crate::accounts::Account,
    provider_id: &str,
    registry: &OAuthProviderRegistry,
    upstream_client: &std::sync::Arc<openproxy_adapters::upstream::UpstreamClient>,
    master_key: &MasterKey,
) -> Result<String> {
    use crate::accounts::{decrypt_access_token, decrypt_refresh_token};

    // 1. Decrypt current access token.
    let access_token = {
        let db_pool = db_pool.clone();
        let master_key = master_key.clone();
        let acc_id = account.id;
        tokio::task::spawn_blocking(move || {
            let conn = db_pool.writer();
            decrypt_access_token(&conn, acc_id, &master_key)
        })
        .await
        .map_err(|e| CoreError::Internal(e.to_string()))??
    };

    // 2. Check expiry — if still fresh, return as-is.
    if !oauth_expires_soon(account, provider_id) {
        return Ok(access_token);
    }

    // 3. Decrypt refresh token under a fresh connection.
    let refresh_token = {
        let db_pool = db_pool.clone();
        let master_key = master_key.clone();
        let acc_id = account.id;
        tokio::task::spawn_blocking(move || {
            let conn = db_pool.writer();
            decrypt_refresh_token(&conn, acc_id, &master_key)
        })
        .await
        .map_err(|e| CoreError::Internal(e.to_string()))??
        .ok_or_else(|| {
            CoreError::Auth(format!(
                "account {} has no refresh token, cannot refresh",
                account.id.0
            ))
        })?
    };

    // 4. Find the provider implementation.
    let provider = registry.get(provider_id).ok_or_else(|| {
        CoreError::Auth(format!("no OAuth provider registered for '{provider_id}'"))
    })?;

    tracing::info!(
        account = account.id.0,
        provider = provider_id,
        "oauth on-demand refresh: refreshing expiring token"
    );

    // 5. Refresh and store through the shared coordinator. It serializes
    // provider refreshes and applies the same persistence behavior used by
    // the scheduler and pipeline.
    let token = TokenRefreshCoordinator::global()
        .refresh_and_store(
            provider_id,
            provider,
            &refresh_token,
            upstream_client,
            account.id,
            DbRef::Pool(db_pool),
            master_key,
        )
        .await?;

    tracing::info!(
        account = account.id.0,
        provider = provider_id,
        "oauth on-demand refresh: token refreshed successfully"
    );

    Ok(token.access_token)
}

/// Check whether we need to call `resolve_oauth_token` in the
/// pipeline's custom-provider path. This is a lighter-weight check
/// that avoids the full refresh flow when the token is still fresh.
pub fn pipeline_token_needs_refresh(db_expires_at: Option<&str>, provider_id: &str) -> bool {
    let Some(ts) = db_expires_at else {
        return false; // no expiry set → don't know when it expires → assume fresh
    };
    let Ok(expires_at) = chrono::DateTime::parse_from_rfc3339(ts) else {
        return false;
    };
    let expires_at = expires_at.with_timezone(&chrono::Utc);
    let lead = refresh_lead_seconds(provider_id);
    let threshold = chrono::Utc::now() + chrono::Duration::seconds(lead as i64);
    expires_at <= threshold
}

// =====================================================================
// Per-provider refresh lead times
// =====================================================================

/// Returns the refresh lead time in seconds for a given provider.
///
/// Different providers need different refresh windows:
/// - **Rotating tokens** (Auth0-backed): 5 minutes before expiry to
///   avoid cascade revocation (Auth0 invalidates the old refresh token
///   when a new one is issued, so we must refresh early enough that
///   the new token is in place before the old one is needed).
/// - **Non-rotating tokens**: 15 minutes before expiry (standard
///   conservative window).
/// - **Special cases** (e.g. iflow): 24 hours before expiry.
pub(crate) fn refresh_lead_seconds(provider_id: &str) -> u64 {
    let adapters = openproxy_adapters::adapters::builtin_adapters();
    if let Some(adapter) = adapters.iter().find(|a| a.id().as_str() == provider_id)
        && let Some(lead) = adapter.metadata().oauth_refresh_lead_seconds
    {
        return lead;
    }
    900 // 15 minutes default
}

/// Returns the refresh lead time in seconds for a given provider.
pub fn oauth_expires_soon(account: &crate::accounts::Account, provider_id: &str) -> bool {
    let expires_at = match &account.expires_at {
        Some(ts) => ts,
        None => return false,
    };

    let Ok(expires_at) = chrono::DateTime::parse_from_rfc3339(expires_at) else {
        return false;
    };
    let expires_at = expires_at.with_timezone(&chrono::Utc);
    let lead = refresh_lead_seconds(provider_id);
    let threshold = chrono::Utc::now() + chrono::Duration::seconds(lead as i64);

    expires_at <= threshold
}

/// Maximum refresh lead time across all providers (900s = 15 min).
/// Used as the SQL query window; per-provider filtering happens in Rust.
const MAX_REFRESH_LEAD_SECS: i64 = 900;

/// Anti-burst stagger delay between consecutive account refreshes.
const STAGGER_DELAY_SECS: u64 = 3;

/// Settle gap after each refresh to protect Auth0 from rapid-fire calls.
const SETTLE_GAP_SECS: u64 = 2;

// =====================================================================
// Refresh scheduler
// =====================================================================

/// Background task that periodically checks for expiring OAuth tokens
/// and refreshes them. Runs as a tokio task.
///
/// `check_interval_secs` controls how often the scheduler polls (default 60).
/// `refresh_before_secs` is deprecated — per-provider lead times are now
/// used instead. Kept in the signature for backward compatibility.
/// Consecutive failures before marking an account `unhealthy`.
const UNHEALTHY_THRESHOLD: u32 = 3;

/// Maximum backoff delay for retrying failed refreshes (1 hour).
const MAX_BACKOFF_SECS: u64 = 3600;

/// Base backoff interval in seconds (doubles each failure).
const BASE_BACKOFF_SECS: u64 = 60;

pub async fn start_refresh_scheduler(
    db_pool: std::sync::Arc<openproxy_db::DbPool>,
    master_key: std::sync::Arc<MasterKey>,
    upstream_client: Arc<UpstreamClient>,
    registry: Arc<OAuthProviderRegistry>,
    check_interval_secs: u64,
) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(check_interval_secs));
    // Skip the first immediate tick.
    tick.tick().await;

    // In-memory tracking: consecutive failure counts and last attempt timestamps.
    let mut failure_counts: HashMap<i64, u32> = HashMap::new();
    let mut last_refresh_attempts: HashMap<i64, chrono::DateTime<chrono::Utc>> = HashMap::new();

    loop {
        tick.tick().await;

        // Query with the maximum lead time (15 min) so we don't miss
        // any accounts; per-provider filtering happens in Rust below.
        let accounts = {
            let db_pool = db_pool.clone();
            let master_key = master_key.clone();
            tokio::task::spawn_blocking(move || {
                let conn = db_pool.writer();
                crate::accounts::list_expiring_oauth_accounts(
                    &conn,
                    MAX_REFRESH_LEAD_SECS,
                    master_key.as_ref(),
                )
            })
            .await
            .unwrap_or_else(|_| Ok(Vec::new()))
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "oauth refresh scheduler: failed to list expiring accounts");
                Vec::new()
            })
        };

        // Filter by per-provider lead time.
        let now = chrono::Utc::now();
        let accounts: Vec<_> = accounts
            .into_iter()
            .filter(|a| {
                if let Some(ref expires_at) = a.expires_at {
                    let expires_at = match chrono::DateTime::parse_from_rfc3339(expires_at) {
                        Ok(dt) => dt.with_timezone(&chrono::Utc),
                        Err(_) => return false,
                    };
                    let lead = refresh_lead_seconds(&a.provider_id.0);
                    let threshold = now + chrono::Duration::seconds(lead as i64);
                    expires_at <= threshold
                } else {
                    false
                }
            })
            .collect();

        if accounts.is_empty() {
            continue;
        }

        tracing::debug!(
            count = accounts.len(),
            "oauth refresh: accounts due for refresh"
        );

        for (i, account) in accounts.iter().enumerate() {
            // Anti-burst staggering: 3s delay between consecutive accounts.
            if i > 0 {
                tokio::time::sleep(std::time::Duration::from_secs(STAGGER_DELAY_SECS)).await;
            }

            let provider = match registry.get(account.provider_id.as_str()) {
                Some(p) => p,
                None => {
                    tracing::debug!(
                        provider = %account.provider_id,
                        "oauth refresh: no provider impl found, skipping"
                    );
                    continue;
                }
            };

            // Backoff gate: skip accounts that failed recently.
            let account_id = account.id.0;
            if let Some(last_attempt) = last_refresh_attempts.get(&account_id) {
                let failure_count = failure_counts.get(&account_id).copied().unwrap_or(0);
                let backoff = backoff_seconds(failure_count);
                let elapsed = chrono::Utc::now().signed_duration_since(*last_attempt);
                if elapsed.num_seconds() < backoff as i64 {
                    continue;
                }
            }

            let refresh_token = {
                let db_pool = db_pool.clone();
                let master_key = master_key.clone();
                let acc_id = account.id;
                match tokio::task::spawn_blocking(move || {
                    let conn = db_pool.writer();
                    crate::accounts::decrypt_refresh_token(&conn, acc_id, &master_key)
                })
                .await
                {
                    Ok(res) => res,
                    Err(e) => Err(CoreError::Internal(e.to_string())),
                }
            };
            let refresh_token = match refresh_token {
                Ok(Some(rt)) => rt,
                Ok(None) => {
                    tracing::debug!(
                        account = account_id,
                        "oauth refresh: no refresh token stored, skipping"
                    );
                    continue;
                }
                Err(e) => {
                    tracing::warn!(
                        account = account_id,
                        error = %e,
                        "oauth refresh: failed to decrypt refresh token"
                    );
                    continue;
                }
            };

            last_refresh_attempts.insert(account_id, chrono::Utc::now());

            match TokenRefreshCoordinator::global()
                .refresh_and_store(
                    account.provider_id.as_str(),
                    provider,
                    &refresh_token,
                    &upstream_client,
                    account.id,
                    DbRef::Pool(&db_pool),
                    &master_key,
                )
                .await
            {
                Ok(token) => {
                    // Reset failure tracking on success.
                    failure_counts.remove(&account_id);
                    last_refresh_attempts.remove(&account_id);

                    let db_pool = db_pool.clone();
                    let acc_id = account.id;
                    let _ = tokio::task::spawn_blocking(move || {
                        let conn = db_pool.writer();
                        if let Err(e) =
                            crate::accounts::set_health(&conn, acc_id, HealthStatus::Healthy)
                        {
                            tracing::warn!(
                                account = account_id,
                                error = %e,
                                "oauth refresh: failed to set health to healthy"
                            );
                        }
                    })
                    .await;

                    tracing::info!(
                        account = account_id,
                        provider = %account.provider_id,
                        token_type = %token.token_type,
                        "oauth refresh: tokens refreshed successfully"
                    );
                }
                Err(e) => {
                    // Increment failure counter and update health status.
                    let count = failure_counts.entry(account_id).or_insert(0);
                    *count += 1;

                    let new_health = if *count >= UNHEALTHY_THRESHOLD {
                        HealthStatus::Unhealthy
                    } else {
                        HealthStatus::Degraded
                    };

                    let db_pool = db_pool.clone();
                    let acc_id = account.id;
                    let count_val = *count;
                    let provider_id_str = account.provider_id.as_str().to_string();
                    let _ = tokio::task::spawn_blocking(move || {
                        let conn = db_pool.writer();
                        if let Err(update_err) =
                            crate::accounts::set_health(&conn, acc_id, new_health)
                        {
                            tracing::warn!(
                                account = account_id,
                                error = %update_err,
                                "oauth refresh: failed to update health status"
                            );
                        }

                        if count_val >= UNHEALTHY_THRESHOLD {
                            let dedup_key = format!(
                                "{}:{}",
                                crate::notifications::CODE_OAUTH_EXPIRED,
                                account_id
                            );
                            let payload = serde_json::json!({
                                "code": crate::notifications::CODE_OAUTH_EXPIRED,
                                "message": format!(
                                    "OAuth token for account {} on {} expired or could not be refreshed ({} consecutive failures)",
                                    account_id, provider_id_str, count_val,
                                ),
                                "provider_id": &provider_id_str,
                                "details": {
                                    "account_id": account_id,
                                    "provider_id": &provider_id_str,
                                    "reason": "refresh_failed",
                                    "consecutive_failures": count_val,
                                },
                            });
                            let _ = crate::notifications::insert_and_broadcast(
                                &conn,
                                crate::notifications::KIND_SYSTEM,
                                &payload,
                                Some(&dedup_key),
                                Some(&provider_id_str),
                            );
                        }
                    }).await;

                    tracing::warn!(
                        account = account_id,
                        provider = %account.provider_id,
                        error = %e,
                        consecutive_failures = *count,
                        health = new_health.as_str(),
                        "oauth refresh: token refresh failed"
                    );
                }
            }

            // 2-second settle gap after each refresh (Auth0 protection).
            tokio::time::sleep(std::time::Duration::from_secs(SETTLE_GAP_SECS)).await;
        }

        // LEAK FIX: prune `failure_counts` / `last_refresh_attempts`
        // entries for accounts that no longer exist in the DB.
        // Without this, deleting an OAuth account leaves its failure
        // tracking entries in memory forever (~80 bytes each).
        {
            let live_account_ids: std::collections::HashSet<i64> = {
                let conn = db_pool.writer();
                match crate::accounts::list_oauth_account_ids(&conn) {
                    Ok(ids) => ids.into_iter().collect(),
                    Err(e) => {
                        tracing::debug!(
                            error = %e,
                            "oauth refresh: failed to list live account ids for prune"
                        );
                        // Skip this prune pass on DB error — don't
                        // block the refresh loop.
                        continue;
                    }
                }
            };
            let before_fc = failure_counts.len();
            let before_lra = last_refresh_attempts.len();
            failure_counts.retain(|id, _| live_account_ids.contains(id));
            last_refresh_attempts.retain(|id, _| live_account_ids.contains(id));
            let pruned_fc = before_fc - failure_counts.len();
            let pruned_lra = before_lra - last_refresh_attempts.len();
            if pruned_fc > 0 || pruned_lra > 0 {
                tracing::debug!(
                    pruned_failure_counts = pruned_fc,
                    pruned_last_refresh_attempts = pruned_lra,
                    "oauth refresh: pruned stale account tracking entries"
                );
            }
        }
    }
}

/// Compute exponential backoff in seconds for a given failure count.
/// Returns `BASE_BACKOFF_SECS * 2^(count-1)`, capped at `MAX_BACKOFF_SECS`.
fn backoff_seconds(failure_count: u32) -> u64 {
    if failure_count == 0 {
        return BASE_BACKOFF_SECS;
    }
    let shift = (failure_count - 1).min(31); // Prevent overflow on u64::wrapping_shl
    let raw = BASE_BACKOFF_SECS.wrapping_shl(shift);
    std::cmp::min(raw, MAX_BACKOFF_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::{Account, HealthStatus};
    use crate::ids::{AccountId, ProviderId};

    #[test]
    fn oauth_flow_str_roundtrip() {
        assert_eq!(OAuthFlow::DeviceCode.as_str(), "device_code");
        assert_eq!(
            OAuthFlow::AuthorizationCodePkce.as_str(),
            "authorization_code_pkce"
        );
    }

    #[test]
    fn oauth_flow_serde_roundtrip() {
        let flow = OAuthFlow::DeviceCode;
        let json = serde_json::to_string(&flow).unwrap();
        assert_eq!(json, "\"device_code\"");
        let back: OAuthFlow = serde_json::from_str(&json).unwrap();
        assert_eq!(back, OAuthFlow::DeviceCode);
    }

    #[test]
    fn token_response_deserialize() {
        let json = r#"{"access_token":"ya29.test","token_type":"Bearer","expires_in":3600,"refresh_token":"1//0test"}"#;
        let tr: TokenResponse = serde_json::from_str(json).unwrap();
        assert_eq!(tr.access_token, "ya29.test");
        assert_eq!(tr.token_type, "Bearer");
        assert_eq!(tr.expires_in, Some(3600));
        assert_eq!(tr.refresh_token.as_deref(), Some("1//0test"));
    }

    #[test]
    fn device_auth_response_deserialize() {
        let json = r#"{
            "deviceCode": "GmRhmhcxhwAzkoEqiMgzy",
            "userCode": "DJQR-KCZS",
            "verificationUri": "https://example.com/device",
            "expiresIn": 1800,
            "interval": 5
        }"#;
        let dar: DeviceAuthorizationResponse = serde_json::from_str(json).unwrap();
        assert_eq!(dar.device_code, "GmRhmhcxhwAzkoEqiMgzy");
        assert_eq!(dar.user_code, "DJQR-KCZS");
        assert_eq!(dar.verification_uri, "https://example.com/device");
        assert_eq!(dar.expires_in, Some(1800));
        assert_eq!(dar.interval, Some(5));
    }

    #[test]
    fn backoff_seconds_zero_failures() {
        assert_eq!(backoff_seconds(0), BASE_BACKOFF_SECS);
    }

    #[test]
    fn backoff_seconds_exponential_growth() {
        assert_eq!(backoff_seconds(1), 60); // 60 * 2^0
        assert_eq!(backoff_seconds(2), 120); // 60 * 2^1
        assert_eq!(backoff_seconds(3), 240); // 60 * 2^2
        assert_eq!(backoff_seconds(4), 480); // 60 * 2^3
    }

    #[test]
    fn backoff_seconds_caps_at_max() {
        assert_eq!(backoff_seconds(100), MAX_BACKOFF_SECS);
        assert_eq!(backoff_seconds(31), MAX_BACKOFF_SECS);
    }

    #[test]
    fn refresh_lead_seconds_rotating_providers() {
        // Auth0-backed rotating token providers: 5 minutes
        assert_eq!(refresh_lead_seconds("kiro"), 300);
        assert_eq!(refresh_lead_seconds("antigravity"), 300);
    }

    #[test]
    fn refresh_lead_seconds_non_rotating_providers() {
        // Non-rotating providers: 15 minutes (default)
        assert_eq!(refresh_lead_seconds("google"), 900);
        assert_eq!(refresh_lead_seconds("github"), 900);
        assert_eq!(refresh_lead_seconds("iflow"), 900);
        assert_eq!(refresh_lead_seconds("unknown-provider"), 900);
    }

    #[test]
    fn oauth_expires_soon_expired() {
        let account = dummy_account(Some("2020-01-01T00:00:00Z"));
        assert!(oauth_expires_soon(&account, "antigravity"));
    }

    #[test]
    fn oauth_expires_soon_due() {
        let expires_at = (chrono::Utc::now() + chrono::Duration::seconds(120))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let account = dummy_account(Some(&expires_at));
        assert!(oauth_expires_soon(&account, "antigravity"));
    }

    #[test]
    fn oauth_expires_soon_not_due() {
        let expires_at = (chrono::Utc::now() + chrono::Duration::seconds(3_600))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let account = dummy_account(Some(&expires_at));
        assert!(!oauth_expires_soon(&account, "antigravity"));
    }

    fn dummy_account(expires_at: Option<&str>) -> Account {
        Account {
            id: AccountId(1),
            provider_id: ProviderId::new("antigravity"),
            label: None,
            priority: 0,
            extra_config_json: None,
            health_status: HealthStatus::Healthy,
            rate_limited_until: None,
            quota_session_used: None,
            quota_session_limit: None,
            quota_session_reset_at: None,
            quota_weekly_used: None,
            quota_weekly_limit: None,
            quota_weekly_reset_at: None,
            quota_plan_name: None,
            quota_last_fetched_at: None,
            quota_fetch_error: None,
            quota_model_details: None,
            auth_type: "oauth".into(),
            email: Some("t@example.com".to_string()),
            oauth_scope: None,
            oauth_provider_specific: None,
            expires_at: expires_at.map(str::to_string),
            created_at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        }
    }
}
