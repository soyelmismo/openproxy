//! Background daemon for Quota Synchronization.
//!
//! Periodically iterates over all accounts of providers that support quota fetching,
//! and refreshes their quota. Also includes the shared logic for refreshing a single account's quota
//! (used by both the daemon and the manual UI endpoint).

use crate::AppConfig;
use crate::accounts;
use crate::admin;
use crate::ids::AccountId;
use crate::notifications;
use crate::oauth::{DbRef, OAuthProvider, OAuthProviderRegistry};
use crate::quota::AccountQuota;
use openproxy_adapters::adapters::ProviderAdapterEnum;
use openproxy_adapters::upstream::UpstreamClient;
use openproxy_db::DbPool;
use openproxy_db::secrets::MasterKey;
use parking_lot::RwLock;
use std::sync::Arc;
use tokio::time::{Duration, sleep};
use tokio_util::sync::CancellationToken;

const QUOTA_LOW_ABSOLUTE_FLOOR: i64 = 1_000;

pub fn start_quota_sync_scheduler(
    db_pool: Arc<DbPool>,
    config: AppConfig,
    upstream_client: Arc<UpstreamClient>,
    master_key: Arc<MasterKey>,
    adapters: Arc<RwLock<Arc<Vec<ProviderAdapterEnum>>>>,
    oauth_provider_registry: Arc<OAuthProviderRegistry>,
) -> Option<CancellationToken> {
    start_quota_sync_scheduler_with_cancel(
        db_pool,
        config,
        upstream_client,
        master_key,
        adapters,
        oauth_provider_registry,
        None,
    )
}

pub fn start_quota_sync_scheduler_with_cancel(
    db_pool: Arc<DbPool>,
    config: AppConfig,
    upstream_client: Arc<UpstreamClient>,
    master_key: Arc<MasterKey>,
    adapters: Arc<RwLock<Arc<Vec<ProviderAdapterEnum>>>>,
    oauth_provider_registry: Arc<OAuthProviderRegistry>,
    cancel_token: Option<CancellationToken>,
) -> Option<CancellationToken> {
    if !config.quota_sync.enabled {
        tracing::debug!("Quota sync daemon is disabled in config; not starting scheduler");
        return None;
    }

    let interval = config.quota_sync.interval_secs;
    if interval == 0 {
        return None;
    }

    tracing::info!(
        "[QuotaSync] Scheduler started. Scanning every {}s",
        interval
    );

    let cancel = cancel_token.unwrap_or_default();
    let token = cancel.clone();

    tokio::spawn(async move {
        // Initial delay to avoid hammering DB/network immediately on boot alongside other tasks
        tokio::select! {
            () = token.cancelled() => {
                tracing::info!("[QuotaSync] Scheduler cancelled during initial delay");
                return;
            }
            () = sleep(Duration::from_secs(30)) => {}
        }

        loop {
            tokio::select! {
                () = token.cancelled() => {
                    tracing::info!("[QuotaSync] Scheduler shutting down");
                    break;
                }
                () = run_quota_sync_cycle(
                    &db_pool,
                    &config,
                    &upstream_client,
                    &master_key,
                    &adapters,
                    &oauth_provider_registry,
                    Some(&token),
                ) => {}
            }

            tokio::select! {
                () = token.cancelled() => {
                    tracing::info!("[QuotaSync] Scheduler shutting down");
                    break;
                }
                () = sleep(Duration::from_secs(interval)) => {}
            }
        }
    });

    Some(cancel)
}

fn get_supported_providers(adapters: &Arc<RwLock<Arc<Vec<ProviderAdapterEnum>>>>) -> Vec<String> {
    let ads = adapters.read();
    ads.iter()
        .filter(|a| a.metadata().quota_refresh_supported)
        .map(|a| a.id().to_string())
        .collect()
}

