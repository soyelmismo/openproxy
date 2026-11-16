//! Background daemon and runner for CodeBuddy daily check-in and credit balance synchronization.

use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::accounts;
use crate::error::{CoreError, Result};
use crate::ids::{AccountId, ProviderId};
use openproxy_adapters::adapters::codebuddy::checkin::{
    CodeBuddyClaimResult, calculate_codebuddy_streak, claim_codebuddy_daily_checkin,
    fetch_codebuddy_credits,
};
use openproxy_adapters::adapters::codebuddy::quota::{
    CodeBuddyProviderSpecific, parse_codebuddy_provider_meta,
};
use openproxy_adapters::upstream::UpstreamClient;
use openproxy_db::DbPool;
use openproxy_db::secrets::MasterKey;
use rusqlite::OptionalExtension;

pub const CODEBUDDY_PROVIDERS: [&str; 3] =
    ["codebuddy", "codebuddy-code", "@tencent-ai/codebuddy-code"];
const DEFAULT_CHECKIN_INTERVAL_SECS: u64 = 14_400; // 4 hours

/// Shared periodic check-in loop used by both CodeBuddy and MiniMax check-in daemons.
pub(crate) async fn run_periodic_checkin_scheduler<F, Fut>(
    tag: &'static str,
    initial_delay_secs: u64,
    interval_secs: u64,
    cancel: CancellationToken,
    mut cycle_fn: F,
) where
    F: FnMut(CancellationToken) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    tokio::select! {
        () = cancel.cancelled() => return,
        () = sleep(Duration::from_secs(initial_delay_secs)) => {}
    }

    loop {
        if cancel.is_cancelled() {
            tracing::info!("[{tag}] Scheduler shutting down");
            break;
        }

        cycle_fn(cancel.clone()).await;

        tokio::select! {
            () = cancel.cancelled() => {
                tracing::info!("[{tag}] Scheduler shutting down");
                break;
            }
            () = sleep(Duration::from_secs(interval_secs)) => {}
        }
    }
}

/// Shared OAuth account filter for daily check-in sweeps across multiple provider IDs.
pub(crate) async fn collect_oauth_accounts_for_checkin<F>(
    db_pool: &Arc<DbPool>,
    master_key: &Arc<MasterKey>,
    providers: &'static [&'static str],
    should_check: F,
) -> Vec<AccountId>
where
    F: Fn(&accounts::Account) -> bool + Send + 'static,
{
    let pool = Arc::clone(db_pool);
    let key = Arc::clone(master_key);
    tokio::task::spawn_blocking(move || {
        let conn = pool.reader();
        let mut list = Vec::new();
        for pid_str in providers {
            let pid = ProviderId::new(*pid_str);
            if let Ok(accs) = accounts::list(&conn, Some(&pid), &key) {
                for acc in accs {
                    if acc.health_status == accounts::HealthStatus::Unhealthy
                        || acc.auth_type.as_ref() != "oauth"
                    {
                        continue;
                    }
                    if should_check(&acc) {
                        list.push(acc.id);
                    }
                }
            }
        }
        list
    })
    .await
    .unwrap_or_default()
}

/// Starts the background scheduler for CodeBuddy daily checkins.
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

/// Asynchronous runner for the CodeBuddy daily check-in scheduler.
pub async fn run_checkin_scheduler(
    db_pool: Arc<DbPool>,
    upstream_client: Arc<UpstreamClient>,
    master_key: Arc<MasterKey>,
    cancel: CancellationToken,
) {
    run_periodic_checkin_scheduler(
        "CodeBuddyCheckin",
        3,
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

/// Runs a single sweep over all CodeBuddy accounts and claims checkin for those not yet claimed today.
pub async fn run_checkin_cycle(
    db_pool: &Arc<DbPool>,
    upstream_client: &Arc<UpstreamClient>,
    master_key: &Arc<MasterKey>,
) {
    run_checkin_cycle_with_cancel(db_pool, upstream_client, master_key, None).await;
}

/// Runs a single sweep over all CodeBuddy accounts and claims checkin for those not yet claimed today,
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
        &CODEBUDDY_PROVIDERS,
        move |acc| {
            let meta = parse_codebuddy_provider_meta(acc.oauth_provider_specific.as_deref());
            let not_checked_in_today = meta.last_checkin_date.as_deref() != Some(today.as_str());
            let needs_enrichment = meta.credit_balance.is_none();
            not_checked_in_today || needs_enrichment
        },
    )
    .await;

    for account_id in accounts_to_check {
        if cancel.is_some_and(|c| c.is_cancelled()) {
            tracing::info!("[CodeBuddyCheckin] Cycle cancelled between accounts");
            break;
        }

        match run_account_checkin(db_pool, upstream_client, master_key, account_id, None).await {
            Ok(summary) => {
                tracing::info!(
                    account_id = account_id.0,
                    success = summary.success,
                    "[CodeBuddyCheckin] {}",
                    summary.message
                );
            }
            Err(e) => {
                tracing::warn!(
                    account_id = account_id.0,
                    error = %e,
                    "[CodeBuddyCheckin] Failed checkin"
                );
            }
        }
    }
}

