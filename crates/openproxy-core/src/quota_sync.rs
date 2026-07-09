//! Background daemon for Quota Synchronization.
//!
//! Periodically iterates over all accounts of providers that support quota fetching,
//! and refreshes their quota. Also includes the shared logic for refreshing a single account's quota
//! (used by both the daemon and the manual UI endpoint).

use crate::AppConfig;
use crate::accounts;
use crate::adapters::{ProviderAdapter, ProviderAdapterEnum};
use crate::admin;
use crate::db::DbPool;
use crate::ids::AccountId;
use crate::notifications;
use crate::oauth::{DbRef, OAuthProvider, OAuthProviderRegistry};
use crate::quota::AccountQuota;
use crate::secrets::MasterKey;
use crate::upstream::UpstreamClient;
use parking_lot::RwLock;
use std::sync::Arc;
use tokio::time::{Duration, sleep};

const QUOTA_LOW_ABSOLUTE_FLOOR: i64 = 1000;

pub async fn start_quota_sync_scheduler(
    db_pool: Arc<DbPool>,
    config: AppConfig,
    upstream_client: Arc<UpstreamClient>,
    master_key: Arc<MasterKey>,
    adapters: Arc<RwLock<Vec<ProviderAdapterEnum>>>,
    oauth_provider_registry: Arc<OAuthProviderRegistry>,
) {
    if !config.quota_sync.enabled {
        tracing::debug!("Quota sync daemon is disabled in config; not starting scheduler");
        return;
    }

    let interval = config.quota_sync.interval_secs;
    if interval == 0 {
        return;
    }

    tracing::info!(
        "[QuotaSync] Scheduler started. Scanning every {}s",
        interval
    );

    tokio::spawn(async move {
        // Initial delay to avoid hammering DB/network immediately on boot alongside other tasks
        sleep(Duration::from_secs(30)).await;

        loop {
            run_quota_sync_cycle(
                &db_pool,
                &config,
                &upstream_client,
                &master_key,
                &adapters,
                &oauth_provider_registry,
            )
            .await;
            sleep(Duration::from_secs(interval)).await;
        }
    });
}

async fn run_quota_sync_cycle(
    db_pool: &Arc<DbPool>,
    config: &AppConfig,
    upstream_client: &Arc<UpstreamClient>,
    master_key: &Arc<MasterKey>,
    adapters: &Arc<RwLock<Vec<ProviderAdapterEnum>>>,
    oauth_registry: &Arc<OAuthProviderRegistry>,
) {
    tracing::debug!("[QuotaSync] Starting cycle");

    // 1. Identify which providers support quota fetching
    let supported_providers: Vec<String> = {
        let ads = adapters.read();
        ads.iter()
            .filter(|a| a.metadata().quota_refresh_supported)
            .map(|a| a.id().to_string())
            .collect()
    };

    if supported_providers.is_empty() {
        return;
    }

    // 2. Fetch all healthy accounts for these providers
    let accounts_to_sync: Vec<AccountId> = {
        let conn = db_pool.reader();
        let mut target_accounts = Vec::new();
        for provider_str in &supported_providers {
            let pid = crate::ids::ProviderId::new(provider_str.clone());
            if let Ok(accs) = accounts::list(&conn, Some(&pid)) {
                for acc in accs {
                    if acc.health_status != accounts::HealthStatus::Unhealthy {
                        target_accounts.push(acc.id);
                    }
                }
            }
        }
        target_accounts
    };

    let delay_ms = config.quota_sync.delay_between_accounts_ms;

    // 3. Process each account with a delay
    let ads = adapters.read().clone();
    for account_id in accounts_to_sync {
        match refresh_single_account_quota(
            account_id,
            db_pool,
            master_key,
            &ads,
            upstream_client,
            oauth_registry,
        )
        .await
        {
            Ok(Some(_)) => {}
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    "[QuotaSync] Failed to refresh quota for account {}: {}",
                    account_id.0,
                    e
                );
            }
        }

        if delay_ms > 0 {
            sleep(Duration::from_millis(delay_ms)).await;
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
    adapters: &[ProviderAdapterEnum],
    upstream_client: &Arc<UpstreamClient>,
    oauth_registry: &Arc<OAuthProviderRegistry>,
) -> crate::error::Result<Option<AccountQuota>> {
    let (provider_id_str, api_key, access_token, provider_specific) = {
        let w = db_pool.writer();
        let acc = admin::account_for_quota_refresh(&w, account_id)?;

        let supports_quota = adapters
            .iter()
            .find(|a| a.id().as_str() == acc.provider_id.as_str())
            .map(|a| a.metadata().quota_refresh_supported)
            .unwrap_or(false);

        if !supports_quota {
            return Ok(None);
        }

        let provider_str = acc.provider_id.to_string();
        let is_oauth = acc.auth_type == "oauth";
        let provider_specific = acc.oauth_provider_specific.clone();

        let (k, token) = if is_oauth {
            let t = accounts::decrypt_access_token(&w, account_id, master_key)?;
            (String::new(), Some(t))
        } else {
            let k = admin::decrypt_api_key_for_account(&w, account_id, master_key)?;
            (k, None)
        };
        (provider_str, k, token, provider_specific)
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
            let w = db_pool.writer();
            accounts::decrypt_refresh_token(&w, account_id, master_key.as_ref())
                .ok()
                .flatten()
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
                        let w = db_pool.writer();
                        let _ = accounts::store_oauth_tokens(
                            &w,
                            account_id,
                            &new_tokens.access_token,
                            new_tokens.refresh_token.as_deref(),
                            master_key,
                            &new_tokens.token_type,
                            expires_at.as_deref(),
                            new_tokens.scope.as_deref(),
                            None,
                            None,
                        );
                    }
                    // Retry the quota call with the new access token.
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
                        error = %e,
                        account_id = account_id.0,
                        "on-demand token refresh failed"
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
        let w = db_pool.writer();
        admin::persist_account_quota(&w, account_id, &q)?;
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
            let w = db_pool.writer();
            let _ = notifications::insert_and_broadcast(
                &w,
                notifications::KIND_SYSTEM,
                &payload,
                Some(&dedup_key),
                Some(&provider_id_str),
            );
        }
    }

    Ok(Some(q))
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