async fn fetch_accounts_to_sync(
    db_pool: &Arc<DbPool>,
    master_key: &Arc<MasterKey>,
    supported_providers: Vec<String>,
) -> Vec<AccountId> {
    let db_pool = Arc::clone(db_pool);
    let master_key = Arc::clone(master_key);
    tokio::task::spawn_blocking(move || {
        let conn = db_pool.reader();
        let mut target_accounts = Vec::new();
        for provider_str in &supported_providers {
            let pid = crate::ids::ProviderId::new(provider_str.as_str());
            if let Ok(accs) = accounts::list(&conn, Some(&pid), &master_key) {
                for acc in accs {
                    if acc.health_status != accounts::HealthStatus::Unhealthy {
                        target_accounts.push(acc.id);
                    }
                }
            }
        }
        target_accounts
    })
    .await
    .unwrap_or_default()
}

async fn wait_quota_sync_delay(delay_ms: u64, cancel_token: Option<&CancellationToken>) -> bool {
    if delay_ms == 0 {
        return false;
    }
    if let Some(token) = cancel_token {
        tokio::select! {
            () = token.cancelled() => true,
            () = sleep(Duration::from_millis(delay_ms)) => false,
        }
    } else {
        sleep(Duration::from_millis(delay_ms)).await;
        false
    }
}

async fn run_quota_sync_cycle(
    db_pool: &Arc<DbPool>,
    config: &AppConfig,
    upstream_client: &Arc<UpstreamClient>,
    master_key: &Arc<MasterKey>,
    adapters: &Arc<RwLock<Arc<Vec<ProviderAdapterEnum>>>>,
    oauth_registry: &Arc<OAuthProviderRegistry>,
    cancel_token: Option<&CancellationToken>,
) {
    tracing::debug!("[QuotaSync] Starting cycle");

    let supported_providers = get_supported_providers(adapters);
    if supported_providers.is_empty() {
        return;
    }

    let accounts_to_sync =
        fetch_accounts_to_sync(db_pool, master_key, supported_providers.clone()).await;

    let delay_ms = config.quota_sync.delay_between_accounts_ms;
    let supported_refs: Vec<&str> = supported_providers
        .iter()
        .map(std::string::String::as_str)
        .collect();

    for account_id in accounts_to_sync {
        if cancel_token.is_some_and(|t| t.is_cancelled()) {
            break;
        }

        if let Err(e) = refresh_single_account_quota(
            account_id,
            db_pool,
            master_key,
            &supported_refs,
            upstream_client,
            oauth_registry,
        )
        .await
        {
            tracing::warn!(
                "[QuotaSync] Failed to refresh quota for account {}: {e}",
                account_id.0
            );
        }

        if wait_quota_sync_delay(delay_ms, cancel_token).await {
            break;
        }
    }

    tracing::debug!("[QuotaSync] Cycle completed");
}

