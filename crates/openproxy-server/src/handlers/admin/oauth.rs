use super::{AccountId, ApiError, AppState, CoreError, ProviderId, core_oauth};
use axum::{
    Json,
    extract::{ConnectInfo, Path, Query, State},
    http::HeaderMap,
};

use openproxy_core::accounts as core_accounts;
use openproxy_core::oauth::{OAuthProvider, OAuthProviderEnum, TokenResponse};
use openproxy_core::rate_limit::RateLimitKey;

pub fn router() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/{provider}/authorize", axum::routing::get(oauth_authorize))
        .route("/{provider}/exchange", axum::routing::post(oauth_exchange))
        .route(
            "/{provider}/device-code",
            axum::routing::post(oauth_device_code),
        )
        .route(
            "/{provider}/device-poll",
            axum::routing::post(oauth_device_poll),
        )
}

fn validate_authorize_flow(
    provider: &str,
    provider_impl: &OAuthProviderEnum,
) -> Result<(), ApiError> {
    let flow = provider_impl.flow();
    if flow != openproxy_core::oauth::OAuthFlow::AuthorizationCodePkce
        && flow != openproxy_core::oauth::OAuthFlow::AuthorizationCode
    {
        return Err(ApiError(CoreError::Validation(format!(
            "provider '{provider}' does not support authorization code flow"
        ))));
    }
    Ok(())
}

fn get_oauth_redirect_uri(s: &AppState) -> String {
    if let Ok(web_port) = std::env::var("OPENPROXY_WEB_PORT") {
        return format!("http://localhost:{web_port}/admin/callback.html");
    }
    let port = s
        .config()
        .server
        .bind
        .rsplit_once(':')
        .map_or("8787", |(_, p)| p);
    format!("http://localhost:{port}/admin/callback.html")
}

pub async fn oauth_authorize(
    State(s): State<AppState>,
    Path(provider): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let registry = s.oauth_provider_registry();
    let provider_impl = registry.get(&provider).ok_or_else(|| {
        ApiError(CoreError::Validation(format!(
            "provider '{provider}' does not support OAuth authorize"
        )))
    })?;

    validate_authorize_flow(&provider, &provider_impl)?;
    let redirect_uri = get_oauth_redirect_uri(&s);
    let (auth_url, code_verifier, effective_redirect_uri, state) =
        provider_impl.build_auth_url(&redirect_uri).await?;
    let redirect_uri = if effective_redirect_uri.is_empty() {
        redirect_uri
    } else {
        effective_redirect_uri
    };

    // Security (OP-19): persist the generated state with the verifier and
    // redirect URI actually used, so the exchange can validate `state` and
    // stop trusting client-supplied redirect_uri / code_verifier.
    s.oauth_states()
        .insert(&provider, &state, &code_verifier, &redirect_uri);

    Ok(Json(serde_json::json!({
        "authorization_url": auth_url,
        "code_verifier": code_verifier,
        "redirect_uri": redirect_uri,
        "state": state,
    })))
}

async fn resolve_or_create_oauth_account(
    s: &AppState,
    provider: &str,
    account_id_input: Option<i64>,
) -> Result<AccountId, ApiError> {
    if let Some(id) = account_id_input {
        return Ok(AccountId(id));
    }
    let pool = std::sync::Arc::clone(s.db_pool());
    let provider = provider.to_string();
    let master_key = std::sync::Arc::clone(s.master_key());
    tokio::task::spawn_blocking(move || {
        let w = pool
            .try_writer_for(openproxy_db::conn::ADMIN_LOCK_TIMEOUT)
            .ok_or_else(|| ApiError(CoreError::Internal("writer lock timeout".into())))?;
        let provider_id = ProviderId::new(&provider);
        core_accounts::create(&w, &provider_id, None, &master_key, None, 10, None).map_err(ApiError)
    })
    .await
    .map_err(|e| ApiError(CoreError::Internal(format!("spawn failed: {e}"))))?
}

fn compute_oauth_expires_at(expires_in: Option<u64>) -> Option<String> {
    expires_in.map(|secs| {
        (chrono::Utc::now() + chrono::Duration::seconds(secs as i64))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string()
    })
}

