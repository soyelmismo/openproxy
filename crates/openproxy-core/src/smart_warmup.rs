//! Background daemon for Smart Warmup.
//!
//! Scans all active Antigravity accounts on a timer. If an account's
//! quota is 100% full, it sends a dummy `generateContent` ping to
//! configured models to prevent Google from putting them to sleep.
//! A 4-hour cooldown prevents pinging the model repeatedly.

use crate::accounts;
use crate::config::AppConfig;
use crate::db::DbPool;
use crate::ids::ProviderId;
use crate::quota::fetch_antigravity_quota;
use crate::secrets::MasterKey;
use crate::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest,
};
use dashmap::DashMap;
use once_cell::sync::Lazy;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

/// History of when an account+model was last warmed up.
/// Key: "account_id:model_name", Value: Unix timestamp (seconds).
static WARMUP_HISTORY: Lazy<DashMap<String, i64>> = Lazy::new(DashMap::new);

/// 4-hour cooldown (since Pro quota resets every 5h).
const COOLDOWN_SECS: i64 = 14_400;

pub async fn start_smart_warmup_scheduler(
    db_pool: Arc<DbPool>,
    config: AppConfig,
    upstream: Arc<UpstreamClient>,
    master_key: Arc<MasterKey>,
) {
    if !config.smart_warmup.enabled {
        tracing::debug!("Smart warmup is disabled in config; not starting scheduler");
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

    tokio::spawn(async move {
        loop {
            run_warmup_cycle(&db_pool, &config, &upstream, &master_key).await;
            sleep(Duration::from_secs(interval)).await;
        }
    });
}

async fn run_warmup_cycle(db_pool: &Arc<DbPool>, config: &AppConfig, upstream: &Arc<UpstreamClient>, master_key: &Arc<MasterKey>) {
    // Extract necessary data so we can drop the DB lock before the network call
    let account_list: Vec<(i64, String, String)> = {
        let conn = db_pool.writer();

        let provider_id = ProviderId::new("antigravity");
        let accounts = match accounts::list(&conn, Some(&provider_id)) {
            Ok(accs) => accs,
            Err(e) => {
                tracing::warn!("[SmartWarmup] Failed to list accounts: {}", e);
                return;
            }
        };

        accounts
            .into_iter()
            .filter(|a| !matches!(a.health_status, crate::accounts::HealthStatus::Unhealthy))
            .filter_map(|a| {
                accounts::decrypt_access_token(&conn, a.id, master_key)
                    .ok()
                    .map(|token| (a.id.0, a.id.0.to_string(), token))
            })
            .collect()
    };

    let now = chrono::Utc::now().timestamp();
    let models_to_ping = &config.smart_warmup.models;

    for (account_id_i64, account_id_str, access_token) in account_list {

        // Fetch fresh quota
        let quota = match fetch_antigravity_quota(upstream, &access_token).await {
            Ok(q) => q,
            Err(e) => {
                tracing::debug!(
                    "[SmartWarmup] Failed to fetch quota for account {}: {}",
                    account_id_str,
                    e
                );
                continue;
            }
        };

        // Persist the fresh quota so the UI / frontend sees it
        {
            let conn = db_pool.writer();
            let _ = crate::accounts::set_quota(&conn, crate::ids::AccountId(account_id_i64), &quota);
        }

        // Check if 100% capacity
        let is_100_percent = matches!(
            (quota.session_used, quota.session_limit),
            (Some(0), Some(limit)) if limit > 0
        );

        if !is_100_percent {
            continue;
        }

        for model in models_to_ping {
            let history_key = format!("{}:{}", account_id_str, model);

            // Check cooldown
            if let Some(last_ts) = WARMUP_HISTORY.get(&history_key) 
                && now - *last_ts < COOLDOWN_SECS 
            {
                continue; // Skip, still in cooldown
            }

            tracing::info!(
                "[SmartWarmup] 🔥 Triggering dummy ping for {} on account {}",
                model,
                account_id_str
            );

            let success =
                ping_antigravity_model(upstream, &access_token, model, &account_id_str).await;

            if success {
                WARMUP_HISTORY.insert(history_key, now);
            }
            
            // Pequeña pausa entre modelos para no acribillar la API
            tokio::time::sleep(Duration::from_millis(6000)).await;
        }

        // Pausa entre cuentas para ser sigilosos (anti-DDoS/bot detection)
        tokio::time::sleep(Duration::from_secs(15)).await;
    }

    // Cleanup history older than 24h to prevent memory leak
    let cutoff = now - 86_400;
    WARMUP_HISTORY.retain(|_, v| *v > cutoff);
}

async fn ping_antigravity_model(
    upstream: &Arc<UpstreamClient>,
    access_token: &str,
    model: &str,
    account_id: &str,
) -> bool {
    let endpoint = "https://cloudcode-pa.googleapis.com/v1internal:generateContent";

    let payload = serde_json::json!({
        "model": model,
        "contents": [{"role": "user", "parts": [{"text": "Say hi"}]}],
        "generationConfig": {
            "maxOutputTokens": 10,
            "temperature": 0.0
        }
    });

    let payload_bytes = match serde_json::to_vec(&payload) {
        Ok(b) => b,
        Err(_) => return false,
    };

    let mut req = UpstreamRequest::post_json(endpoint, bytes::Bytes::from(payload_bytes));
    if let Ok(v) = http::HeaderValue::from_str(&format!("Bearer {}", access_token)) {
        req.headers.insert(http::header::AUTHORIZATION, v);
    }

    crate::antigravity_headers::inject_antigravity_headers(&mut req.headers, Some(account_id));

    let cancel = CancellationToken::new();
    match upstream.call(req, TimeoutProfile::Quota, cancel).await {
        Ok(resp) => {
            if !resp.status.is_success() {
                tracing::warn!(
                    "[SmartWarmup] Ping failed for {} on {}: HTTP {}",
                    model,
                    account_id,
                    resp.status
                );
                false
            } else {
                true
            }
        }
        Err(e) => {
            tracing::warn!(
                "[SmartWarmup] Network error pinging {} on {}: {}",
                model,
                account_id,
                e
            );
            false
        }
    }
}
