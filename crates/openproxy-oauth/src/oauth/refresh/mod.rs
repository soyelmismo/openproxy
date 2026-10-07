//! OAuth token refresh coordinator, on-demand token resolution, and background scheduler.

use std::sync::Arc;

use openproxy_adapters::upstream::UpstreamClient;
use openproxy_db::secrets::MasterKey;
use openproxy_types::error::{CoreError, Result};
use openproxy_types::ids::AccountId;

use super::{
    DbRef, OAuthProvider, OAuthProviderEnum, OAuthProviderRegistry, StoreOAuthTokensParams,
    TokenResponse, store_oauth_tokens,
};

pub mod scheduler;
pub use scheduler::*;

pub struct OAuthRefreshParams<'a> {
    pub provider_id: &'a str,
    pub provider: OAuthProviderEnum,
    pub refresh_token: &'a str,
    pub upstream_client: &'a Arc<UpstreamClient>,
    pub account_id: AccountId,
    pub db: DbRef<'a>,
    pub master_key: &'a MasterKey,
    pub force: bool,
}

type AccountMutexKey = (Box<str>, i64);
type AccountMutexMap = std::collections::HashMap<AccountMutexKey, Arc<tokio::sync::Mutex<()>>>;

/// Global coordinator for OAuth token refreshes.
///
/// Serializes concurrent refresh requests per `(provider_id, account_id)`.
/// If multiple requests arrive for the same account, only one performs the
/// upstream refresh; subsequent callers check the database under the lock
/// and return the freshly stored tokens without burning a rotating refresh token.
pub struct TokenRefreshCoordinator {
    account_mutexes: std::sync::Mutex<AccountMutexMap>,
}