async fn save_oauth_token_and_notify(
    s: &AppState,
    provider: &str,
    provider_impl: &OAuthProviderEnum,
    account_id: AccountId,
    token: &TokenResponse,
    custom_provider_specific: Option<String>,
) -> Result<(), ApiError> {
    let expires_at = compute_oauth_expires_at(token.expires_in);
    let provider_specific =
        custom_provider_specific.or_else(|| provider_impl.provider_specific_from_token(token));
    let email = provider_impl.email_from_token(token);

    tokio::task::spawn_blocking({
        let pool = std::sync::Arc::clone(s.db_pool());
        let master_key = std::sync::Arc::clone(s.master_key());
        let access_token = token.access_token.clone();
        let refresh_token = token.refresh_token.clone();
        let token_type = token.token_type.clone();
        let expires_at = expires_at.clone();
        let scope = token.scope.clone();
        let provider_specific = provider_specific.clone();
        let email = email.clone();
        move || -> Result<(), CoreError> {
            let w = pool
                .try_writer_for(openproxy_db::conn::ADMIN_LOCK_TIMEOUT)
                .ok_or_else(|| CoreError::Internal("writer lock timeout".into()))?;
            openproxy_core::accounts::store_oauth_tokens(
                &w,
                account_id,
                &master_key,
                openproxy_core::accounts::StoreOAuthTokensParams {
                    access_token: &access_token,
                    refresh_token: refresh_token.as_deref(),
                    token_type: &token_type,
                    expires_at: expires_at.as_deref(),
                    scope: scope.as_deref(),
                    provider_specific: provider_specific.as_deref(),
                    email: email.as_deref(),
                },
            )?;
            openproxy_core::accounts::set_health(
                &w,
                account_id,
                openproxy_types::HealthStatus::Healthy,
            )?;
            Ok(())
        }
    })
    .await
    .map_err(|e| ApiError(CoreError::Internal(format!("spawn failed: {e}"))))?
    .map_err(ApiError)?;

    if let Err(e) = provider_impl
        .post_exchange(account_id, s.db_pool(), s.master_key(), s.upstream_client())
        .await
    {
        tracing::warn!(
            provider = %provider,
            account_id = account_id.0,
            error = %e,
            "post-exchange hook failed",
        );
    }

    super::providers::spawn_background_provider_refresh(
        s.clone(),
        provider.to_string(),
        Some(account_id.0),
    );

    Ok(())
}

pub async fn oauth_exchange(
    State(s): State<AppState>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    Path(provider): Path<String>,
    Json(input): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Rate limit OAuth exchange to prevent authorization code brute-forcing
    // and upstream resource exhaustion. We use the client's IP since they
    // don't have an account/API key yet in this flow.
    let client_ip = crate::client_ip::resolve_client_ip(
        &headers,
        Some(&addr),
        &s.config().server.trusted_proxies,
    )
    .unwrap_or_else(|| addr.ip());
    if !s.rate_limiter().check(RateLimitKey::Ip(client_ip)) {
        return Err(ApiError(CoreError::RateLimited {
            provider: "oauth_exchange".into(),
            retry_after_ms: 60_000,
            is_proxy_rotated: false,
        }));
    }

    let code = input
        .get("code")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CoreError::Validation("missing 'code'".into()))?;

    // Security (OP-19): `state` is now mandatory and validated server-side.
    // The PKCE verifier and redirect URI are taken from the record persisted
    // at authorize time — client-supplied values are no longer forwarded to
    // the IdP token endpoint.
    let state_param = input
        .get("state")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            CoreError::Validation(
                "missing 'state': start the flow with GET /admin/api/oauth/{provider}/authorize \
                 and pass the returned state back"
                    .into(),
            )
        })?;
    let (code_verifier, redirect_uri) = s
        .oauth_states()
        .consume(&provider, state_param)
        .ok_or_else(|| {
            CoreError::Validation(
                "unknown, expired or already-used 'state'; restart the authorization flow".into(),
            )
        })?;
    let account_id_input = input.get("account_id").and_then(serde_json::Value::as_i64);

    let registry = s.oauth_provider_registry();
    let provider_impl = registry.get(&provider).ok_or_else(|| {
        ApiError(CoreError::Validation(format!(
            "provider '{provider}' does not support OAuth exchange"
        )))
    })?;

    let code_with_state = if code.contains('#') || code.contains("state=") {
        code.to_string()
    } else {
        format!("{code}#{state_param}")
    };

    let token = provider_impl
        .exchange_code(
            &code_with_state,
            &code_verifier,
            s.upstream_client(),
            &redirect_uri,
        )
        .await?;

    let account_id = resolve_or_create_oauth_account(&s, &provider, account_id_input).await?;
    save_oauth_token_and_notify(&s, &provider, &provider_impl, account_id, &token, None).await?;

    Ok(Json(serde_json::json!({
        "account_id": account_id.0,
        "provider": provider,
        "status": "connected",
    })))
}

