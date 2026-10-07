use crate::accounts::{HealthStatus, StoreOAuthTokensParams, store_oauth_tokens};
use crate::error::{CoreError, Result};
use crate::ids::AccountId;
use governor::{Quota, RateLimiter};
use openproxy_adapters::upstream::UpstreamClient;
use openproxy_db::secrets::MasterKey;
use std::collections::HashMap;
use std::num::NonZero;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use super::{DbRef, OAuthProvider, OAuthProviderEnum, OAuthProviderRegistry, TokenResponse};

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
type AccountMutexMap = HashMap<AccountMutexKey, Arc<tokio::sync::Mutex<()>>>;

/// Coordinates OAuth refresh calls so every runtime path uses the same
/// serialization and persistence behavior.
///
/// The lock is scoped per `(provider_id, account_id)`. Serializing at the
/// account level prevents multiple concurrent requests or race conditions
/// between quota synchronization and background schedulers from attempting
/// parallel refresh operations on the same rotating refresh token (which causes
/// Auth0/MiniMax/Cline reuse detection to revoke the grant).
#[derive(Default)]
pub struct TokenRefreshCoordinator {
    account_mutexes: Arc<std::sync::Mutex<AccountMutexMap>>,
}

impl TokenRefreshCoordinator {
    pub fn new() -> Self {
        Self {
            account_mutexes: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

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
            && let Some((_, Some(access_token), maybe_rt, acc)) = check_res
        {
            tracing::info!(
                account = account_id.0,
                provider = provider_id,
                "oauth refresh: account already refreshed by concurrent caller, reusing current token"
            );
            return Ok(TokenResponse {
                access_token,
                token_type: "Bearer".to_string(),
                expires_in: None,
                refresh_token: maybe_rt,
                scope: acc.oauth_scope.map(|s| s.to_string()),
                id_token: None,
            });
        }

        // If SQLite holds a newer refresh token than the caller's parameter,
        // use the database's latest token so we never send a stale/consumed token.
        let effective_refresh_token = match &check_res {
            Some((_, _, Some(latest_rt), _)) if !latest_rt.is_empty() => latest_rt.as_str(),
            _ => refresh_token,
        };

        let token = provider
            .refresh_token(effective_refresh_token, upstream_client, account_id, db)
            .await?;
        let expires_at = token_expires_at(token.expires_in);

        let stored_token = token.clone();
        let store_master_key = master_key.clone();
        let email = provider.email_from_token(&token);
        db.with_conn_async(move |conn| {
            store_oauth_tokens(
                conn,
                account_id,
                &store_master_key,
                StoreOAuthTokensParams {
                    access_token: &stored_token.access_token,
                    refresh_token: stored_token.refresh_token.as_deref(),
                    token_type: &stored_token.token_type,
                    expires_at: expires_at.as_deref(),
                    scope: stored_token.scope.as_deref(),
                    provider_specific: None,
                    email: email.as_deref(),
                },
            )
        })
        .await?;

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

    // one clone per stage: DbPool is Arc-backed, so clones are cheap and
    // independent
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

    // the shared coordinator serializes per provider and applies the same
    // persistence the scheduler and pipeline use
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
///
/// Different providers need different refresh windows:
/// - **Rotating tokens** (Auth0-backed): 5 minutes before expiry to
///   avoid cascade revocation (Auth0 invalidates the old refresh token
///   when a new one is issued, so we must refresh early enough that
///   the new token is in place before the old one is needed).
/// - **Non-rotating tokens**: 15 minutes before expiry (standard
///   conservative window).
/// - **Special cases** (e.g. iflow): 24 hours before expiry.
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
pub(crate) const UNHEALTHY_THRESHOLD: u32 = 3;

/// Maximum backoff delay for retrying failed refreshes (1 hour).
pub(crate) const MAX_BACKOFF_SECS: u64 = 3600;

/// Base backoff interval in seconds (doubles each failure).
pub(crate) const BASE_BACKOFF_SECS: u64 = 60;

pub async fn start_refresh_scheduler(
    db_pool: std::sync::Arc<openproxy_db::DbPool>,
    master_key: std::sync::Arc<MasterKey>,
    upstream_client: Arc<UpstreamClient>,
    registry: Arc<OAuthProviderRegistry>,
    check_interval_secs: u64,
) {
    let cancel = CancellationToken::new();
    run_refresh_scheduler(
        db_pool,
        master_key,
        upstream_client,
        registry,
        check_interval_secs,
        cancel,
    )
    .await;
}

/// Run the OAuth token refresh scheduler with cancellation support.
///
/// Selects cancel ONLY on idle ticks; batches are awaited to completion so no
/// JoinSet tasks or database writes in flight are dropped. Checks cancellation between batches.
pub async fn run_refresh_scheduler(
    db_pool: std::sync::Arc<openproxy_db::DbPool>,
    master_key: std::sync::Arc<MasterKey>,
    upstream_client: Arc<UpstreamClient>,
    registry: Arc<OAuthProviderRegistry>,
    check_interval_secs: u64,
    cancel: CancellationToken,
) {
    if cancel.is_cancelled() {
        tracing::debug!("oauth refresh scheduler: cancelled before start");
        return;
    }

    let mut tick = tokio::time::interval(std::time::Duration::from_secs(check_interval_secs));
    // Skip the first immediate tick while respecting cancellation.
    tokio::select! {
        () = cancel.cancelled() => {
            tracing::debug!("oauth refresh scheduler: cancelled during initial tick");
            return;
        }
        _ = tick.tick() => {}
    }

    let mut failure_counts: HashMap<i64, u32> = HashMap::new();
    let mut last_refresh_attempts: HashMap<i64, chrono::DateTime<chrono::Utc>> = HashMap::new();

    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                tracing::info!("oauth refresh scheduler: cancelled during idle tick");
                break;
            }
            _ = tick.tick() => {}
        }

        if cancel.is_cancelled() {
            break;
        }

        tick_refresh_cycle(
            &db_pool,
            &master_key,
            &upstream_client,
            &registry,
            &mut failure_counts,
            &mut last_refresh_attempts,
        )
        .await;

        if cancel.is_cancelled() {
            break;
        }
    }
}

