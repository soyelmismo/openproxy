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
    let lower = model.to_lowercase();
    let is_gemini = lower.contains("gemini");

    let (prompt, max_tokens, temperature) = if is_gemini {
        (
            "Write a Python function to check if a number is prime and explain how it works with a brief example.",
            Some(256),
            Some(0.2),
        )
    } else {
        ("Say hi", None, Some(0.0))
    };

    OpenAIRequest {
        model: model.to_string(),
        messages: vec![OpenAIMessage {
            role: "user".to_string(),
            content: Some(serde_json::Value::String(prompt.to_string())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::new(),
        }],
        max_tokens,
        temperature,
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
    struct WarmupAccount {
        id: i64,
        token: String,
        project_id: String,
        account_desc: String,
    }

    // Read the account list inside spawn_blocking so no DB guard is held across the
    // network calls below.
    let account_list: Vec<WarmupAccount> = {
        let db_pool = Arc::clone(db_pool);
        let master_key = Arc::clone(master_key);
        tokio::task::spawn_blocking(move || {
            let conn = db_pool.writer();

            let provider_id = ProviderId::new("antigravity");
            let accounts = match accounts::list(&conn, Some(&provider_id), &master_key) {
                Ok(accs) => accs,
                Err(e) => {
                    tracing::warn!(
                        provider = "antigravity",
                        error = %e,
                        "[SmartWarmup] Failed to list accounts for provider 'antigravity': {}",
                        e
                    );
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
                    let acc_id_str = acc_id.to_string();
                    let label = a
                        .label
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty() && *s != acc_id_str);
                    let email = a.email.as_deref().map(str::trim).filter(|s| !s.is_empty());
                    let account_desc = match (label, email) {
                        (Some(l), Some(e)) => format!("{acc_id} ({l} / {e})"),
                        (Some(l), None) => format!("{acc_id} ({l})"),
                        (None, Some(e)) => format!("{acc_id} ({e})"),
                        (None, None) => acc_id_str,
                    };
                    Some(WarmupAccount {
                        id: acc_id,
                        token,
                        project_id,
                        account_desc,
                    })
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

    for acc in account_list {
        // Fetch fresh quota
        let quota = match fetch_antigravity_quota(upstream, &acc.token, &acc.project_id).await {
            Some(Ok(q)) => q,
            Some(Err(e)) => {
                tracing::debug!(
                    provider = "antigravity",
                    account_id = acc.id,
                    error = %e,
                    "[SmartWarmup] Failed to fetch quota for account {} (provider: 'antigravity'): {}",
                    acc.account_desc,
                    e
                );
                continue;
            }
            None => continue,
        };

        // Persist the fresh quota so the UI sees it
        {
            let db_pool = Arc::clone(db_pool);
            let quota = quota.clone();
            let acc_id = acc.id;
            let _ = tokio::task::spawn_blocking(move || {
                let conn = db_pool.writer();
                let _ = crate::accounts::set_quota(&conn, crate::ids::AccountId(acc_id), &quota);
            })
            .await;
        }

        for model_alias in models_to_ping {
            let true_model_id = {
                let db_pool = Arc::clone(db_pool);
                let alias = model_alias.to_owned();
                tokio::task::spawn_blocking(move || {
                    let conn = db_pool.reader();
                    resolve_warmup_target(&conn, &alias)
                })
                .await
                .unwrap_or(None)
            };

            let Some(true_model_id) = true_model_id else {
                continue;
            };

            if !is_model_quota_ready_for_warmup(&quota, &true_model_id, now) {
                tracing::debug!(
                    provider = "antigravity",
                    account_id = acc.id,
                    model = %true_model_id,
                    alias = %model_alias,
                    "[SmartWarmup] Skipping model '{}' on account {}: quota is not full or window is already ticking",
                    true_model_id,
                    acc.account_desc
                );
                continue;
            }

            let history_key = format!("{}:{true_model_id}", acc.id);

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
                provider = "antigravity",
                account_id = acc.id,
                model = %true_model_id,
                alias = %model_alias,
                "[SmartWarmup] 🔥 Triggering dummy ping for model '{}' (alias: '{}') on account {} (provider: 'antigravity')",
                true_model_id,
                model_alias,
                acc.account_desc
            );

            let success = ping_antigravity_model(
                upstream,
                &acc.token,
                &acc.project_id,
                &true_model_id,
                acc.id,
                &acc.account_desc,
            )
            .await;

            if success {
                {
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

                // Refresh and persist quota immediately so UI shows updated quota / ticking reset timer
                if let Some(Ok(fresh_quota)) =
                    fetch_antigravity_quota(upstream, &acc.token, &acc.project_id).await
                {
                    let db_pool = Arc::clone(db_pool);
                    let acc_id = acc.id;
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

            // Pausa entre modelos para no acribillar la API
            tokio::time::sleep(Duration::from_secs(6)).await;
        }

        // Pausa entre cuentas: evita detección anti-DDoS/bot
        tokio::time::sleep(Duration::from_secs(15)).await;
    }

    // Limpia historial de más de 24h para acotar el crecimiento de la tabla
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
    account_id: i64,
    account_desc: &str,
) -> bool {
    let physical_model =
        openproxy_adapters::adapters::antigravity::map_antigravity_physical_model(model);
    let request = build_warmup_request(model);
    let request_payload = serde_json::to_value(
        openproxy_adapters::adapters::gemini::openai_to_gemini(&request, &request.messages),
    )
    .unwrap_or_else(|_| serde_json::json!({}));

    let wrapped = serde_json::json!({
        "project": project_id,
        "model": physical_model,
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
                    provider = "antigravity",
                    account_id = account_id,
                    model = %model,
                    status = %status,
                    "[SmartWarmup] Ping failed with status {} for model '{}' on account {} (provider: 'antigravity')",
                    status,
                    model,
                    account_desc
                );
                false
            }
        }
        Err(e) => {
            tracing::warn!(
                provider = "antigravity",
                account_id = account_id,
                model = %model,
                error = %e,
                "[SmartWarmup] Ping request failed for model '{}' on account {} (provider: 'antigravity'): {}",
                model,
                account_desc,
                e
            );
            false
        }
    }
}

fn is_model_quota_ready_for_warmup(
    quota: &openproxy_types::AccountQuota,
    true_model_id: &str,
    now: i64,
) -> bool {
    let lower_target = true_model_id.to_lowercase();
    let is_claude = lower_target.contains("claude");
    let is_gemini = lower_target.contains("gemini");

    if is_claude
        && let Some(details) = &quota.model_details
    {
        let weekly_detail = details.iter().find(|d| d.model_id == "Claude (Weekly)");
        let session_detail = details
            .iter()
            .find(|d| d.model_id == "Claude (5h)")
            .or_else(|| details.iter().find(|d| d.model_id == true_model_id));

        // If Claude weekly window has usage and is actively ticking in the future, it's already warmed up
        if let Some(w) = weekly_detail
            && w.session_used > 0
            && let Some(reset_str) = &w.session_reset_at
            && let Ok(dt) = chrono::DateTime::parse_from_rfc3339(reset_str)
            && dt.timestamp() > now
        {
            return false;
        }

        // If Claude 5h session has usage and is actively ticking in the future, skip
        if let Some(s) = session_detail
            && s.session_used > 0
            && let Some(reset_str) = &s.session_reset_at
            && let Ok(dt) = chrono::DateTime::parse_from_rfc3339(reset_str)
            && dt.timestamp() > now
        {
            return false;
        }

        if let Some(detail) = session_detail.or(weekly_detail) {
            return detail.session_used == 0 && detail.remaining_fraction >= 0.999;
        }
    }

    if is_gemini {
        // If Gemini weekly window already has usage and is actively ticking in the future, it's already warmed up
        if let Some(used) = quota.weekly_used
            && used > 0
            && let Some(reset_str) = &quota.weekly_reset_at
            && let Ok(dt) = chrono::DateTime::parse_from_rfc3339(reset_str)
            && dt.timestamp() > now
        {
            return false;
        }

        // If specific model has usage and is ticking in the future, skip
        if let Some(details) = &quota.model_details {
            let matched = details.iter().find(|d| d.model_id == true_model_id);
            if let Some(d) = matched
                && d.session_used > 0
                && let Some(reset_str) = &d.session_reset_at
                && let Ok(dt) = chrono::DateTime::parse_from_rfc3339(reset_str)
                && dt.timestamp() > now
            {
                return false;
            }
        }

        // If account-level 5h session has usage and is ticking in the future, skip
        if let Some(used) = quota.session_used
            && used > 0
            && let Some(reset_str) = &quota.session_reset_at
            && let Ok(dt) = chrono::DateTime::parse_from_rfc3339(reset_str)
            && dt.timestamp() > now
        {
            return false;
        }

        // Gemini is ready if weekly is 0 (or not set) or session is 0
        return quota.weekly_used.unwrap_or(0) == 0 || quota.session_used.unwrap_or(0) == 0;
    }

    // Fallback for any other provider / model:
    if let Some(details) = &quota.model_details
        && let Some(detail) = details.iter().find(|d| d.model_id == true_model_id)
    {
        if let Some(reset_str) = &detail.session_reset_at
            && let Ok(dt) = chrono::DateTime::parse_from_rfc3339(reset_str)
            && dt.timestamp() > now
            && detail.session_used > 0
        {
            return false;
        }
        return detail.session_used == 0 && detail.remaining_fraction >= 0.999;
    }

    if let Some(reset_str) = &quota.session_reset_at
        && let Ok(dt) = chrono::DateTime::parse_from_rfc3339(reset_str)
        && dt.timestamp() > now
        && quota.session_used.unwrap_or(0) > 0
    {
        return false;
    }

    quota.session_used == Some(0)
}

/// Helper: maps a config string or family name (like "gemini-pro", "claude", "claude-sonnet-4-6")
/// into the true active provider model_id (like "gemini-2.5-pro", "claude-sonnet-4-6")
/// dynamically resolving against the combos and models tables.
pub fn resolve_warmup_target(conn: &rusqlite::Connection, alias: &str) -> Option<String> {
    // 1. Try exact lookup as combo or exact active model name
    if let Some(exact) = resolve_model_alias(conn, alias) {
        return Some(exact);
    }

    let lower = alias.to_lowercase();
    let is_claude = lower.contains("claude");
    let is_gemini = lower.contains("gemini");
    let wants_pro = lower.contains("pro");

    if !is_claude && !is_gemini {
        return None;
    }

    // 2. Dynamic resolution against active models for Antigravity, ordered DESC for newest versions
    let mut stmt = conn
        .prepare(
            "SELECT model_id FROM models \
             WHERE provider_id = 'antigravity' AND active = 1 \
             ORDER BY model_id DESC",
        )
        .ok()?;

    let models: Vec<String> = stmt
        .query_map([], |row| row.get(0))
        .ok()?
        .filter_map(|r| r.ok())
        .collect();

    if is_claude {
        // Preferred: newest Claude Sonnet
        if let Some(m) = models
            .iter()
            .find(|m| m.contains("claude") && m.contains("sonnet"))
        {
            return Some(m.clone());
        }
        // Fallback: any newest Claude model (e.g. opus)
        if let Some(m) = models.iter().find(|m| m.contains("claude")) {
            return Some(m.clone());
        }
    }

    if is_gemini {
        if wants_pro {
            // Preferred for pro: flagship Gemini 2.5 Pro or Pro High/Agent that deducts from weekly quota
            if let Some(m) = models
                .iter()
                .find(|m| m.contains("gemini") && m.contains("2.5") && m.contains("pro"))
            {
                return Some(m.clone());
            }
            if let Some(m) = models
                .iter()
                .find(|m| m.contains("gemini") && m.contains("pro") && !m.contains("low"))
            {
                return Some(m.clone());
            }
            // Fallback: any newest pro
            if let Some(m) = models
                .iter()
                .find(|m| m.contains("gemini") && m.contains("pro"))
            {
                return Some(m.clone());
            }
        }
        // Fallback for flash / generic: newest flash-low or flash
        if let Some(m) = models
            .iter()
            .find(|m| m.contains("gemini") && m.contains("flash") && m.contains("low"))
        {
            return Some(m.clone());
        }
        if let Some(m) = models.iter().find(|m| m.contains("gemini")) {
            return Some(m.clone());
        }
    }

    None
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
    fn warmup_request_shapes_for_claude_and_gemini() {
        let claude = super::build_warmup_request("claude-sonnet-4-6");
        assert_eq!(claude.model, "claude-sonnet-4-6");
        assert_eq!(
            claude.messages[0]
                .content
                .as_ref()
                .and_then(|v| v.as_str()),
            Some("Say hi")
        );
        assert_eq!(claude.max_tokens, None);
        assert_eq!(claude.temperature, Some(0.0));

        let gemini = super::build_warmup_request("gemini-2.5-pro");
        assert_eq!(gemini.model, "gemini-2.5-pro");
        let content = gemini.messages[0]
            .content
            .as_ref()
            .and_then(|v| v.as_str())
            .unwrap();
        assert!(content.contains("prime"));
        assert_eq!(gemini.max_tokens, Some(256));
        assert_eq!(gemini.temperature, Some(0.2));
    }

    #[test]
    fn smart_warmup_extract_reads_snake_case_canonical() {
        use serde_json::json;
        let v = json!({"project_id":"canonical"});
        assert_eq!(
            openproxy_pipeline::credentials::antigravity_project_from_value(&v),
            Some("canonical".to_string())
        );
    }

    #[test]
    fn test_is_model_quota_ready_for_warmup() {
        use openproxy_types::{AccountQuota, ModelQuotaDetail};

        let now = 1700000000;
        let future_str = chrono::DateTime::from_timestamp(now + 3600, 0).unwrap().to_rfc3339();
        let past_str = chrono::DateTime::from_timestamp(now - 3600, 0).unwrap().to_rfc3339();

        let detail = |id: &str, used, reset, frac| ModelQuotaDetail {
            model_id: id.to_string(),
            session_used: used,
            session_limit: 1000,
            session_reset_at: reset,
            remaining_fraction: frac,
        };

        // 1. Claude model with future reset and usage is NOT ready
        let claude_ticking = AccountQuota {
            model_details: Some(vec![detail("claude-sonnet-4-6", 1, Some(future_str.clone()), 0.999)].into_boxed_slice()),
            ..AccountQuota::empty()
        };
        assert!(!super::is_model_quota_ready_for_warmup(&claude_ticking, "claude-sonnet-4-6", now));

        // 2. Claude model with expired reset and 100% capacity IS ready
        let claude_ready = AccountQuota {
            model_details: Some(vec![detail("claude-sonnet-4-6", 0, Some(past_str), 1.0)].into_boxed_slice()),
            ..AccountQuota::empty()
        };
        assert!(super::is_model_quota_ready_for_warmup(&claude_ready, "claude-sonnet-4-6", now));

        // 3. Model matching Claude summary bucket "Claude (5h)"
        let claude_summary_ticking = AccountQuota {
            model_details: Some(vec![detail("Claude (5h)", 1, Some(future_str.clone()), 0.999)].into_boxed_slice()),
            ..AccountQuota::empty()
        };
        assert!(!super::is_model_quota_ready_for_warmup(&claude_summary_ticking, "claude-opus-4-6-thinking", now));

        // 4. Gemini account with 0 weekly used IS ready (kickstarts weekly countdown)
        let gemini_ready = AccountQuota {
            weekly_used: Some(0),
            weekly_limit: Some(1000),
            weekly_reset_at: Some(future_str.clone()),
            session_used: Some(0),
            session_limit: Some(1000),
            session_reset_at: Some(future_str.clone()),
            ..AccountQuota::empty()
        };
        assert!(super::is_model_quota_ready_for_warmup(&gemini_ready, "gemini-2.5-pro", now));

        // 5. Gemini account with weekly usage already ticking is NOT ready
        let gemini_ticking = AccountQuota {
            weekly_used: Some(1),
            weekly_limit: Some(1000),
            weekly_reset_at: Some(future_str),
            session_used: Some(1),
            session_limit: Some(1000),
            ..AccountQuota::empty()
        };
        assert!(!super::is_model_quota_ready_for_warmup(&gemini_ticking, "gemini-2.5-pro", now));
    }
}
