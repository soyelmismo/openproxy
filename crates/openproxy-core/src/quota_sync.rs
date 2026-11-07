//! Background quota synchronization: periodically refreshes quota for every account
//! of a provider that supports it, and hosts the single-account refresh shared with
//! the manual UI endpoint.

use crate::AppConfig;
use crate::accounts;
use crate::admin;
use crate::ids::AccountId;
use crate::notifications;
use crate::oauth::{DbRef, OAuthProviderRegistry};
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

/// Writes `last_checkin_date` / `streak_days` into the account's encrypted
/// `oauth_provider_specific` blob.
///
/// Split out of [`spawn_autoclaw_checkin`] so the read-modify-write can be
/// exercised against a real database without reaching the network.
///
/// Fail-closed on an unreadable blob: the column also holds the account's OAuth
/// tokens, so overwriting it with a two-key map would destroy the credential.
fn persist_autoclaw_checkin(
    db_pool: &DbPool,
    master_key: &MasterKey,
    account_id: AccountId,
    checkin: &openproxy_adapters::adapters::zai::AutoclawCheckinResult,
) -> crate::error::Result<()> {
    let conn = db_pool
        .try_writer_for(openproxy_db::conn::ADMIN_LOCK_TIMEOUT)
        .ok_or_else(|| crate::error::CoreError::Internal("writer timeout".into()))?;
    // One immediate transaction: the pool serialises writers, and this keeps
    // the read-modify-write of the credential blob indivisible.
    let tx = conn
        .unchecked_transaction()
        .map_err(openproxy_db::error::map_db_error)?;

    let row: Option<String> = tx
        .query_row(
            "SELECT oauth_provider_specific FROM accounts WHERE id = ?1",
            rusqlite::params![account_id.0],
            |r| r.get(0),
        )
        .ok()
        .flatten();

    let mut map = match row.as_deref() {
        Some(raw_enc) => {
            let plain =
                openproxy_db::accounts::decrypt_oauth_provider_specific(Some(raw_enc), master_key);
            match plain.as_deref().and_then(|p| {
                serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(p).ok()
            }) {
                Some(existing) => existing,
                None => {
                    tracing::error!(
                        account_id = account_id.0,
                        "autoclaw check-in metadata not persisted: existing \
                         oauth_provider_specific is unreadable"
                    );
                    return Ok(());
                }
            }
        }
        None => serde_json::Map::new(),
    };

    map.insert(
        "last_checkin_date".to_string(),
        serde_json::Value::String(checkin.last_checkin_date.clone()),
    );
    map.insert(
        "streak_days".to_string(),
        serde_json::Value::Number(checkin.streak_days.into()),
    );
    let updated_json = serde_json::to_string(&map).unwrap_or_default();
    let enc = openproxy_db::accounts::encrypt_oauth_provider_specific(&updated_json, master_key)?;
    tx.execute(
        "UPDATE accounts SET oauth_provider_specific = ?1 WHERE id = ?2",
        rusqlite::params![enc, account_id.0],
    )
    .map_err(openproxy_db::error::map_db_error_ctx(
        "update autoclaw streak metadata",
    ))?;
    tx.commit().map_err(openproxy_db::error::map_db_error)?;
    Ok(())
}

