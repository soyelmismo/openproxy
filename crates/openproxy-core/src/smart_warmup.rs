//! Background daemon for Smart Warmup across providers (Antigravity, Codex, Claude Code).
//!
//! Periodically scans accounts whose quota is 100% full and sends a minimal dummy
//! request to kickstart the sliding reset window early. A 4-hour cooldown prevents
//! repeated pings.

pub mod antigravity;
pub mod claude_code;
pub mod codex;
pub mod traits;

#[cfg(test)]
mod tests;

pub use antigravity::{AntigravityWarmupStrategy, build_warmup_request, resolve_antigravity_model};
pub use claude_code::ClaudeCodeWarmupStrategy;
pub use codex::CodexWarmupStrategy;
pub use traits::{WarmupAccountContext, WarmupStrategy, WarmupStrategyEnum};

use crate::accounts;
use crate::config::AppConfig;
use crate::ids::ProviderId;
use openproxy_adapters::upstream::UpstreamClient;
use openproxy_db::DbPool;
use openproxy_db::secrets::MasterKey;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

/// 4-hour cooldown (since typical Pro/Team quotas reset every 3-5h).
pub const COOLDOWN_SECS: i64 = 14_400;

/// Default list of all built-in warmup strategies.
pub fn default_strategies() -> Vec<WarmupStrategyEnum> {
    vec![
        WarmupStrategyEnum::Antigravity(AntigravityWarmupStrategy::new()),
        WarmupStrategyEnum::Codex(CodexWarmupStrategy::new()),
        WarmupStrategyEnum::ClaudeCode(ClaudeCodeWarmupStrategy::new()),
    ]
}

pub fn start_smart_warmup_scheduler(
    db_pool: Arc<DbPool>,
    config: AppConfig,
    upstream: Arc<UpstreamClient>,
    master_key: Arc<MasterKey>,
) {
    let _ = start_smart_warmup_scheduler_with_cancel(db_pool, config, upstream, master_key, None);
}

pub fn start_smart_warmup_scheduler_with_cancel(
    db_pool: Arc<DbPool>,
    config: AppConfig,
    upstream: Arc<UpstreamClient>,
    master_key: Arc<MasterKey>,
    cancel_token: Option<CancellationToken>,
) -> Option<CancellationToken> {
    if !config.smart_warmup.enabled {
        tracing::debug!("Smart warmup is disabled in config; not starting scheduler");
        return None;
    }

    let interval = config.smart_warmup.interval_secs;
    if interval == 0 {
        return None;
    }

    let cancel = cancel_token.unwrap_or_default();
    let token = cancel.clone();

    tokio::spawn(async move {
        run_smart_warmup_scheduler(db_pool, config, upstream, master_key, token).await;
    });

    Some(cancel)
}

pub async fn run_smart_warmup_scheduler(
    db_pool: Arc<DbPool>,
    config: AppConfig,
    upstream: Arc<UpstreamClient>,
    master_key: Arc<MasterKey>,
    cancel: CancellationToken,
) {
    if !config.smart_warmup.enabled {
        tracing::debug!("Smart warmup is disabled in config; not running scheduler");
        return;
    }

    let interval = config.smart_warmup.interval_secs;
    if interval == 0 {
        return;
    }

    tracing::info!(
        "[SmartWarmup] Scheduler started. Scanning every {}s for {} models",
        interval,
        config.smart_warmup.models.len()
    );

    loop {
        if cancel.is_cancelled() {
            tracing::info!("[SmartWarmup] Scheduler shutting down");
            break;
        }

        run_warmup_cycle_with_cancel(&db_pool, &config, &upstream, &master_key, Some(&cancel))
            .await;

        tokio::select! {
            () = cancel.cancelled() => {
                tracing::info!("[SmartWarmup] Scheduler shutting down");
                break;
            }
            () = sleep(Duration::from_secs(interval)) => {}
        }
    }
}

