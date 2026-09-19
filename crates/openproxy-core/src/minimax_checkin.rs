//! Background daemon and runner for MiniMax daily check-in (signin rewards).

use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::accounts;
use crate::error::{CoreError, Result};
use crate::ids::{AccountId, ProviderId};
use crate::oauth::minimax::MiniMaxAccountMeta;
use crate::oauth::minimax::checkin::{self, DailyCheckinSummary};
use crate::oauth::minimax::matrix::MiniMaxRegion;
use openproxy_adapters::upstream::UpstreamClient;
use openproxy_db::DbPool;
use openproxy_db::secrets::MasterKey;

const MINIMAX_PROVIDERS: [&str; 4] = ["minimax", "minimax-coding", "minimax-managed", "minimax-cn"];
const DEFAULT_CHECKIN_INTERVAL_SECS: u64 = 14_400; // 4 hours

/// Starts the background scheduler for MiniMax daily checkins.
pub fn start_checkin_scheduler(
    db_pool: Arc<DbPool>,
    upstream_client: Arc<UpstreamClient>,
    master_key: Arc<MasterKey>,
) -> Option<CancellationToken> {
    let cancel = CancellationToken::new();
    let token = cancel.clone();

    tokio::spawn(async move {
        // Initial delay so system boot finishes
        tokio::select! {
            () = token.cancelled() => return,
            () = sleep(Duration::from_secs(45)) => {}
        }

        loop {
            run_checkin_cycle(&db_pool, &upstream_client, &master_key).await;

            tokio::select! {
                () = token.cancelled() => {
                    tracing::info!("[MiniMaxCheckin] Scheduler shutting down");
                    break;
                }
                () = sleep(Duration::from_secs(DEFAULT_CHECKIN_INTERVAL_SECS)) => {}
            }
        }
    });

    Some(cancel)
}

/// Runs a single sweep over all MiniMax accounts and claims checkin for those not yet claimed today.
pub async fn run_checkin_cycle(
    db_pool: &Arc<DbPool>,
    upstream_client: &Arc<UpstreamClient>,
    master_key: &Arc<MasterKey>,
) {
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();

    let accounts_to_check = {
        let pool = Arc::clone(db_pool);
        let key = Arc::clone(master_key);
        let today_clone = today.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.reader();
            let mut list = Vec::new();
            for pid_str in &MINIMAX_PROVIDERS {
                let pid = ProviderId::new(*pid_str);
                if let Ok(accs) = accounts::list(&conn, Some(&pid), &key) {
                    for acc in accs {
                        if acc.health_status == accounts::HealthStatus::Unhealthy
                            || acc.auth_type.as_ref() != "oauth"
                        {
                            continue;
                        }

                        let meta = acc
                            .oauth_provider_specific
                            .as_deref()
                            .and_then(|r| serde_json::from_str::<MiniMaxAccountMeta>(r).ok())
                            .unwrap_or_default();

                        let not_checked_in_today =
                            meta.last_checkin_date.as_deref() != Some(today_clone.as_str());
                        let needs_enrichment = meta.credit_balance.is_none()
                            || meta.real_user_id.is_none()
                            || acc.label.as_deref().unwrap_or("").trim().is_empty();

                        if not_checked_in_today || needs_enrichment {
                            list.push(acc.id);
                        }
                    }
                }
            }
            list
        })
        .await
        .unwrap_or_default()
    };

    for account_id in accounts_to_check {
        match run_account_checkin(db_pool, upstream_client, master_key, account_id).await {
            Ok(summary) => {
                tracing::info!(
                    account_id = account_id.0,
                    points = summary.points_claimed,
                    streak = summary.streak_days,
                    "[MiniMaxCheckin] {}",
                    summary.message
                );
            }
            Err(e) => {
                tracing::warn!(
                    account_id = account_id.0,
                    error = %e,
                    "[MiniMaxCheckin] Failed checkin"
                );
            }
        }
    }
}

/// Executes daily checkin on a single account and updates metadata.
pub async fn run_account_checkin(
    db_pool: &Arc<DbPool>,
    upstream_client: &Arc<UpstreamClient>,
    master_key: &Arc<MasterKey>,
    account_id: AccountId,
) -> Result<DailyCheckinSummary> {
    let (access_token, mut meta) = {
        let pool = Arc::clone(db_pool);
        let key = Arc::clone(master_key);
        tokio::task::spawn_blocking(move || -> Result<(String, MiniMaxAccountMeta)> {
            let conn = pool.reader();
            let token = accounts::decrypt_access_token(&conn, account_id, &key)?;
            let raw: Option<String> = conn
                .query_row(
                    "SELECT oauth_provider_specific FROM accounts WHERE id = ?1",
                    rusqlite::params![account_id.0],
                    |r| r.get(0),
                )
                .unwrap_or(None);

            let meta = raw
                .and_then(|s| serde_json::from_str::<MiniMaxAccountMeta>(&s).ok())
                .unwrap_or_default();

            Ok((token, meta))
        })
        .await
        .map_err(|e| CoreError::Internal(format!("spawn failed: {e}")))??
    };

    let region = meta
        .region
        .as_deref()
        .map_or(MiniMaxRegion::Global, MiniMaxRegion::parse_str);

    // Enrich identity (real_user_id, email, display_name)
    let identity =
        crate::oauth::minimax::resolve_user_identity(upstream_client, &access_token, region).await;
    if meta.real_user_id.is_none() && identity.real_user_id.is_some() {
        meta.real_user_id = identity.real_user_id.clone();
    }
    if meta.email.is_none() && identity.email.is_some() {
        meta.email = identity.email.clone();
    }

    let user_id = meta.real_user_id.as_deref().unwrap_or("0");

    let summary =
        checkin::execute_daily_checkin(upstream_client, &access_token, user_id, region).await?;

    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    meta.last_checkin_date = Some(today);
    meta.streak_days = Some(summary.streak_days);

    // Refresh membership info (op_group_id, tier, credit_balance) AFTER checkin
    let uid = meta.real_user_id.as_deref().unwrap_or("0");
    if let Some((op_group_id, tier, credits)) =
        crate::oauth::minimax::resolve_membership_info(upstream_client, &access_token, uid, region)
            .await
    {
        meta.op_group_id = Some(op_group_id);
        if tier.is_some() {
            meta.token_plan_tier = tier;
        }
        if credits.is_some() {
            meta.credit_balance = credits;
        }
    }

    let meta_json = serde_json::to_string(&meta)
        .map_err(|e| CoreError::Parse(format!("serialize meta: {e}")))?;

    let final_email = meta.email.clone();
    let display_label = identity.display_label();
    let final_label = if display_label.is_empty() {
        format!("MiniMax User {user_id}")
    } else {
        display_label
    };

    let pool = Arc::clone(db_pool);
    tokio::task::spawn_blocking(move || -> Result<()> {
        let conn = pool
            .try_writer_for(openproxy_db::conn::ADMIN_LOCK_TIMEOUT)
            .ok_or_else(|| CoreError::Internal("writer timeout".into()))?;
        conn.execute(
            "UPDATE accounts SET oauth_provider_specific = ?1, email = COALESCE(?2, email), \
             label = COALESCE(NULLIF(label, ''), ?3) WHERE id = ?4",
            rusqlite::params![meta_json, final_email, final_label, account_id.0],
        )
        .map_err(openproxy_db::error::map_db_error_ctx(
            "update provider_specific + label",
        ))?;
        Ok(())
    })
    .await
    .map_err(|e| CoreError::Internal(format!("spawn failed: {e}")))??;

    Ok(summary)
}