async fn tick_refresh_cycle(
    db_pool: &Arc<openproxy_db::DbPool>,
    master_key: &Arc<MasterKey>,
    upstream_client: &Arc<UpstreamClient>,
    registry: &Arc<OAuthProviderRegistry>,
    failure_counts: &mut HashMap<i64, u32>,
    last_refresh_attempts: &mut HashMap<i64, chrono::DateTime<chrono::Utc>>,
) {
    let accounts = fetch_expiring_oauth_accounts(db_pool, master_key).await;
    let accounts = filter_accounts_due(accounts);
    if accounts.is_empty() {
        return;
    }

    tracing::debug!(
        count = accounts.len(),
        "oauth refresh: accounts due for refresh"
    );

    let account_ids: Vec<_> = accounts.iter().map(|a| a.id).collect();
    let mut refresh_tokens = {
        let db_pool = Arc::clone(db_pool);
        let master_key = Arc::clone(master_key);
        let ids = account_ids;
        tokio::task::spawn_blocking(move || {
            let conn = db_pool.reader();
            crate::accounts::decrypt_refresh_tokens(&conn, &ids, &master_key)
        })
        .await
        .unwrap_or_else(|_| Ok(std::collections::HashMap::new()))
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "oauth refresh scheduler: failed to batch decrypt refresh tokens");
            std::collections::HashMap::new()
        })
    };

    let quota = match Quota::with_period(std::time::Duration::from_secs(STAGGER_DELAY_SECS)) {
        Some(q) => q.allow_burst(NonZero::<u32>::MIN),
        None => Quota::per_second(NonZero::<u32>::MIN),
    };
    let limiter = std::sync::Arc::new(RateLimiter::direct(quota));
    let mut join_set = tokio::task::JoinSet::new();

    for account in accounts {
        let Some(provider) = registry.get(account.provider_id.as_str()) else {
            tracing::debug!(
                provider = %account.provider_id,
                "oauth refresh: no provider impl found, skipping"
            );
            continue;
        };

        let account_id = account.id.0;
        if let Some(last_attempt) = last_refresh_attempts.get(&account_id) {
            let failure_count = failure_counts.get(&account_id).copied().unwrap_or(0);
            let backoff = backoff_seconds(failure_count);
            let elapsed = chrono::Utc::now().signed_duration_since(*last_attempt);
            if elapsed.num_seconds() < backoff as i64 {
                continue;
            }
        }

        let refresh_token = match refresh_tokens.remove(&account.id) {
            Some(Ok(Some(t))) => t,
            Some(Ok(None)) => {
                tracing::debug!(
                    account = account_id,
                    "oauth refresh: no refresh token stored, skipping"
                );
                continue;
            }
            Some(Err(e)) => {
                tracing::warn!(
                    account = account_id,
                    error = %e,
                    "oauth refresh: failed to decrypt refresh token"
                );
                continue;
            }
            None => {
                tracing::warn!(
                    account = account_id,
                    "oauth refresh: refresh token not found in batch"
                );
                continue;
            }
        };

        last_refresh_attempts.insert(account_id, chrono::Utc::now());

        let lim = Arc::clone(&limiter);
        let upstream_client = Arc::clone(upstream_client);
        let db_pool = Arc::clone(db_pool);
        let master_key = Arc::clone(master_key);

        join_set.spawn(async move {
            lim.until_ready().await;

            let res = TokenRefreshCoordinator::global()
                .refresh_and_store(OAuthRefreshParams {
                    provider_id: account.provider_id.as_str(),
                    provider,
                    refresh_token: &refresh_token,
                    upstream_client: &upstream_client,
                    account_id: account.id,
                    db: DbRef::Pool(&db_pool),
                    master_key: &master_key,
                    force: false,
                })
                .await;

            tokio::time::sleep(std::time::Duration::from_secs(SETTLE_GAP_SECS)).await;
            (account, res)
        });
    }

    while let Some(res) = join_set.join_next().await {
        let (account, result) = match res {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, "oauth refresh task panicked");
                continue;
            }
        };

        match result {
            Ok(token) => {
                handle_refresh_success(
                    db_pool,
                    &account,
                    &token,
                    failure_counts,
                    last_refresh_attempts,
                )
                .await;
            }
            Err(e) => {
                handle_refresh_failure(db_pool, &account, &e, failure_counts).await;
            }
        }
    }

    prune_stale_tracking_entries(db_pool, failure_counts, last_refresh_attempts).await;
}