/// Spawns the AutoClaw daily check-in for one zai account and persists the
/// resulting streak.
///
/// This is the only site that performs the check-in; the quota fetch stays a
/// pure read. Two invariants keep it correct across restarts and manual
/// refreshes:
///
/// 1. **The daily gate is durable.** The advisory check here reads the persisted
///    `last_checkin_date` from the database, so it survives restarts, and the
///    upstream task list inside [`perform_autoclaw_checkin`] is the final
///    authority on whether a claim is still needed. The in-process dedupe window
///    is 60 s and only collapses concurrent runs: a long window silently
///    swallows the next day's claim, which is how the previous 6 h throttle
///    broke the daily bonus.
/// 2. **The write is fail-closed.** See [`persist_autoclaw_checkin`].
fn spawn_autoclaw_checkin(
    account_id: AccountId,
    db_pool: &Arc<DbPool>,
    master_key: &Arc<MasterKey>,
    upstream_client: &Arc<UpstreamClient>,
    token: String,
    proxy: Option<String>,
    provider_specific: Option<Box<str>>,
) {
    // Advisory daily gate. Skipping here only avoids a pointless round trip;
    // the upstream probe inside `perform_autoclaw_checkin` is authoritative.
    if let Some(raw) = provider_specific.as_deref()
        && let Ok(map) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(raw)
        && map.get("last_checkin_date").and_then(|v| v.as_str())
            == Some(openproxy_adapters::adapters::zai::today_utc().as_str())
    {
        tracing::debug!(
            account_id = account_id.0,
            "autoclaw check-in already recorded for today; skipping"
        );
        return;
    }

    let pool = Arc::clone(db_pool);
    let up = Arc::clone(upstream_client);
    let mk = Arc::clone(master_key);
    tokio::spawn(async move {
        let Some(checkin) = openproxy_adapters::adapters::zai::perform_autoclaw_checkin(
            &up,
            &token,
            proxy.as_deref(),
        )
        .await
        else {
            return;
        };

        // Logged after the write, so keep the fields out of the blocking closure.
        let logged_date = checkin.last_checkin_date.clone();
        let logged_streak = checkin.streak_days;
        let logged_already_claimed = checkin.already_claimed;

        let pool = Arc::clone(&pool);
        let mk = Arc::clone(&mk);
        let write_res = tokio::task::spawn_blocking(move || {
            persist_autoclaw_checkin(&pool, &mk, account_id, &checkin)
        })
        .await;

        match write_res {
            Ok(Ok(())) => tracing::info!(
                account_id = account_id.0,
                streak = logged_streak,
                date = %logged_date,
                already_claimed = logged_already_claimed,
                "autoclaw check-in metadata persisted"
            ),
            Ok(Err(e)) => tracing::warn!(
                account_id = account_id.0,
                error = %e,
                "Failed to update AutoClaw checkin streak metadata"
            ),
            Err(e) => tracing::warn!(
                account_id = account_id.0,
                error = %e,
                "AutoClaw check-in metadata writer task failed"
            ),
        }
    });
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

    let active_proxy = crate::free_proxies::resolve_active_proxy(
        db_pool,
        &crate::ids::ProviderId::new(&provider_id_str),
        Some(account_id),
    )
    .await
    .unwrap_or(None);

    let q = admin::fetch_account_quota_with_proxy(
        &provider_id_str,
        upstream_client,
        &api_key,
        access_token.as_deref(),
        provider_specific.as_deref(),
        active_proxy.as_deref(),
    )
    .await;

    if provider_id_str == "minimax" && access_token.is_some() {
        let _ = crate::minimax_checkin::run_account_checkin(
            db_pool,
            upstream_client,
            master_key,
            account_id,
        )
        .await;
    }

    if provider_id_str == "zai" && access_token.is_some() {
        // The daily check-in runs after the token-refresh retry below, so it
        // always uses a live credential. See `spawn_autoclaw_checkin`.
    }

    if provider_id_str == "codex" && access_token.is_some() {
        let pool = Arc::clone(db_pool);
        let up = Arc::clone(upstream_client);
        let tok = access_token.clone().unwrap_or_default();
        let prov_spec = provider_specific.clone();
        tokio::spawn(async move {
            let req = openproxy_adapters::adapters::codex::quota::build_codex_reset_credits_request(
                &tok,
                prov_spec.as_deref(),
            );
            let cancel = openproxy_adapters::CancellationToken::new();
            if let Ok(resp) = up
                .call(
                    req,
                    openproxy_adapters::upstream::TimeoutProfile::Chat,
                    cancel,
                )
                .await
                && resp.status.is_success()
                && let Ok(body) = resp.collect().await
                && let Ok(json) = serde_json::from_slice::<serde_json::Value>(&body)
                && let Ok((_, count)) =
                    openproxy_adapters::adapters::codex::quota::parse_codex_reset_credits(&json)
            {
                let _ = tokio::task::spawn_blocking(move || {
                    let w = pool.writer();
                    openproxy_db::accounts::update_codex_reset_credits(&w, account_id.0, count)
                })
                .await;
            }
        });
    }

    let mut effective_token = access_token.clone();
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
            // Use the global coordinator to serialize refresh calls per-provider.
            // Providers like MiniMax (Auth0-backed) rotate refresh tokens on each
            // use; concurrent callers that bypass the coordinator burn the token
            // and get `invalid_grant`.
            match crate::oauth::TokenRefreshCoordinator::global()
                .refresh_and_store(crate::oauth::OAuthRefreshParams {
                    provider_id: &provider_id_str,
                    provider,
                    refresh_token: &refresh_token,
                    upstream_client,
                    account_id,
                    db: DbRef::Pool(db_pool.as_ref()),
                    master_key,
                    force: true,
                })
                .await
            {
                Ok(new_tokens) => {
                    effective_token = Some(new_tokens.access_token.clone());
                    // Retry quota fetch with the new access token
                    admin::fetch_account_quota_with_proxy(
                        &provider_id_str,
                        upstream_client,
                        &api_key,
                        Some(&new_tokens.access_token),
                        provider_specific.as_deref(),
                        active_proxy.as_deref(),
                    )
                    .await
                }
                Err(e) => {
                    tracing::warn!(
                        account_id = account_id.0,
                        error = %e,
                        "on-demand oauth refresh failed during quota sync"
                    );
                    let db_pool = Arc::clone(db_pool);
                    let _ = tokio::task::spawn_blocking(move || {
                        let w = db_pool.writer();
                        let _ = crate::accounts::set_health(
                            &w,
                            account_id,
                            crate::accounts::HealthStatus::Unhealthy,
                        );
                    })
                    .await;
                    AccountQuota::with_error(format!(
                        "OAuth token expired and refresh failed: re-authentication required ({e})"
                    ))
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

    if provider_id_str == "zai"
        && let Some(token) = effective_token
    {
        spawn_autoclaw_checkin(
            account_id,
            db_pool,
            master_key,
            upstream_client,
            token,
            active_proxy.clone(),
            provider_specific.clone(),
        );
    }

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

    if provider_id_str == "codebuddy"
        && let Some(limit) = q.session_limit
    {
        let used = q.session_used.unwrap_or(0);
        let balance = (limit - used).max(0);
        let db_pool_meta = Arc::clone(db_pool);
        let master_key_meta = Arc::clone(master_key);
        let _ = tokio::task::spawn_blocking(move || {
            let conn = db_pool_meta.writer();
            crate::oauth::codebuddy::update_codebuddy_credit_balance(
                &conn,
                account_id,
                balance,
                limit,
                master_key_meta.as_ref(),
            )
        })
        .await;
    }

    // prune "live-limited" sentinels only after a healthy fetch. `clear_for_account`
    // applies the `until_ts <= now` filter, so a `mark_limited` racing this refresh
    // is not wiped (docs/specs/antigravity-gaps-p2.md §4.4).
    let source_health = q.pools.as_deref().map(quota_pool_health);
    if let Some(health) = source_health {
        let db_pool_health = Arc::clone(db_pool);
        let _ = tokio::task::spawn_blocking(move || {
            let conn = db_pool_health.writer();
            accounts::set_health(&conn, account_id, health)
        })
        .await;
        if health == accounts::HealthStatus::Healthy {
            clear_live_limited_after_refresh(db_pool, account_id).await;
        }
    } else if q.fetch_error.is_none() {
        let db_pool_health = Arc::clone(db_pool);
        let _ = tokio::task::spawn_blocking(move || {
            let conn = db_pool_health.writer();
            let _ = accounts::set_health(&conn, account_id, accounts::HealthStatus::Healthy);
        })
        .await;
        clear_live_limited_after_refresh(db_pool, account_id).await;
    } else if q.fetch_error.as_deref().is_some_and(|e| {
        e.contains("401")
            || e.contains("Unauthorized")
            || e.contains("Authentication failed")
            || e.contains("402")
            || e.contains("Insufficient Balance")
            || e.contains("Payment Required")
            || e.contains("insufficient_quota")
    }) {
        let db_pool_health = Arc::clone(db_pool);
        let _ = tokio::task::spawn_blocking(move || {
            let conn = db_pool_health.writer();
            let _ = accounts::set_health(&conn, account_id, accounts::HealthStatus::Unhealthy);
        })
        .await;
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
    } else if let Some(ref err) = q.fetch_error {
        let dedup_key = format!(
            "{}:{}",
            notifications::CODE_QUOTA_FETCH_FAILED,
            account_id.0
        );
        let payload = serde_json::json!({
            "code": notifications::CODE_QUOTA_FETCH_FAILED,
            "message": format!(
                "Account {} on {} quota fetch failed: {}",
                account_id.0, provider_id_str, err,
            ),
            "provider_id": &provider_id_str,
            "details": {
                "account_id": account_id.0,
                "provider_id": &provider_id_str,
                "error": err,
                "proxy_used": active_proxy,
            },
        });
        let db_pool_notif = Arc::clone(db_pool);
        let provider_id_notif = provider_id_str.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let w = db_pool_notif.writer();
            let _ = notifications::insert_and_broadcast(
                &w,
                notifications::KIND_SYSTEM,
                &payload,
                Some(&dedup_key),
                Some(&provider_id_notif),
            );
        })
        .await;

        if active_proxy.is_some() {
            let is_proxy_err = err.contains("handshake")
                || err.contains("HTTP CONNECT")
                || err.contains("proxy connection error")
                || err.contains("Proxy")
                || err.contains("402");
            if is_proxy_err {
                let is_connect = err.contains("handshake")
                    || err.contains("connection error")
                    || err.contains("timeout");
                let _ = crate::free_proxies::report_proxy_failure(
                    db_pool,
                    &crate::ids::ProviderId::new(&provider_id_str),
                    Some(account_id),
                    is_connect,
                )
                .await;
            }
        }
    }

    Ok(Some(q))
}

/// Prune expired per-(account, model) "live-limited" rows. Split out so it can be
/// tested without the full `refresh_single_account_quota` machinery.
///
/// The writer guard is acquired and released entirely inside `spawn_blocking`: no
/// guard crosses an `.await` (AGENTS.md §4.3).
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

fn quota_pool_health(pools: &[openproxy_types::quota::QuotaPool]) -> accounts::HealthStatus {
    use openproxy_types::quota::QuotaPoolStatus;
    let usable_source = pools
        .iter()
        .any(|pool| pool.status == QuotaPoolStatus::Active && pool.fetch_error.is_none());
    let unreadable_source = pools.iter().any(|pool| {
        let is_missing_credential = pool.fetch_error.as_deref().is_some_and(|err| {
            err.contains("not configured")
                || err.contains("credential is missing")
                || err.contains("credential is not configured")
        });
        !is_missing_credential
            && (pool.status == QuotaPoolStatus::Unavailable || pool.fetch_error.is_some())
    });
    if usable_source || !unreadable_source {
        accounts::HealthStatus::Healthy
    } else {
        accounts::HealthStatus::Degraded
    }
}

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