pub async fn run_warmup_cycle_with_cancel(
    db_pool: &Arc<DbPool>,
    config: &AppConfig,
    upstream: &Arc<UpstreamClient>,
    master_key: &Arc<MasterKey>,
    cancel: Option<&CancellationToken>,
) {
    let strategies = default_strategies();
    run_warmup_cycle_with_strategies(db_pool, config, upstream, master_key, &strategies, cancel)
        .await;
}

pub async fn run_warmup_cycle_with_strategies(
    db_pool: &Arc<DbPool>,
    config: &AppConfig,
    upstream: &Arc<UpstreamClient>,
    master_key: &Arc<MasterKey>,
    strategies: &[WarmupStrategyEnum],
    cancel: Option<&CancellationToken>,
) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    for strategy in strategies {
        if cancel.is_some_and(|c| c.is_cancelled()) {
            break;
        }

        let provider_name = strategy.provider_id().to_string();
        let provider_id = ProviderId::new(&provider_name);

        // Read candidate accounts for this provider inside spawn_blocking
        let accounts_list: Vec<WarmupAccountContext> = {
            let db_pool = Arc::clone(db_pool);
            let master_key = Arc::clone(master_key);
            let p_id = provider_id.clone();
            let strategy_clone = strategy.clone();
            tokio::task::spawn_blocking(move || {
                let conn = db_pool.writer();
                let accounts = match accounts::list(&conn, Some(&p_id), &master_key) {
                    Ok(accs) => accs,
                    Err(e) => {
                        tracing::warn!(
                            provider = %p_id,
                            error = %e,
                            "[SmartWarmup] Failed to list accounts: {e}"
                        );
                        return Vec::new();
                    }
                };

                accounts
                    .into_iter()
                    .filter(|a| {
                        !matches!(a.health_status, crate::accounts::HealthStatus::Unhealthy)
                    })
                    .filter_map(|a| strategy_clone.extract_account(&a, &conn, &master_key))
                    .collect()
            })
            .await
            .unwrap_or_default()
        };

        for acc in accounts_list {
            if cancel.is_some_and(|c| c.is_cancelled()) {
                break;
            }

            // 1. Fetch fresh quota
            let quota = match strategy.fetch_quota(upstream, &acc).await {
                Some(Ok(q)) => q,
                Some(Err(e)) => {
                    tracing::debug!(
                        provider = %provider_name,
                        account_id = acc.account_id,
                        error = %e,
                        "[SmartWarmup] Failed to fetch quota for account {}: {e}",
                        acc.account_desc
                    );
                    continue;
                }
                None => continue,
            };

            // 2. Persist fresh quota so the UI / dashboard sees it
            {
                let db_pool = Arc::clone(db_pool);
                let q_persist = quota.clone();
                let acc_id = acc.account_id;
                let _ = tokio::task::spawn_blocking(move || {
                    let conn = db_pool.writer();
                    let _ = crate::accounts::set_quota(
                        &conn,
                        crate::ids::AccountId(acc_id),
                        &q_persist,
                    );
                })
                .await;
            }

            // 3. Resolve target models for this strategy
            let models_to_ping = {
                let db_pool = Arc::clone(db_pool);
                let config_models = config.smart_warmup.models.to_vec();
                let strategy_clone = strategy.clone();
                tokio::task::spawn_blocking(move || {
                    let conn = db_pool.reader();
                    strategy_clone.resolve_models(&conn, &config_models)
                })
                .await
                .unwrap_or_default()
            };

            for true_model_id in models_to_ping {
                if cancel.is_some_and(|c| c.is_cancelled()) {
                    break;
                }

                // 4. Check if quota is ready for warmup
                if !strategy.is_quota_ready(&quota, &true_model_id, now) {
                    tracing::debug!(
                        provider = %provider_name,
                        account_id = acc.account_id,
                        model = %true_model_id,
                        "[SmartWarmup] Skipping model '{true_model_id}' on account {}: window already ticking or quota not full",
                        acc.account_desc
                    );
                    continue;
                }

                let history_key = format!("{}:{provider_name}:{true_model_id}", acc.account_id);
                let legacy_key = format!("{}:{true_model_id}", acc.account_id);

                // 5. Check cooldown from DB history
                let last_ts = {
                    let db_pool = Arc::clone(db_pool);
                    let k1 = history_key.clone();
                    let k2 = legacy_key.clone();
                    tokio::task::spawn_blocking(move || {
                        let conn = db_pool.reader();
                        conn.query_row(
                            "SELECT last_ts FROM smart_warmup_history WHERE history_key IN (?1, ?2) ORDER BY last_ts DESC LIMIT 1",
                            rusqlite::params![k1, k2],
                            |r| r.get::<_, i64>(0),
                        )
                        .ok()
                    })
                    .await
                    .unwrap_or(None)
                };

                if let Some(ts) = last_ts
                    && now - ts < COOLDOWN_SECS
                {
                    continue; // Skip, still in cooldown
                }

                tracing::info!(
                    provider = %provider_name,
                    account_id = acc.account_id,
                    model = %true_model_id,
                    "[SmartWarmup] 🔥 Triggering dummy ping for model '{true_model_id}' on account {} (provider: '{provider_name}')",
                    acc.account_desc
                );

                // 6. Execute dummy ping
                let success = strategy.ping_model(upstream, &acc, &true_model_id).await;

                if success {
                    {
                        let db_pool = Arc::clone(db_pool);
                        let k_save = history_key.clone();
                        let _ = tokio::task::spawn_blocking(move || {
                            let conn = db_pool.writer();
                            let _ = conn.execute(
                                "INSERT INTO smart_warmup_history (history_key, last_ts) VALUES (?1, ?2) \
                                 ON CONFLICT(history_key) DO UPDATE SET last_ts = excluded.last_ts",
                                rusqlite::params![k_save, now],
                            );
                        })
                        .await;
                    }

                    // Refresh and persist quota immediately so UI shows the newly ticking timer
                    if let Some(Ok(fresh_quota)) = strategy.fetch_quota(upstream, &acc).await {
                        let db_pool = Arc::clone(db_pool);
                        let acc_id = acc.account_id;
                        let _ = tokio::task::spawn_blocking(move || {
                            let conn = db_pool.writer();
                            let _ = crate::accounts::set_quota(
                                &conn,
                                crate::ids::AccountId(acc_id),
                                &fresh_quota,
                            );
                        })
                        .await;
                    }
                }

                // Pause between models
                if let Some(token) = cancel {
                    tokio::select! {
                        () = token.cancelled() => break,
                        () = sleep(Duration::from_secs(6)) => {}
                    }
                } else {
                    sleep(Duration::from_secs(6)).await;
                }
            }

            // Pause between accounts
            if let Some(token) = cancel {
                tokio::select! {
                    () = token.cancelled() => break,
                    () = sleep(Duration::from_secs(15)) => {}
                }
            } else {
                sleep(Duration::from_secs(15)).await;
            }
        }
    }

    if cancel.is_some_and(|c| c.is_cancelled()) {
        return;
    }

    // Prune entries older than 24h
    let cutoff = now - 86_400;
    {
        let db_pool = Arc::clone(db_pool);
        let _ = tokio::task::spawn_blocking(move || {
            let conn = db_pool.writer();
            let _ = conn.execute(
                "DELETE FROM smart_warmup_history WHERE last_ts <= ?1",
                rusqlite::params![cutoff],
            );
        })
        .await;
    }
}

/// Backwards compatibility helper for Antigravity target resolution.
pub fn resolve_warmup_target(conn: &rusqlite::Connection, alias: &str) -> Option<String> {
    antigravity::resolve_antigravity_model(conn, alias)
}

/// Backwards compatibility helper for tests.
pub fn is_model_quota_ready_for_warmup(
    quota: &openproxy_types::AccountQuota,
    true_model_id: &str,
    now: i64,
) -> bool {
    AntigravityWarmupStrategy::new().is_quota_ready(quota, true_model_id, now)
}