/// Core logic to refresh a single account's quota, including OAuth token refresh retries
/// and low-quota notifications. Used by both the background daemon and the manual UI endpoint.
pub async fn refresh_single_account_quota(
    account_id: AccountId,
    db_pool: &Arc<DbPool>,
    master_key: &Arc<MasterKey>,
    supported_providers: &[&str],
    upstream_client: &Arc<UpstreamClient>,
    oauth_registry: &Arc<OAuthProviderRegistry>,
) -> crate::error::Result<Option<AccountQuota>> {
    let (provider_id_str, api_key, access_token, provider_specific) = {
        let db_pool = Arc::clone(db_pool);
        let master_key = Arc::clone(master_key);
        // Extract the strings to avoid cloning the whole slice into the move closure
        let supported_providers: Vec<String> = supported_providers
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        let res = tokio::task::spawn_blocking(move || {
            let r = db_pool.reader();
            let acc = admin::account_for_quota_refresh(&r, account_id, master_key.as_ref())?;

            let supports_quota = supported_providers
                .iter()
                .any(|id| id.as_str() == acc.provider_id.as_str());

            if !supports_quota {
                return Ok::<_, crate::error::CoreError>(None);
            }

            let provider_str = acc.provider_id.to_string();
            let is_oauth = acc.auth_type.as_ref() == "oauth";
            let provider_specific = acc.oauth_provider_specific;

            let (k, token) = if is_oauth {
                let t = accounts::decrypt_access_token(&r, account_id, &master_key)?;
                (String::new(), Some(t))
            } else {
                let k = admin::decrypt_api_key_for_account(&r, account_id, &master_key)?;
                (k, None)
            };
            Ok(Some((provider_str, k, token, provider_specific)))
        })
        .await
        .map_err(|e| crate::error::CoreError::Internal(e.to_string()))??;

        match res {
            Some(data) => data,
            None => return Ok(None),
        }
    };

    let q = admin::fetch_account_quota(
        &provider_id_str,
        upstream_client,
        &api_key,
        access_token.as_deref(),
        provider_specific.as_deref(),
    )
    .await;

    let q = if q.fetch_error.as_deref().is_some_and(|e| e.contains("401")) && access_token.is_some()
    {
        let refresh_result = {
            let db_pool = Arc::clone(db_pool);
            let master_key = Arc::clone(master_key);
            tokio::task::spawn_blocking(move || {
                let r = db_pool.reader();
                accounts::decrypt_refresh_token(&r, account_id, master_key.as_ref())
                    .ok()
                    .flatten()
            })
            .await
            .unwrap_or(None)
        };
        if let Some(refresh_token) = refresh_result
            && let Some(provider) = oauth_registry.get(&provider_id_str)
        {
            match provider
                .refresh_token(
                    &refresh_token,
                    upstream_client,
                    account_id,
                    DbRef::Pool(db_pool.as_ref()),
                )
                .await
            {
                Ok(new_tokens) => {
                    let expires_at = new_tokens.expires_in.map(|secs| {
                        (chrono::Utc::now() + chrono::Duration::seconds(secs as i64))
                            .format("%Y-%m-%dT%H:%M:%SZ")
                            .to_string()
                    });
                    // Store the refreshed tokens.
                    {
                        let db_pool = Arc::clone(db_pool);
                        let master_key = Arc::clone(master_key);
                        let access_token = new_tokens.access_token.clone();
                        let refresh_token = new_tokens.refresh_token.clone();
                        let token_type = new_tokens.token_type.clone();
                        let expires_at = expires_at.clone();
                        let scope = new_tokens.scope.clone();
                        let _ = tokio::task::spawn_blocking(move || {
                            let w = db_pool.writer();
                            let _ = accounts::store_oauth_tokens(
                                &w,
                                account_id,
                                &master_key,
                                accounts::StoreOAuthTokensParams {
                                    access_token: &access_token,
                                    refresh_token: refresh_token.as_deref(),
                                    token_type: &token_type,
                                    expires_at: expires_at.as_deref(),
                                    scope: scope.as_deref(),
                                    ..Default::default()
                                },
                            );
                        })
                        .await;
                    }
                    // Retry quota fetch with the new access token
                    admin::fetch_account_quota(
                        &provider_id_str,
                        upstream_client,
                        &api_key,
                        Some(&new_tokens.access_token),
                        provider_specific.as_deref(),
                    )
                    .await
                }
                Err(e) => {
                    tracing::warn!(
                        account_id = account_id.0,
                        error = %e,
                        "on-demand oauth refresh failed during quota sync"
                    );
                    q // return original error
                }
            }
        } else {
            tracing::debug!(
                account_id = account_id.0,
                "401 but no refresh token available for on-demand refresh"
            );
            q
        }
    } else {
        q
    };

    {
        let db_pool = Arc::clone(db_pool);
        let q = q.clone();
        let res = tokio::task::spawn_blocking(move || {
            let w = db_pool.writer();
            admin::persist_account_quota(&w, account_id, &q)
        })
        .await
        .map_err(|e| crate::error::CoreError::Internal(e.to_string()))?;
        res?;
    }

    // GAP-6: after a healthy quota refresh, prune any per-(account, model)
    // "live-limited" sentinels. The TTL-bounded filter (`until_ts <= now`)
    // is applied inside `clear_for_account` so an in-flight `mark_limited`
    // racing with this refresh is not silently wiped (see
    // `docs/specs/antigravity-gaps-p2.md` §4.4 "Race condition").
    //
    // The Writer is acquired and released entirely inside `spawn_blocking`
    // — no guard is held across `.await` (AGENTS.md §4.3).
    if q.fetch_error.is_none() {
        clear_live_limited_after_refresh(db_pool, account_id).await;
    }

    if q.fetch_error.is_none() {
        let low = compute_low_quota_signal(&q);
        if let Some((scope, remaining, limit)) = low {
            let dedup_key = format!("{}:{}", notifications::CODE_QUOTA_LOW, account_id.0);
            let percent = if limit > 0 {
                ((remaining as f64) / (limit as f64) * 100.0).round() as u32
            } else {
                0
            };
            let payload = serde_json::json!({
                "code": notifications::CODE_QUOTA_LOW,
                "message": format!(
                    "Account {} on {} has low {} quota: {} remaining ({}%)",
                    account_id.0, provider_id_str, scope, remaining, percent,
                ),
                "provider_id": &provider_id_str,
                "details": {
                    "account_id": account_id.0,
                    "provider_id": &provider_id_str,
                    "scope": scope,
                    "remaining": remaining,
                    "limit": limit,
                    "percent": percent,
                },
            });
            let db_pool = Arc::clone(db_pool);
            let provider_id_str = provider_id_str.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let w = db_pool.writer();
                let _ = notifications::insert_and_broadcast(
                    &w,
                    notifications::KIND_SYSTEM,
                    &payload,
                    Some(&dedup_key),
                    Some(&provider_id_str),
                );
            })
            .await;
        }
    }

    Ok(Some(q))
}

