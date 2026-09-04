//! Background daemon for Smart Warmup.
//!
//! Scans all active Antigravity accounts on a timer. If an account's
//! quota is 100% full, it sends a tiny request through the same
//! Antigravity executor used by normal API traffic.
//! A 4-hour cooldown prevents pinging the model repeatedly.

use crate::accounts;
use crate::config::AppConfig;
use crate::ids::ProviderId;
use openproxy_adapters::upstream::UpstreamClient;
use openproxy_db::DbPool;
use openproxy_db::secrets::MasterKey;
use openproxy_types::{OpenAIMessage, OpenAIRequest};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::sleep;

/// 4-hour cooldown (since Pro quota resets every 5h).
const COOLDOWN_SECS: i64 = 14_400;

fn build_warmup_request(model: &str) -> OpenAIRequest {
    OpenAIRequest {
        model: model.to_string(),
        messages: vec![OpenAIMessage {
            role: "user".to_string(),
            content: Some(serde_json::Value::String("Say hi".to_string())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::new(),
        }],
        max_tokens: None,
        temperature: Some(0.0),
        stream: false,
        top_p: None,
        stop: None,
        tools: None,
        tool_choice: None,
        top_k: None,
        user: None,
        extra: serde_json::Map::new(),
    }
}

pub fn start_smart_warmup_scheduler(
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

async fn run_warmup_cycle(
    db_pool: &Arc<DbPool>,
    config: &AppConfig,
    upstream: &Arc<UpstreamClient>,
    master_key: &Arc<MasterKey>,
) {
    // Extract necessary data so we can drop the DB lock before the network call
    let account_list: Vec<(i64, String, String, String)> = {
        let db_pool = Arc::clone(db_pool);
        let master_key = Arc::clone(master_key);
        tokio::task::spawn_blocking(move || {
            let conn = db_pool.writer();

            let provider_id = ProviderId::new("antigravity");
            let accounts = match accounts::list(&conn, Some(&provider_id), &master_key) {
                Ok(accs) => accs,
                Err(e) => {
                    tracing::warn!("[SmartWarmup] Failed to list accounts: {}", e);
                    return Vec::new();
                }
            };

            accounts
                .into_iter()
                .filter(|a| !matches!(a.health_status, crate::accounts::HealthStatus::Unhealthy))
                .filter_map(|a| {
                    let acc_id = a.id.0;
                    let token = accounts::decrypt_access_token(&conn, a.id, &master_key).ok()?;
                    let meta = a.oauth_provider_specific?;
                    let v: serde_json::Value = serde_json::from_str(&meta).ok()?;
                    let project_id =
                        openproxy_pipeline::credentials::antigravity_project_from_value(&v)?;
                    let account_id_str = acc_id.to_string();
                    Some((acc_id, token, project_id, account_id_str))
                })
                .collect()
        })
        .await
        .unwrap_or_default()
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let models_to_ping = &config.smart_warmup.models;

    for (account_id_i64, access_token, project_id, account_id_str) in account_list {
        // Fetch fresh quota
        let quota = match fetch_antigravity_quota(upstream, &access_token, &project_id).await {
            Some(Ok(q)) => q,
            Some(Err(e)) => {
                tracing::debug!(
                    "[SmartWarmup] Failed to fetch quota for account {}: {}",
                    account_id_str,
                    e
                );
                continue;
            }
            None => continue,
        };

        // Persist the fresh quota so the UI / frontend sees it
        {
            let db_pool = Arc::clone(db_pool);
            let quota = quota.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let conn = db_pool.writer();
                let _ = crate::accounts::set_quota(
                    &conn,
                    crate::ids::AccountId(account_id_i64),
                    &quota,
                );
            })
            .await;
        }

        // Check if 100% capacity
        let is_100_percent = matches!(
            (quota.session_used, quota.session_limit),
            (Some(0), Some(limit)) if limit > 0
        );

        if !is_100_percent {
            continue;
        }

        for model_alias in models_to_ping {
            let true_model_id = {
                let db_pool = Arc::clone(db_pool);
                let alias = model_alias.to_owned();
                tokio::task::spawn_blocking(move || {
                    let conn = db_pool.reader();
                    resolve_model_alias(&conn, &alias)
                })
                .await
                .unwrap_or(None)
            };

            let Some(true_model_id) = true_model_id else {
                continue;
            };

            let history_key = format!("{account_id_str}:{true_model_id}");

            // Check cooldown
            let last_ts = {
                let db_pool = Arc::clone(db_pool);
                let history_key_check = history_key.clone();
                tokio::task::spawn_blocking(move || {
                    let conn = db_pool.reader();
                    conn.query_row(
                        "SELECT last_ts FROM smart_warmup_history WHERE history_key = ?1",
                        rusqlite::params![history_key_check],
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
                "[SmartWarmup] 🔥 Triggering dummy ping for {} (alias: {}) on account {}",
                true_model_id,
                model_alias,
                account_id_str
            );

            let success = ping_antigravity_model(
                upstream,
                &access_token,
                &project_id,
                &true_model_id,
                &account_id_str,
            )
            .await;

            if success {
                let db_pool = Arc::clone(db_pool);
                let history_key = history_key.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    let conn = db_pool.writer();
                    let _ = conn.execute(
                        "INSERT INTO smart_warmup_history (history_key, last_ts) VALUES (?1, ?2) \
                         ON CONFLICT(history_key) DO UPDATE SET last_ts = excluded.last_ts",
                        rusqlite::params![history_key, now],
                    );
                })
                .await;
            }

            // Pequeña pausa entre modelos para no acribillar la API
            tokio::time::sleep(Duration::from_secs(6)).await;
        }

        // Pausa entre cuentas para ser sigilosos (anti-DDoS/bot detection)
        tokio::time::sleep(Duration::from_secs(15)).await;
    }

    // Cleanup history older than 24h to prevent table growth
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

async fn fetch_antigravity_quota(
    upstream: &Arc<UpstreamClient>,
    access_token: &str,
    project_id: &str,
) -> Option<crate::error::Result<openproxy_types::AccountQuota>> {
    let adapter = openproxy_adapters::adapters::ProviderAdapterEnum::Antigravity(Box::new(
        openproxy_adapters::adapters::AntigravityAdapter::new(),
    ));
    adapter
        .fetch_quota(upstream, project_id, Some(access_token), None)
        .await
}

async fn ping_antigravity_model(
    upstream: &Arc<UpstreamClient>,
    access_token: &str,
    project_id: &str,
    model: &str,
    account_id: &str,
) -> bool {
    let request = build_warmup_request(model);
    let request_payload = serde_json::to_value(
        openproxy_adapters::adapters::gemini::openai_to_gemini(&request, &request.messages),
    )
    .unwrap_or_else(|_| serde_json::json!({}));

    let wrapped = serde_json::json!({
        "project": project_id,
        "model": model,
        "requestType": "agent",
        "requestId": uuid::Uuid::new_v4().to_string(),
        "userAgent": "antigravity",
        "request": request_payload,
        "enabledCreditTypes": ["GOOGLE_ONE_AI"]
    });

    let payload = match serde_json::to_vec(&wrapped) {
        Ok(b) => bytes::Bytes::from(b),
        Err(_) => return false,
    };

    let url = format!(
        "{}/v1internal:generateContent",
        openproxy_adapters::adapters::antigravity::DEFAULT_ANTIGRAVITY_BASE_URL
    );

    let mut req = openproxy_adapters::upstream::UpstreamRequest::post_json(url, payload);
    if let Ok(v) = http::HeaderValue::from_str(&format!("Bearer {access_token}")) {
        req.headers.insert(http::header::AUTHORIZATION, v);
    }
    openproxy_adapters::antigravity_headers::inject_antigravity_headers(&mut req.headers, None);

    let cancel = openproxy_adapters::upstream::CancellationToken::new();
    match upstream
        .call(
            req,
            openproxy_adapters::upstream::TimeoutProfile::Chat,
            cancel,
        )
        .await
    {
        Ok(resp) => {
            let status = resp.status;
            let _ = resp.collect().await;
            if status.is_success() {
                true
            } else {
                tracing::warn!(
                    "[SmartWarmup] Ping failed with status {} for {} on {}",
                    status,
                    model,
                    account_id
                );
                false
            }
        }
        Err(e) => {
            tracing::warn!(
                "[SmartWarmup] Ping request failed for {} on {}: {}",
                model,
                account_id,
                e
            );
            false
        }
    }
}

/// Helper: maps a config string (like "gpt-oss-120b-medium") into the true provider model_id
/// (like "gemini-3.1-pro-low") by resolving it against the `combos` and `models` tables.
/// If it can't find a combo or model, it assumes the string itself is the target.
fn resolve_model_alias(conn: &rusqlite::Connection, alias: &str) -> Option<String> {
    use crate::ids::ProviderId;

    // Try to lookup as a combo
    if let Ok(Some(combo)) = openproxy_db::combos::get_combo_by_name(conn, alias) {
        let mut visited = Vec::new();
        if let Ok(targets) = openproxy_pipeline::repository::resolve_combo_to_targets(
            conn,
            combo.id,
            &mut visited,
            0,
        ) {
            for target in targets {
                if let Some(row_id) = target.model_row_id
                    && let Ok(Some(model)) = crate::models::get_by_row_id(conn, row_id)
                    && model.provider_id.as_str() == "antigravity"
                {
                    return Some(model.model_id.0);
                }
            }
        }
    }

    // Try to lookup as an exact model name for antigravity
    if let Ok(Some(model)) = crate::models::find_active_by_provider_and_name(
        conn,
        &ProviderId::new("antigravity"),
        alias,
    ) {
        return Some(model.model_id.0);
    }

    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn warmup_request_is_minimal_openai_shape() {
        let request = super::build_warmup_request("gemini-2.5-flash-lite");

        assert_eq!(request.model, "gemini-2.5-flash-lite");
        assert_eq!(request.messages.len(), 1);
        assert_eq!(request.messages[0].role, "user");
        assert_eq!(
            request.messages[0]
                .content
                .as_ref()
                .and_then(|v| v.as_str()),
            Some("Say hi")
        );
        assert!(!request.stream);
        assert_eq!(request.temperature, Some(0.0));
    }

    /// The warmup closure uses `openproxy_pipeline::credentials::
    /// antigravity_project_from_value`, which (post-C.4 wire-format
    /// unification) reads snake_case `project_id`. The migration
    /// `000065_antigravity_project_id_wire_format.sql` normalizes any
    /// pre-existing camelCase rows at startup, so the closure does not
    /// need to handle camelCase directly. This test pins the contract.
    #[test]
    fn smart_warmup_extract_reads_snake_case_canonical() {
        use serde_json::json;
        let v = json!({"project_id":"canonical"});
        assert_eq!(
            openproxy_pipeline::credentials::antigravity_project_from_value(&v),
            Some("canonical".to_string())
        );
    }
}
