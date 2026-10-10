use std::collections::HashMap;
use std::num::NonZero;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use governor::{Quota, RateLimiter};
use openproxy_adapters::upstream::UpstreamClient;
use openproxy_db::secrets::MasterKey;
use openproxy_types::accounts::HealthStatus;
use tokio_util::sync::CancellationToken;

use crate::oauth::{
    DbRef, OAuthProviderRegistry, OAuthRefreshParams, TokenRefreshCoordinator, TokenResponse,
};

use super::refresh_lead_seconds;

/// Maximum refresh lead time across all providers (900s = 15 min).
/// Used as the SQL query window; per-provider filtering happens in Rust.
const MAX_REFRESH_LEAD_SECS: i64 = 900;

/// Anti-burst stagger delay between consecutive account refreshes.
const STAGGER_DELAY_SECS: u64 = 3;

/// Settle gap after each refresh to protect Auth0 from rapid-fire calls.
const SETTLE_GAP_SECS: u64 = 2;

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
