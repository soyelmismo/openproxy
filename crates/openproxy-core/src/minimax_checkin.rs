//! Background daemon and runner for MiniMax daily check-in (signin rewards).

use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::accounts;
use crate::codebuddy_checkin::{
    collect_oauth_accounts_for_checkin, run_periodic_checkin_scheduler,
};
use crate::error::{CoreError, Result};
use crate::ids::AccountId;
use crate::oauth::minimax::MiniMaxAccountMeta;
use crate::oauth::minimax::checkin::{self, DailyCheckinSummary};
use crate::oauth::minimax::matrix::MiniMaxRegion;
use openproxy_adapters::upstream::UpstreamClient;
use openproxy_db::DbPool;
use openproxy_db::secrets::MasterKey;
use openproxy_types::ResultExt;

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
        run_checkin_scheduler(db_pool, upstream_client, master_key, token).await;
    });

    Some(cancel)
}

/// Asynchronous runner for the MiniMax daily check-in scheduler.
pub async fn run_checkin_scheduler(
    db_pool: Arc<DbPool>,
    upstream_client: Arc<UpstreamClient>,
    master_key: Arc<MasterKey>,
    cancel: CancellationToken,
) {
    run_periodic_checkin_scheduler(
        "MiniMaxCheckin",
        45,
        DEFAULT_CHECKIN_INTERVAL_SECS,
        cancel,
        move |c| {
            let db_pool = Arc::clone(&db_pool);
            let upstream_client = Arc::clone(&upstream_client);
            let master_key = Arc::clone(&master_key);
            async move {
                run_checkin_cycle_with_cancel(&db_pool, &upstream_client, &master_key, Some(&c))
                    .await;
            }
        },
    )
    .await;
}

/// Runs a single sweep over all MiniMax accounts and claims checkin for those not yet claimed today.
pub async fn run_checkin_cycle(
    db_pool: &Arc<DbPool>,
    upstream_client: &Arc<UpstreamClient>,
    master_key: &Arc<MasterKey>,
) {
    run_checkin_cycle_with_cancel(db_pool, upstream_client, master_key, None).await;
}

/// Runs a single sweep over all MiniMax accounts and claims checkin for those not yet claimed today,
/// with cooperative cancellation checking between accounts.
pub async fn run_checkin_cycle_with_cancel(
    db_pool: &Arc<DbPool>,
    upstream_client: &Arc<UpstreamClient>,
    master_key: &Arc<MasterKey>,
    cancel: Option<&CancellationToken>,
) {
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();

    let accounts_to_check = collect_oauth_accounts_for_checkin(
        db_pool,
        master_key,
        &MINIMAX_PROVIDERS,
        move |acc| {
            let meta = acc
                .oauth_provider_specific
                .as_deref()
                .and_then(|r| serde_json::from_str::<MiniMaxAccountMeta>(r).ok())
                .unwrap_or_default();
            let not_checked_in_today = meta.last_checkin_date.as_deref() != Some(today.as_str());
            let needs_enrichment = meta.credit_balance.is_none()
                || meta.real_user_id.is_none()
                || acc.label.as_deref().unwrap_or("").trim().is_empty();
            not_checked_in_today || needs_enrichment
        },
    )
    .await;

    for account_id in accounts_to_check {
        if cancel.is_some_and(|c| c.is_cancelled()) {
            tracing::info!("[MiniMaxCheckin] Cycle cancelled between accounts");
            break;
        }

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
            let meta = openproxy_db::accounts::read_provider_meta::<MiniMaxAccountMeta>(
                &conn,
                Some(&key),
                account_id.0,
            )?
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

    // AFTER checkin: refresca membresía (op_group_id, tier, credit_balance)
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

    let meta_json = serde_json::to_string(&meta).ctx_parse("serialize meta")?;

    let final_email = meta.email.clone();
    let display_label = identity.display_label();
    let final_label = if display_label.is_empty() {
        format!("MiniMax User {user_id}")
    } else {
        display_label
    };

    let pool = Arc::clone(db_pool);
    let key = Arc::clone(master_key);
    tokio::task::spawn_blocking(move || -> Result<()> {
        let conn = pool
            .try_writer_for(openproxy_db::conn::ADMIN_LOCK_TIMEOUT)
            .ok_or_else(|| CoreError::Internal("writer timeout".into()))?;
        let encrypted_meta =
            openproxy_db::accounts::encrypt_oauth_provider_specific(&meta_json, &key)?;
        conn.execute(
            "UPDATE accounts SET oauth_provider_specific = ?1, email = COALESCE(?2, email), \
             label = COALESCE(NULLIF(label, ''), ?3) WHERE id = ?4",
            rusqlite::params![encrypted_meta, final_email, final_label, account_id.0],
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