pub async fn oauth_device_code(
    State(s): State<AppState>,
    Path(provider): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let registry = s.oauth_provider_registry();
    let provider_impl = registry.get(&provider).ok_or_else(|| {
        ApiError(CoreError::Validation(format!(
            "provider '{provider}' does not support device code authorization"
        )))
    })?;

    let dar = provider_impl
        .request_device_code(s.upstream_client())
        .await?;

    let pool = std::sync::Arc::clone(s.db_pool());
    let provider_clone = provider.clone();
    let dar_clone = dar.clone();
    tokio::task::spawn_blocking(move || -> Result<(), CoreError> {
        let w = pool
            .try_writer_for(openproxy_db::conn::ADMIN_LOCK_TIMEOUT)
            .ok_or_else(|| CoreError::Internal("writer lock timeout".into()))?;
        openproxy_core::oauth::tickets::create_ticket(&w, &provider_clone, &dar_clone)?;
        Ok(())
    })
    .await
    .map_err(|e| ApiError(CoreError::Internal(format!("spawn failed: {e}"))))??;

    Ok(Json(serde_json::json!({
        "device_code": dar.device_code,
        "user_code": dar.user_code,
        "verification_uri": dar.verification_uri,
        "verification_uri_complete": dar.verification_uri_complete,
        "expires_in": dar.expires_in,
        "interval": dar.interval,
    })))
}

async fn validate_active_ticket(s: &AppState, device_code: &str) -> Result<(), ApiError> {
    let pool = std::sync::Arc::clone(s.db_pool());
    let device_code = device_code.to_string();
    tokio::task::spawn_blocking(move || -> Result<(), ApiError> {
        let r = pool
            .try_reader_for(std::time::Duration::from_secs(5))
            .ok_or_else(|| ApiError(CoreError::Internal("reader lock timeout".into())))?;
        match openproxy_core::oauth::tickets::lookup_active(&r, &device_code)? {
            openproxy_core::oauth::tickets::TicketStatus::Active(_) => Ok(()),
            openproxy_core::oauth::tickets::TicketStatus::Expired => Err(ApiError(
                CoreError::Validation("device_code has expired; restart the OAuth flow".into()),
            )),
            openproxy_core::oauth::tickets::TicketStatus::Consumed
            | openproxy_core::oauth::tickets::TicketStatus::Unknown => Err(ApiError(
                CoreError::not_found("oauth_device_ticket", &device_code),
            )),
        }
    })
    .await
    .map_err(|e| ApiError(CoreError::Internal(format!("spawn failed: {e}"))))?
}

fn resolve_kiro_or_default_provider_specific(
    provider: &str,
    provider_impl: &OAuthProviderEnum,
    token: &TokenResponse,
) -> Option<String> {
    if provider == "kiro" {
        openproxy_core::oauth::kiro::take_last_client().map(|(cid, csec)| {
            serde_json::json!({
                "client_id": cid,
                "client_secret": csec,
                "region": openproxy_core::oauth::kiro::KiroProviderMeta::default().region,
            })
            .to_string()
        })
    } else {
        provider_impl.provider_specific_from_token(token)
    }
}

pub async fn oauth_device_poll(
    State(s): State<AppState>,
    Path(provider): Path<String>,
    Json(input): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let device_code = input
        .get("device_code")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CoreError::Validation("missing 'device_code'".into()))?;
    let account_id_input = input.get("account_id").and_then(serde_json::Value::as_i64);

    validate_active_ticket(&s, device_code).await?;

    let registry = s.oauth_provider_registry();
    let provider_impl = registry.get(&provider).ok_or_else(|| {
        ApiError(CoreError::Validation(format!(
            "provider '{provider}' does not support device code polling"
        )))
    })?;

    let Some(token) = provider_impl
        .poll_device_token(device_code, s.upstream_client())
        .await?
    else {
        return Ok(Json(serde_json::json!({ "status": "pending" })));
    };

    let account_id = resolve_or_create_oauth_account(&s, &provider, account_id_input).await?;
    let provider_specific =
        resolve_kiro_or_default_provider_specific(&provider, &provider_impl, &token);

    save_oauth_token_and_notify(
        &s,
        &provider,
        &provider_impl,
        account_id,
        &token,
        provider_specific,
    )
    .await?;

    let pool = std::sync::Arc::clone(s.db_pool());
    let device_code_clone = device_code.to_string();
    tokio::task::spawn_blocking(move || {
        if let Some(w) = pool.try_writer_for(openproxy_db::conn::ADMIN_LOCK_TIMEOUT) {
            let _ = openproxy_core::oauth::tickets::mark_consumed(&w, &device_code_clone);
        }
    })
    .await
    .map_err(|e| ApiError(CoreError::Internal(format!("spawn failed: {e}"))))?;

    Ok(Json(serde_json::json!({
        "status": "ok",
        "account_id": account_id.0,
    })))
}