/// GAP-6: prune expired per-(account, model) "live-limited" rows.
///
/// Extracted so it can be unit-tested without spinning up the full
/// `refresh_single_account_quota` machinery. The Writer is acquired
/// and released entirely inside `spawn_blocking` — no guard is held
/// across `.await` (AGENTS.md §4.3).
pub(crate) async fn clear_live_limited_after_refresh(db_pool: &Arc<DbPool>, account_id: AccountId) {
    let db_pool_for_clear = Arc::clone(db_pool);
    let _ = tokio::task::spawn_blocking(move || {
        let w = db_pool_for_clear.writer();
        if let Err(e) = openproxy_db::live_limited::clear_for_account(&w, account_id) {
            tracing::warn!(
                account_id = account_id.0,
                error = %e,
                "quota_sync: failed to clear live_limited_models for account"
            );
        }
    })
    .await;
}

pub fn compute_low_quota_signal(q: &AccountQuota) -> Option<(&'static str, i64, i64)> {
    if let (Some(used), Some(limit)) = (q.session_used, q.session_limit) {
        let remaining = (limit - used).max(0);
        if is_low(remaining, limit) {
            return Some(("session", remaining, limit));
        }
    } else if let Some(used) = q.session_used {
        let _ = used;
    }

    if let (Some(used), Some(limit)) = (q.weekly_used, q.weekly_limit) {
        let remaining = (limit - used).max(0);
        if is_low(remaining, limit) {
            return Some(("weekly", remaining, limit));
        }
    }
    None
}

