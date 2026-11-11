//! Background quota synchronization: periodically refreshes quota for every account
//! of a provider that supports it, and hosts the single-account refresh shared with
//! the manual UI endpoint.

use crate::AppConfig;
use crate::accounts;
use crate::ids::AccountId;
use crate::oauth::OAuthProviderRegistry;
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

    let cancel = cancel_token.unwrap_or_default();
    let token = cancel.clone();

    tokio::spawn(async move {
        Box::pin(run_quota_sync_scheduler(
            db_pool,
            config,
            upstream_client,
            master_key,
            adapters,
            oauth_provider_registry,
            token,
        ))
        .await;
    });

    Some(cancel)
}

pub async fn run_quota_sync_scheduler(
    db_pool: Arc<DbPool>,
    config: AppConfig,
    upstream_client: Arc<UpstreamClient>,
    master_key: Arc<MasterKey>,
    adapters: Arc<RwLock<Arc<Vec<ProviderAdapterEnum>>>>,
    oauth_provider_registry: Arc<OAuthProviderRegistry>,
    cancel: CancellationToken,
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

    // initial delay: do not hit DB/network during the boot stampede
    tokio::select! {
        () = cancel.cancelled() => {
            tracing::info!("[QuotaSync] Scheduler cancelled during initial delay");
            return;
        }
        () = sleep(Duration::from_secs(30)) => {}
    }

    loop {
        if cancel.is_cancelled() {
            tracing::info!("[QuotaSync] Scheduler shutting down");
            break;
        }

        Box::pin(run_quota_sync_cycle(
            &db_pool,
            &config,
            &upstream_client,
            &master_key,
            &adapters,
            &oauth_provider_registry,
            Some(&cancel),
        ))
        .await;

        tokio::select! {
            () = cancel.cancelled() => {
                tracing::info!("[QuotaSync] Scheduler shutting down");
                break;
            }
            () = sleep(Duration::from_secs(interval)) => {}
        }
    }
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
                    let is_local_cli = matches!(
                        acc.provider_id.as_str(),
                        "claude-code" | "claude" | "antigravity"
                    );
                    if acc.health_status != accounts::HealthStatus::Unhealthy || is_local_cli {
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

        if let Err(e) = Box::pin(refresh_single_account_quota(
            account_id,
            db_pool,
            master_key,
            &supported_refs,
            upstream_client,
            oauth_registry,
        ))
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

pub(crate) mod checkin;
pub(crate) mod refresh;

pub use refresh::refresh_single_account_quota;

#[cfg(test)]
pub(crate) use checkin::persist_autoclaw_checkin;
#[cfg(test)]
pub(crate) use refresh::{clear_live_limited_after_refresh, quota_pool_health};

#[cfg(test)]
#[path = "quota_sync_pool_tests.rs"]
mod pool_tests;

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
mod tests;