impl Default for TokenRefreshCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl TokenRefreshCoordinator {
    pub fn new() -> Self {
        Self {
            account_mutexes: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Access the global coordinator singleton.
    pub fn global() -> &'static Self {
        static COORDINATOR: std::sync::OnceLock<TokenRefreshCoordinator> =
            std::sync::OnceLock::new();
        COORDINATOR.get_or_init(TokenRefreshCoordinator::new)
    }

    fn mutex_for_account(
        &self,
        provider_id: &str,
        account_id: AccountId,
    ) -> Result<Arc<tokio::sync::Mutex<()>>> {
        let mut map = self
            .account_mutexes
            .lock()
            .map_err(|e| CoreError::Internal(format!("account_mutexes lock poisoned: {e}")))?;
        let key = (Box::from(provider_id), account_id.0);
        if let Some(mutex) = map.get(&key) {
            return Ok(Arc::clone(mutex));
        }
        Ok(Arc::clone(
            map.entry(key)
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        ))
    }

    pub async fn refresh_and_store(&self, params: OAuthRefreshParams<'_>) -> Result<TokenResponse> {
        let OAuthRefreshParams {
            provider_id,
            provider,
            refresh_token,
            upstream_client,
            account_id,
            db,
            master_key,
            force,
        } = params;
        let mutex = self.mutex_for_account(provider_id, account_id)?;
        let _guard = mutex.lock().await;

        // Double-checked locking against the database:
        // Check if another concurrent task already refreshed this account while
        // we waited for the account lock (i.e. DB's latest refresh token differs
        // from the caller's parameter). If so, reuse the freshly stored tokens to
        // avoid burning rotating refresh tokens.
        let check_master_key = master_key.clone();
        let check_provider_id = provider_id.to_owned();
        let check_res = db
            .with_read_conn_async(move |conn| {
                let acc = openproxy_db::accounts::get(conn, account_id, &check_master_key)?;
                let Some(acc) = acc else {
                    return Ok(None);
                };
                let needs_refresh =
                    pipeline_token_needs_refresh(acc.expires_at.as_deref(), &check_provider_id);
                let access_token = openproxy_db::accounts::decrypt_access_token(
                    conn,
                    account_id,
                    &check_master_key,
                )
                .ok();
                let latest_refresh_token = openproxy_db::accounts::decrypt_refresh_token(
                    conn,
                    account_id,
                    &check_master_key,
                )
                .ok()
                .flatten();
                Ok(Some((
                    needs_refresh,
                    access_token,
                    latest_refresh_token,
                    acc,
                )))
            })
            .await?;

        let token_rotated = match &check_res {
            Some((_, _, Some(latest_rt), _)) => !latest_rt.is_empty() && latest_rt != refresh_token,
            _ => false,
        };

        if (token_rotated || (!force && matches!(&check_res, Some((false, Some(_), _, _)))))
            && let Some((_, Some(access_token), maybe_rt, acc)) = &check_res
        {
            tracing::info!(
                account = account_id.0,
                provider = provider_id,
                "oauth refresh: account already refreshed by concurrent caller, reusing current token"
            );
            return Ok(TokenResponse {
                access_token: access_token.clone(),
                token_type: "Bearer".to_string(),
                expires_in: None,
                refresh_token: maybe_rt.clone(),
                scope: acc.oauth_scope.as_ref().map(|s| s.to_string()),
                id_token: None,
            });
        }

        let mut effective_refresh_token = match &check_res {
            Some((_, _, Some(latest_rt), _)) if !latest_rt.is_empty() => latest_rt.clone(),
            _ => refresh_token.to_string(),
        };

        // Check if host CLI (Claude Code, Antigravity) updated tokens on disk
        if let Some((_, access_token_ref, _, acc)) = &check_res
            && let Some(discovered) = crate::account_scanner::check_local_cli_updated_tokens(
                provider_id,
                acc,
                access_token_ref.as_deref(),
                Some(&effective_refresh_token),
            )
        {
            tracing::info!(
                account = account_id.0,
                provider = provider_id,
                "oauth refresh: host CLI on disk has updated tokens for account; syncing to database"
            );
            let disk_token = discovered.clone();
            let store_master_key = master_key.clone();
            let disk_access = disk_token.access_token.clone();
            let disk_refresh = disk_token.refresh_token.clone();
            let disk_expires = disk_token.expires_at.clone();
            let disk_spec = disk_token.oauth_provider_specific.clone();
            let disk_email = disk_token.email.clone();
            db.with_conn_async(move |conn| {
                store_oauth_tokens(
                    conn,
                    account_id,
                    &store_master_key,
                    StoreOAuthTokensParams {
                        access_token: &disk_access,
                        refresh_token: disk_refresh.as_deref(),
                        token_type: "Bearer",
                        expires_at: disk_expires.as_deref(),
                        scope: None,
                        provider_specific: disk_spec.as_deref(),
                        email: disk_email.as_deref(),
                    },
                )
            })
            .await?;

            let disk_needs_refresh =
                pipeline_token_needs_refresh(disk_token.expires_at.as_deref(), provider_id);

            if !force && !disk_needs_refresh {
                tracing::info!(
                    account = account_id.0,
                    provider = provider_id,
                    "oauth refresh: synced token from host CLI is fresh, reusing without upstream call"
                );
                return Ok(TokenResponse {
                    access_token: disk_token.access_token,
                    token_type: "Bearer".to_string(),
                    expires_in: None,
                    refresh_token: disk_token.refresh_token,
                    scope: None,
                    id_token: None,
                });
            }

            if let Some(rt) = disk_token.refresh_token {
                effective_refresh_token = rt;
            }
        }

        let token = match provider
            .refresh_token(&effective_refresh_token, upstream_client, account_id, db)
            .await
        {
            Ok(tok) => tok,
            Err(e) => {
                let err_str = e.to_string();
                let is_invalid_grant = err_str.contains("invalid_grant")
                    || err_str.contains("Refresh token not found or invalid")
                    || err_str.contains("revoked");

                if is_invalid_grant
                    && let Some((_, access_token_ref, _, acc)) = &check_res
                    && let Some(discovered) = crate::account_scanner::check_local_cli_updated_tokens(
                        provider_id,
                        acc,
                        access_token_ref.as_deref(),
                        Some(&effective_refresh_token),
                    )
                    && discovered.refresh_token.as_deref() != Some(&effective_refresh_token)
                {
                    tracing::warn!(
                        account = account_id.0,
                        provider = provider_id,
                        "oauth refresh: DB token failed with invalid_grant, but host CLI on disk has newer refresh token; recovering from disk"
                    );
                    let disk_token = discovered.clone();
                    let store_master_key = master_key.clone();
                    let disk_access = disk_token.access_token.clone();
                    let disk_refresh = disk_token.refresh_token.clone();
                    let disk_expires = disk_token.expires_at.clone();
                    let disk_spec = disk_token.oauth_provider_specific.clone();
                    let disk_email = disk_token.email.clone();
                    db.with_conn_async(move |conn| {
                        store_oauth_tokens(
                            conn,
                            account_id,
                            &store_master_key,
                            StoreOAuthTokensParams {
                                access_token: &disk_access,
                                refresh_token: disk_refresh.as_deref(),
                                token_type: "Bearer",
                                expires_at: disk_expires.as_deref(),
                                scope: None,
                                provider_specific: disk_spec.as_deref(),
                                email: disk_email.as_deref(),
                            },
                        )
                    })
                    .await?;

                    if !pipeline_token_needs_refresh(disk_token.expires_at.as_deref(), provider_id)
                    {
                        return Ok(TokenResponse {
                            access_token: disk_token.access_token,
                            token_type: "Bearer".to_string(),
                            expires_in: None,
                            refresh_token: disk_token.refresh_token,
                            scope: None,
                            id_token: None,
                        });
                    }

                    if let Some(new_rt) = &disk_token.refresh_token {
                        provider
                            .refresh_token(new_rt, upstream_client, account_id, db)
                            .await?
                    } else {
                        return Err(e);
                    }
                } else {
                    return Err(e);
                }
            }
        };

        let expires_at = token_expires_at(token.expires_in);

        let stored_token = token.clone();
        let store_master_key = master_key.clone();
        let email = provider.email_from_token(&token);
        let store_token = stored_token.clone();
        let store_expires_at = expires_at.clone();
        let store_email = email.clone();
        db.with_conn_async(move |conn| {
            store_oauth_tokens(
                conn,
                account_id,
                &store_master_key,
                StoreOAuthTokensParams {
                    access_token: &store_token.access_token,
                    refresh_token: store_token.refresh_token.as_deref(),
                    token_type: &store_token.token_type,
                    expires_at: store_expires_at.as_deref(),
                    scope: store_token.scope.as_deref(),
                    provider_specific: None,
                    email: store_email.as_deref(),
                },
            )
        })
        .await?;

        // Propagate newly refreshed tokens back to host CLI if this account is active locally
        if let Some((_, _, _, acc)) = &check_res {
            crate::account_scanner::sync_to_local_cli_if_active(
                provider_id,
                acc,
                &stored_token.access_token,
                stored_token.refresh_token.as_deref(),
                expires_at.as_deref(),
                email.as_deref(),
            );
        }

        Ok(token)
    }
}

pub fn token_expires_at(expires_in: Option<u64>) -> Option<String> {
    expires_in.map(|secs| {
        (chrono::Utc::now() + chrono::Duration::seconds(secs as i64))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string()
    })
}

/// Resolve an OAuth access token for an account, refreshing it when
/// `oauth_expires_soon()` fires.
///
/// Each stage takes its own short-lived connection from `db_pool` so no SQLite
/// connection is held across an `.await`.
pub async fn resolve_oauth_token(
    db_pool: &openproxy_db::DbPool,
    account: &crate::accounts::Account,
    provider_id: &str,
    registry: &OAuthProviderRegistry,
    upstream_client: &std::sync::Arc<openproxy_adapters::upstream::UpstreamClient>,
    master_key: &MasterKey,
) -> Result<String> {
    use crate::accounts::{decrypt_access_token, decrypt_refresh_token};

    let pool_clone = db_pool.clone();
    let master_key_clone = master_key.clone();
    let account_id = account.id;

    let access_token = tokio::task::spawn_blocking(move || {
        let conn = pool_clone
            .try_reader_for(std::time::Duration::from_secs(5))
            .ok_or_else(|| CoreError::Internal("reader lock timeout".into()))?;
        decrypt_access_token(&conn, account_id, &master_key_clone)
    })
    .await
    .map_err(|e| CoreError::Internal(format!("spawn failed: {e}")))??;

    if !oauth_expires_soon(account, provider_id) {
        return Ok(access_token);
    }

    let pool_clone2 = db_pool.clone();
    let master_key_clone2 = master_key.clone();
    let refresh_token = tokio::task::spawn_blocking(move || {
        let conn = pool_clone2
            .try_reader_for(std::time::Duration::from_secs(5))
            .ok_or_else(|| CoreError::Internal("reader lock timeout".into()))?;
        decrypt_refresh_token(&conn, account_id, &master_key_clone2)
    })
    .await
    .map_err(|e| CoreError::Internal(format!("spawn failed: {e}")))?
    .map_err(|e| CoreError::Internal(format!("decrypt refresh token failed: {e}")))?
    .ok_or_else(|| {
        CoreError::Auth(format!(
            "account {} has no refresh token, cannot refresh",
            account.id.0
        ))
    })?;

    let provider = registry.get(provider_id).ok_or_else(|| {
        CoreError::Auth(format!("no OAuth provider registered for '{provider_id}'"))
    })?;

    tracing::info!(
        account = account.id.0,
        provider = provider_id,
        "oauth on-demand refresh: refreshing expiring token"
    );

    let token = TokenRefreshCoordinator::global()
        .refresh_and_store(OAuthRefreshParams {
            provider_id,
            provider,
            refresh_token: refresh_token.as_str(),
            upstream_client,
            account_id: account.id,
            db: DbRef::Pool(db_pool),
            master_key,
            force: false,
        })
        .await?;

    tracing::info!(
        account = account.id.0,
        provider = provider_id,
        "oauth on-demand refresh: token refreshed successfully"
    );

    Ok(token.access_token)
}

/// Lighter check than [`resolve_oauth_token`] for the pipeline's custom-provider
/// path, which only needs the expiry comparison.
pub fn pipeline_token_needs_refresh(db_expires_at: Option<&str>, provider_id: &str) -> bool {
    let Some(ts) = db_expires_at else {
        return false; // no expiry recorded: assume fresh
    };
    let Ok(expires_at) = openproxy_types::timestamp::parse_timestamp(ts) else {
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
pub fn refresh_lead_seconds(provider_id: &str) -> u64 {
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
    let Some(expires_at) = &account.expires_at else {
        return false;
    };

    let Ok(expires_at) = openproxy_types::timestamp::parse_timestamp(expires_at) else {
        return false;
    };
    let expires_at = expires_at.with_timezone(&chrono::Utc);
    let lead = refresh_lead_seconds(provider_id);
    let threshold = chrono::Utc::now() + chrono::Duration::seconds(lead as i64);

    expires_at <= threshold
}