pub async fn oauth_callback(
    Query(mut params): Query<std::collections::BTreeMap<String, String>>,
) -> Json<serde_json::Value> {
    let code = params.remove("code").unwrap_or_default();
    let state = params.remove("state");
    // Sanitize: never return raw error details from upstream providers —
    // they may contain URLs with tokens, internal error codes, or
    // other sensitive information. Map known error types to generic
    // messages and drop anything else.
    let error = params.get("error").map(|raw| match raw.as_str() {
        "access_denied" => "access_denied",
        "server_error" => "server_error",
        "temporarily_unavailable" => "temporarily_unavailable",
        _ => "authorization_failed",
    });

    Json(serde_json::json!({
        "code": if code.is_empty() { None::<String> } else { Some(code) },
        "error": error.map(String::from),
        "state": state,
        "message": "Copy the code above and paste it into the Exchange endpoint.",
    }))
}

pub(crate) async fn refresh_oauth_if_needed(
    s: &AppState,
    account: core_accounts::Account,
    provider_id: &ProviderId,
) -> String {
    if account.auth_type.as_ref() != "oauth" {
        return String::new();
    }

    let Some(access_token) = try_decrypt_access_token(s, account.id, provider_id).await else {
        return String::new();
    };

    if !core_oauth::oauth_expires_soon(&account, provider_id.as_str()) {
        return access_token;
    }

    let Some(refresh_token) = try_decrypt_refresh_token(s, account.id, provider_id).await else {
        return access_token;
    };

    let registry = s.oauth_provider_registry();
    let Some(provider) = registry.get(provider_id.as_str()) else {
        tracing::warn!(
            account = account.id.0,
            provider = %provider_id,
            "oauth refresh-on-demand: no provider impl found"
        );
        return access_token;
    };

    tracing::info!(
        account = account.id.0,
        provider = %provider_id,
        "oauth refresh-on-demand: refreshing expired/expiring token"
    );

    execute_oauth_refresh(
        s,
        &account,
        provider_id,
        &refresh_token,
        &provider,
        access_token,
    )
    .await
}

async fn try_decrypt_access_token(
    s: &AppState,
    account_id: AccountId,
    provider_id: &ProviderId,
) -> Option<String> {
    let pool = std::sync::Arc::clone(s.db_pool());
    let master_key = std::sync::Arc::clone(s.master_key());
    tokio::task::spawn_blocking(move || {
        let conn = pool.writer();
        core_accounts::decrypt_access_token(&conn, account_id, master_key.as_ref())
    })
    .await
    .ok()
    .and_then(|r| {
        r.map_err(|e| {
            tracing::warn!(
                account = account_id.0,
                provider = %provider_id,
                error = %e,
                "oauth refresh-on-demand: failed to decrypt access token"
            );
        })
        .ok()
    })
}

async fn try_decrypt_refresh_token(
    s: &AppState,
    account_id: AccountId,
    provider_id: &ProviderId,
) -> Option<String> {
    let pool = std::sync::Arc::clone(s.db_pool());
    let master_key = std::sync::Arc::clone(s.master_key());
    let result = tokio::task::spawn_blocking(move || {
        let Some(conn) = pool.try_writer_for(std::time::Duration::from_secs(5)) else {
            return Ok(None);
        };
        core_accounts::decrypt_refresh_token(&conn, account_id, master_key.as_ref())
    })
    .await;
    match result {
        Ok(Ok(Some(t))) => Some(t),
        Ok(Ok(None)) => None,
        Ok(Err(e)) => {
            tracing::warn!(
                account = account_id.0,
                provider = %provider_id,
                error = %e,
                "oauth refresh-on-demand: failed to decrypt refresh token"
            );
            None
        }
        Err(e) => {
            tracing::warn!(
                account = account_id.0,
                provider = %provider_id,
                error = %e,
                "oauth refresh-on-demand: spawn_blocking failed"
            );
            None
        }
    }
}