async fn fetch_expiring_oauth_accounts(
    db_pool: &Arc<openproxy_db::DbPool>,
    master_key: &Arc<MasterKey>,
) -> Vec<crate::accounts::Account> {
    let db_pool = Arc::clone(db_pool);
    let master_key = Arc::clone(master_key);
    tokio::task::spawn_blocking(move || {
        let conn = db_pool.reader();
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
}

fn filter_accounts_due(accounts: Vec<crate::accounts::Account>) -> Vec<crate::accounts::Account> {
    let now = chrono::Utc::now();
    accounts
        .into_iter()
        .filter(|a| {
            if let Some(ref expires_at) = a.expires_at {
                let expires_at = match openproxy_types::timestamp::parse_timestamp(expires_at) {
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
        .collect()
}

async fn handle_refresh_success(
    db_pool: &Arc<openproxy_db::DbPool>,
    account: &crate::accounts::Account,
    token: &TokenResponse,
    failure_counts: &mut HashMap<i64, u32>,
    last_refresh_attempts: &mut HashMap<i64, chrono::DateTime<chrono::Utc>>,
) {
    let account_id = account.id.0;
    failure_counts.remove(&account_id);
    last_refresh_attempts.remove(&account_id);

    let db_pool = Arc::clone(db_pool);
    let acc_id = account.id;
    let _ = tokio::task::spawn_blocking(move || {
        let conn = db_pool.writer();
        if let Err(e) = crate::accounts::set_health(&conn, acc_id, HealthStatus::Healthy) {
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

async fn handle_refresh_failure(
    db_pool: &Arc<openproxy_db::DbPool>,
    account: &crate::accounts::Account,
    err: &openproxy_types::error::CoreError,
    failure_counts: &mut HashMap<i64, u32>,
) {
    let account_id = account.id.0;
    let count = failure_counts.entry(account_id).or_insert(0);
    *count += 1;

    let new_health = if *count >= UNHEALTHY_THRESHOLD {
        HealthStatus::Unhealthy
    } else {
        HealthStatus::Degraded
    };

    let db_pool = Arc::clone(db_pool);
    let acc_id = account.id;
    let count_val = *count;
    let provider_id_str = account.provider_id.as_str().to_string();
    let err_str = err.to_string();
    let is_unrecoverable = err_str.contains("invalid_grant")
        || err_str.contains("Invalid refresh token")
        || err_str.contains("invalid_client")
        || err_str.contains("unauthorized_client");

    let _ = tokio::task::spawn_blocking(move || {
        let conn = db_pool.writer();
        if is_unrecoverable {
            let _ = crate::accounts::clear_oauth_expires_at(&conn, acc_id.0);
        }
        if let Err(update_err) = crate::accounts::set_health(&conn, acc_id, new_health) {
            tracing::warn!(
                account = account_id,
                error = %update_err,
                "oauth refresh: failed to update health status"
            );
        }

        if count_val >= UNHEALTHY_THRESHOLD {
            let dedup_key = format!("{}:{}", crate::notifications::CODE_OAUTH_EXPIRED, account_id);
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
    })
    .await;

    tracing::warn!(
        account = account_id,
        provider = %account.provider_id,
        error = %err,
        consecutive_failures = *count,
        health = new_health.as_str(),
        "oauth refresh: token refresh failed"
    );
}

async fn prune_stale_tracking_entries(
    db_pool: &Arc<openproxy_db::DbPool>,
    failure_counts: &mut HashMap<i64, u32>,
    last_refresh_attempts: &mut HashMap<i64, chrono::DateTime<chrono::Utc>>,
) {
    let db_pool = Arc::clone(db_pool);
    let live_account_ids: Option<std::collections::HashSet<i64>> =
        tokio::task::spawn_blocking(move || {
            let conn = db_pool.reader();
            match crate::accounts::list_oauth_account_ids(&conn) {
                Ok(ids) => Some(ids.into_iter().collect()),
                Err(e) => {
                    tracing::debug!(
                        error = %e,
                        "oauth refresh: failed to list live account ids for prune"
                    );
                    None
                }
            }
        })
        .await
        .unwrap_or(None);

    if let Some(live_account_ids) = live_account_ids {
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

/// Compute exponential backoff in seconds for a given failure count.
/// Returns `BASE_BACKOFF_SECS * 2^(count-1)`, capped at `MAX_BACKOFF_SECS`.
pub(crate) fn backoff_seconds(failure_count: u32) -> u64 {
    if failure_count == 0 {
        return BASE_BACKOFF_SECS;
    }
    let shift = (failure_count - 1).min(31); // Prevent overflow on u64::wrapping_shl
    let raw = BASE_BACKOFF_SECS.wrapping_shl(shift);
    std::cmp::min(raw, MAX_BACKOFF_SECS)
}