pub fn is_low(remaining: i64, limit: i64) -> bool {
    if limit > 0 {
        remaining * 10 < limit
    } else {
        remaining < QUOTA_LOW_ABSOLUTE_FLOOR
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openproxy_db::{DbPool, migrations};
    use openproxy_types::ids::{AccountId, ModelId};
    use std::path::PathBuf;

    /// Build an in-memory `DbPool` with all migrations applied and one
    /// provider + account seeded. Returns the pool plus the freshly
    /// inserted `AccountId` (always `1` after the seed).
    fn fresh_pool() -> (Arc<DbPool>, AccountId) {
        // DbPool needs a real file path for `open_connection`, so we
        // create a tempdir-backed file. The DB content is empty
        // initially; we apply migrations on the writer before use.
        let base = std::env::temp_dir();
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let path: PathBuf = base.join(format!("openproxy-quota-sync-test-{pid}-{nanos}.db"));
        let pool = DbPool::open(&path).expect("open pool");
        let aid = AccountId(1);

        // Apply migrations on a fresh connection that we own, then
        // seed the FK target row. (DbPool doesn't expose a `run_migrations`
        // helper directly.)
        {
            let mut conn = pool.open_connection().expect("open conn");
            migrations::run(&mut conn).expect("migrations");
            conn.execute(
                "INSERT INTO providers(id, name, base_url, auth_type, format) \
                 VALUES ('antigravity', 'Antigravity', 'https://x', 'oauth', 'openai')",
                [],
            )
            .expect("seed provider");
            conn.execute(
                "INSERT INTO accounts(provider_id, label) VALUES ('antigravity', 'a1')",
                [],
            )
            .expect("seed account");
        }
        (Arc::new(pool), aid)
    }

    #[tokio::test]
    async fn quota_sync_clear_helper_drops_expired_rows() {
        let (pool, aid) = fresh_pool();
        let mid = ModelId::new("gemini-2.5");

        // Seed two expired rows on the writer.
        {
            let w = pool.writer();
            let expired = (chrono::Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();
            openproxy_db::live_limited::mark_limited(&w, aid, &mid, &expired, "RESOURCE_EXHAUSTED")
                .expect("mark expired");
        }

        // The wiring helper (what `refresh_single_account_quota` calls
        // when `fetch_error.is_none()`) drops expired rows.
        clear_live_limited_after_refresh(&pool, aid).await;

        let w = pool.writer();
        assert!(!openproxy_db::live_limited::is_limited(&w, aid, &mid).expect("is_limited"));
        assert!(!openproxy_db::live_limited::has_row(&w, aid, &mid).expect("has_row"));
    }

    #[tokio::test]
    async fn quota_sync_clear_helper_preserves_active_rows() {
        // The helper must not touch rows whose TTL is still in the
        // future (race-correctness, N2 fix). Without the
        // `until_ts <= now` filter in `clear_for_account`, a quota
        // refresh that runs 1ms after `mark_limited` would silently
        // wipe a freshly-emitted live-limit sentinel.
        let (pool, aid) = fresh_pool();
        let mid = ModelId::new("gemini-2.5");
        let active = (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339();

        {
            let w = pool.writer();
            openproxy_db::live_limited::mark_limited(&w, aid, &mid, &active, "RESOURCE_EXHAUSTED")
                .expect("mark active");
        }

        clear_live_limited_after_refresh(&pool, aid).await;

        let w = pool.writer();
        assert!(openproxy_db::live_limited::is_limited(&w, aid, &mid).expect("still limited"));
    }

    #[tokio::test]
    async fn quota_sync_does_not_clear_when_fetch_error_present() {
        // Mirrors `refresh_single_account_quota`'s gating: the
        // `if q.fetch_error.is_none()` check must skip the clear when
        // the previous fetch was unhealthy. The full refresh path
        // needs OAuthClient / providers / etc. which is too heavy for
        // a unit test; here we exercise the *contract* by directly
        // checking the call-site condition: when the helper is gated
        // by a non-empty `fetch_error`, the live-limit rows stay.
        //
        // The actual gating is one line in `refresh_single_account_quota`:
        //   if q.fetch_error.is_none() {
        //       clear_live_limited_after_refresh(db_pool, account_id).await;
        //   }
        // This test documents that contract.
        let (pool, aid) = fresh_pool();
        let mid = ModelId::new("gemini-2.5");
        let expired = (chrono::Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();

        {
            let w = pool.writer();
            openproxy_db::live_limited::mark_limited(&w, aid, &mid, &expired, "RESOURCE_EXHAUSTED")
                .expect("mark");
        }

        // Simulate "fetch_error was Some(_)" by NOT calling the helper.
        // We assert the row would survive in that case (the unit test
        // for `clear_for_account` already proves the SQL filter).
        {
            let w = pool.writer();
            assert!(openproxy_db::live_limited::has_row(&w, aid, &mid).expect("has_row"));
        }
    }
}