/// Executes daily check-in and refreshes credit balance for a single CodeBuddy account.
pub async fn run_account_checkin(
    db_pool: &Arc<DbPool>,
    upstream_client: &Arc<UpstreamClient>,
    master_key: &Arc<MasterKey>,
    account_id: AccountId,
    proxy_url: Option<&str>,
) -> Result<CodeBuddyClaimResult> {
    let (access_token, raw_meta) = {
        let pool = Arc::clone(db_pool);
        let key = Arc::clone(master_key);
        tokio::task::spawn_blocking(move || -> Result<(String, Option<String>)> {
            let conn = pool.reader();
            let token = accounts::decrypt_access_token(&conn, account_id, &key)?;
            let raw_enc = conn
                .query_row(
                    "SELECT oauth_provider_specific FROM accounts WHERE id = ?1",
                    rusqlite::params![account_id.0],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()
                .map_err(openproxy_db::error::map_db_error_ctx("get codebuddy meta"))?
                .flatten();

            let decrypted = raw_enc.and_then(|enc| {
                openproxy_db::accounts::decrypt_oauth_provider_specific(Some(&enc), &key)
                    .or(Some(enc))
            });

            Ok((token, decrypted))
        })
        .await
        .map_err(|e| CoreError::Internal(format!("spawn failed: {e}")))??
    };

    let meta: CodeBuddyProviderSpecific = parse_codebuddy_provider_meta(raw_meta.as_deref());

    let claim_res = claim_codebuddy_daily_checkin(upstream_client, &access_token, proxy_url)
        .await
        .map_err(|e| CoreError::UpstreamConnection(format!("codebuddy daily checkin: {e}")))?;

    let today = chrono::Utc::now().date_naive();
    let today_str = today.format("%Y-%m-%d").to_string();

    let (new_streak, date_to_set) = if claim_res.success {
        let streak = calculate_codebuddy_streak(
            meta.last_checkin_date.as_deref(),
            meta.streak_days,
            today,
        );
        (Some(streak), Some(today_str.as_str()))
    } else {
        (
            meta.streak_days.filter(|_| meta.last_checkin_date.is_some()),
            meta.last_checkin_date.as_deref(),
        )
    };

    // Query upstream for real updated credit balance
    let credits_opt = fetch_codebuddy_credits(upstream_client, &access_token, proxy_url)
        .await
        .unwrap_or(None);

    let (new_bal, new_tot) = match credits_opt {
        Some((bal, tot)) => (Some(bal), Some(tot)),
        None => {
            if claim_res.newly_claimed
                && let Some(granted) = claim_res.credits_granted
            {
                let bal = meta.credit_balance.map(|b| b + granted);
                let tot = meta.total_credits.map(|t| t + granted);
                (bal, tot)
            } else {
                (meta.credit_balance, meta.total_credits)
            }
        }
    };

    let pool = Arc::clone(db_pool);
    let key = Arc::clone(master_key);
    let date_clone = date_to_set.map(ToString::to_string);
    tokio::task::spawn_blocking(move || -> Result<()> {
        let conn = pool.writer();
        openproxy_oauth::oauth::codebuddy::update_codebuddy_meta(
            &conn,
            account_id,
            new_bal,
            new_tot,
            date_clone.as_deref(),
            new_streak,
            &key,
        )
    })
    .await
    .map_err(|e| CoreError::Internal(format!("spawn failed: {e}")))??;

    Ok(claim_res)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_codebuddy_providers_constant() {
        assert!(CODEBUDDY_PROVIDERS.contains(&"codebuddy"));
        assert!(CODEBUDDY_PROVIDERS.contains(&"codebuddy-code"));
        assert!(CODEBUDDY_PROVIDERS.contains(&"@tencent-ai/codebuddy-code"));
    }
}