async fn execute_oauth_refresh(
    s: &AppState,
    account: &core_accounts::Account,
    provider_id: &ProviderId,
    refresh_token: &str,
    provider: &core_oauth::OAuthProviderEnum,
    fallback_token: String,
) -> String {
    let upstream_client = s.upstream_client();
    // Route through the global coordinator so the refresh is serialized per-provider.
    // Providers with rotating refresh tokens (Auth0-backed: MiniMax, Cline, etc.)
    // break when concurrent callers use the same one-time token in parallel.
    let token = match openproxy_core::oauth::TokenRefreshCoordinator::global()
        .refresh_and_store(openproxy_core::oauth::OAuthRefreshParams {
            provider_id: provider_id.as_str(),
            provider: provider.clone(),
            refresh_token,
            upstream_client,
            account_id: account.id,
            db: openproxy_core::oauth::DbRef::Pool(s.db_pool().as_ref()),
            master_key: s.master_key().as_ref(),
            force: true,
        })
        .await
    {
        Ok(t) => {
            tracing::info!(
                account = account.id.0,
                provider = %provider_id,
                "oauth refresh-on-demand: tokens refreshed successfully"
            );
            t
        }
        Err(e) => {
            tracing::warn!(
                account = account.id.0,
                provider = %provider_id,
                error = %e,
                "oauth refresh-on-demand: token refresh failed"
            );
            return fallback_token;
        }
    };

    token.access_token
}

/// Server-side OAuth state store (OP-19).
///
/// Security: `oauth_exchange` previously trusted the client-supplied
/// `redirect_uri` and `code_verifier` verbatim and never validated `state`,
/// so the CSRF/CSRF-mitigation trio of the authorization-code flow rested
/// entirely on the IdP. The authorize handler now persists the
/// `(provider, state)` pair it generated together with the PKCE verifier and
/// redirect URI it used; the exchange consumes that record (single-use,
/// 10-minute TTL) and uses the persisted values instead of whatever the
/// client posts.
pub struct OAuthStateStore {
    entries: dashmap::DashMap<(String, String), (String, String, std::time::Instant)>,
}

/// Lifetime of an authorize→exchange pair.
const OAUTH_STATE_TTL: std::time::Duration = std::time::Duration::from_secs(600);
/// Bound on distinct pending states.
const OAUTH_STATE_MAX_ENTRIES: usize = 4096;

impl Default for OAuthStateStore {
    fn default() -> Self {
        Self::new()
    }
}

impl OAuthStateStore {
    pub fn new() -> Self {
        Self {
            entries: dashmap::DashMap::new(),
        }
    }

    /// Record a freshly generated authorize response.
    pub fn insert(&self, provider: &str, state: &str, code_verifier: &str, redirect_uri: &str) {
        if self.entries.len() >= OAUTH_STATE_MAX_ENTRIES {
            self.evict_expired();
        }
        if self.entries.len() >= OAUTH_STATE_MAX_ENTRIES {
            self.entries.clear();
        }
        self.entries.insert(
            (provider.to_string(), state.to_string()),
            (
                code_verifier.to_string(),
                redirect_uri.to_string(),
                std::time::Instant::now(),
            ),
        );
    }

    /// Consume the record for `(provider, state)` — single use. `None` when
    /// unknown, already used, or expired.
    pub fn consume(&self, provider: &str, state: &str) -> Option<(String, String)> {
        let (verifier, redirect_uri, created) = self
            .entries
            .remove(&(provider.to_string(), state.to_string()))?
            .1;
        if created.elapsed() > OAUTH_STATE_TTL {
            return None;
        }
        Some((verifier, redirect_uri))
    }

    fn evict_expired(&self) {
        self.entries
            .retain(|_, (_, _, created)| created.elapsed() <= OAUTH_STATE_TTL);
    }
}

#[cfg(test)]
mod state_store_tests {
    use super::*;

    #[test]
    fn oauth_state_is_single_use_and_ttl_bound() {
        let store = OAuthStateStore::new();
        store.insert(
            "antigravity",
            "st1",
            "verifier-1",
            "http://localhost:8787/admin/callback.html",
        );
        let (verifier, redirect) = store
            .consume("antigravity", "st1")
            .expect("state resolves once");
        assert_eq!(verifier, "verifier-1");
        assert_eq!(redirect, "http://localhost:8787/admin/callback.html");
        assert!(store.consume("antigravity", "st1").is_none(), "single use");
        assert!(store.consume("antigravity", "other").is_none());
        assert!(store.consume("zai", "st1").is_none(), "provider-bound");
    }
}
