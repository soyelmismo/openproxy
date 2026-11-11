//! Background daemon for the AutoClaw (Z.ai) daily credit claim.
//!
//! Root-cause fix for missed daily claims. The legacy path spawned the
//! check-in from the quota-sync cycle, whose account set excludes Unhealthy
//! accounts and whose token refresh runs only after a quota fetch failure —
//! so an account that failed one OAuth refresh stopped claiming forever, and
//! a drained account that flipped to Unhealthy silently dropped out of the
//! daily bonus. Pausing an account (`active = 0`) never excluded it from that
//! cycle, but every other failure did.
//!
//! This scheduler owns the claim instead: it selects EVERY `zai` OAuth
//! account — active or paused, any health status — daily-gates on the
//! persisted claim date, and retries once through a coordinator-driven token
//! refresh when the claim outcome is unknown (typically an expired access
//! token). Claiming never writes `health_status`: a claim failure must not
//! feed back into routing eligibility.
//!
//! The persisted-date gate is advisory only. The upstream task list inside
//! [`perform_autoclaw_checkin`] is the final authority, claims are idempotent
//! upstream, and the in-process dedupe collapses concurrent runs, so a gate
//! that opens late costs nothing and a gate that opens early cannot
//! double-claim.

use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::accounts;
use crate::free_proxies::resolve_active_proxy;
use crate::ids::{AccountId, ProviderId};
use crate::oauth::{DbRef, OAuthProviderRegistry};
use crate::quota_sync::checkin::persist_autoclaw_checkin;
use openproxy_adapters::adapters::zai::{
    AutoclawCheckinResult, perform_autoclaw_checkin, today_utc,
};
use openproxy_adapters::upstream::UpstreamClient;
use openproxy_db::DbPool;
use openproxy_db::secrets::MasterKey;

const ZAI_PROVIDER_ID: &str = "zai";
/// Hourly cadence. The daily gate opens at the UTC date flip, so a claim
/// lands within the first hour of each UTC day; a failed attempt is retried
/// every hour until it lands instead of waiting 24 h.
const CYCLE_INTERVAL_SECS: u64 = 3_600;
/// Breathing room between accounts: the upstream rate-limits claim bursts.
const INTER_ACCOUNT_DELAY_MS: u64 = 2_000;

/// Outcome of one account's claim attempt, for cycle logging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaimOutcome {
    Claimed { streak: u32, already_claimed: bool },
    NoCredential,
    Unresolved,
}

/// Scheduler loop for the AutoClaw daily claim.
///
/// Driven by `AutoClawCheckinService`; runs one sweep at startup and then
/// once per [`CYCLE_INTERVAL_SECS`], checking cancellation between accounts.
pub async fn run_autoclaw_checkin_scheduler(
    db_pool: Arc<DbPool>,
    upstream_client: Arc<UpstreamClient>,
    master_key: Arc<MasterKey>,
    oauth_registry: Arc<OAuthProviderRegistry>,
    cancel: CancellationToken,
) {
    // Initial delay: stay out of the boot stampede.
    tokio::select! {
        () = cancel.cancelled() => return,
        () = sleep(Duration::from_secs(45)) => {}
    }

    loop {
        if cancel.is_cancelled() {
            tracing::info!("[AutoClawCheckin] Scheduler shutting down");
            break;
        }

        run_checkin_cycle_with_cancel(
            &db_pool,
            &upstream_client,
            &master_key,
            &oauth_registry,
            Some(&cancel),
        )
        .await;

        tokio::select! {
            () = cancel.cancelled() => {
                tracing::info!("[AutoClawCheckin] Scheduler shutting down");
                break;
            }
            () = sleep(Duration::from_secs(CYCLE_INTERVAL_SECS)) => {}
        }
    }
}

/// One sweep over every `zai` OAuth account whose daily claim is still open.
async fn run_checkin_cycle_with_cancel(
    db_pool: &Arc<DbPool>,
    upstream_client: &Arc<UpstreamClient>,
    master_key: &Arc<MasterKey>,
    oauth_registry: &Arc<OAuthProviderRegistry>,
    cancel: Option<&CancellationToken>,
) {
    let candidates = {
        let pool = Arc::clone(db_pool);
        let key = Arc::clone(master_key);
        tokio::task::spawn_blocking(move || {
            let conn = pool.reader();
            gather_candidate_accounts(&conn, key.as_ref())
        })
        .await
        .unwrap_or_default()
    };

    if candidates.is_empty() {
        return;
    }
    tracing::debug!(
        count = candidates.len(),
        "[AutoClawCheckin] cycle starting for accounts pending today's claim"
    );

    for account_id in candidates {
        if cancel.is_some_and(CancellationToken::is_cancelled) {
            tracing::info!("[AutoClawCheckin] Cycle cancelled between accounts");
            break;
        }

        match claim_account(
            db_pool,
            upstream_client,
            master_key,
            oauth_registry,
            account_id,
        )
        .await
        {
            ClaimOutcome::Claimed {
                streak,
                already_claimed,
            } => tracing::info!(
                account_id = account_id.0,
                streak,
                already_claimed,
                "[AutoClawCheckin] daily claim settled"
            ),
            ClaimOutcome::NoCredential => tracing::debug!(
                account_id = account_id.0,
                "[AutoClawCheckin] account has no AutoClaw OAuth credential; skipped"
            ),
            ClaimOutcome::Unresolved => tracing::warn!(
                account_id = account_id.0,
                "[AutoClawCheckin] claim outcome unknown; retrying next cycle"
            ),
        }

        sleep(Duration::from_millis(INTER_ACCOUNT_DELAY_MS)).await;
    }
}

/// Claims the daily bonus for one account, refreshing the token if the
/// first attempt has no established outcome.
async fn claim_account(
    db_pool: &Arc<DbPool>,
    upstream_client: &Arc<UpstreamClient>,
    master_key: &Arc<MasterKey>,
    oauth_registry: &Arc<OAuthProviderRegistry>,
    account_id: AccountId,
) -> ClaimOutcome {
    let (stored_token, refresh_token) = {
        let pool = Arc::clone(db_pool);
        let key = Arc::clone(master_key);
        tokio::task::spawn_blocking(move || load_credentials(&pool, key.as_ref(), account_id))
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(
                    account_id = account_id.0,
                    error = %e,
                    "[AutoClawCheckin] credential load failed"
                );
                (None, None)
            })
    };

    if stored_token.is_none() && refresh_token.is_none() {
        return ClaimOutcome::NoCredential;
    }

    let mut token = match stored_token {
        Some(token) => token,
        None => match refresh_access_token(
            db_pool,
            upstream_client,
            master_key,
            oauth_registry,
            account_id,
            refresh_token.as_deref(),
        )
        .await
        {
            Some(fresh) => fresh,
            None => return ClaimOutcome::Unresolved,
        },
    };

    let proxy = resolve_active_proxy(db_pool, &ProviderId::new(ZAI_PROVIDER_ID), Some(account_id))
        .await
        .unwrap_or(None);

    let mut outcome = perform_autoclaw_checkin(upstream_client, &token, proxy.as_deref()).await;

    // An unknown outcome usually means the access token expired upstream.
    // Refresh once and retry: this is the path a paused or Unhealthy account
    // never had, and the reason its claims stopped silently.
    if outcome.is_none()
        && refresh_token.is_some()
        && let Some(fresh) = refresh_access_token(
            db_pool,
            upstream_client,
            master_key,
            oauth_registry,
            account_id,
            refresh_token.as_deref(),
        )
        .await
        && fresh != token
    {
        token = fresh;
        outcome = perform_autoclaw_checkin(upstream_client, &token, proxy.as_deref()).await;
    }

    let Some(checkin) = outcome else {
        return ClaimOutcome::Unresolved;
    };
    settle_claim(db_pool, master_key, account_id, &checkin).await
}

/// Persists a settled claim (upstream already recorded it) and reports it.
///
/// A persist failure does not demote the outcome: the upstream claim landed,
/// and the next cycle re-probes it as `already_claimed`.
async fn settle_claim(
    db_pool: &Arc<DbPool>,
    master_key: &Arc<MasterKey>,
    account_id: AccountId,
    checkin: &AutoclawCheckinResult,
) -> ClaimOutcome {
    let pool = Arc::clone(db_pool);
    let key = Arc::clone(master_key);
    let outcome = ClaimOutcome::Claimed {
        streak: checkin.streak_days,
        already_claimed: checkin.already_claimed,
    };
    // `AutoclawCheckinResult` is not `Clone`: `.clone()` on the shared
    // reference would copy the borrow itself and escape the function body.
    let owned = AutoclawCheckinResult {
        last_checkin_date: checkin.last_checkin_date.clone(),
        streak_days: checkin.streak_days,
        already_claimed: checkin.already_claimed,
    };
    let write = tokio::task::spawn_blocking(move || {
        persist_autoclaw_checkin(&pool, key.as_ref(), account_id, &owned)
    })
    .await;

    match write {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!(
            account_id = account_id.0,
            error = %e,
            "[AutoClawCheckin] claim settled upstream but metadata persist failed"
        ),
        Err(e) => tracing::warn!(
            account_id = account_id.0,
            error = %e,
            "[AutoClawCheckin] metadata writer task failed"
        ),
    }
    outcome
}

/// Reads the account's stored access and refresh tokens.
///
/// A missing or undecryptable access token is `None`, not an error: the
/// refresh token is the fallback credential and a corrupt row must not abort
/// the sweep for the other accounts.
fn load_credentials(
    db_pool: &DbPool,
    master_key: &MasterKey,
    account_id: AccountId,
) -> (Option<String>, Option<String>) {
    let conn = db_pool.reader();
    let access = accounts::decrypt_access_token(&conn, account_id, master_key).ok();
    let refresh = accounts::decrypt_refresh_token(&conn, account_id, master_key)
        .ok()
        .flatten();
    (access, refresh)
}

/// Refreshes the account's access token through the global coordinator.
///
/// The coordinator serialises refreshes per `(provider, account)`; the zai
/// refresh token does not rotate, so `force` is safe and idempotent.
async fn refresh_access_token(
    db_pool: &Arc<DbPool>,
    upstream_client: &Arc<UpstreamClient>,
    master_key: &Arc<MasterKey>,
    oauth_registry: &Arc<OAuthProviderRegistry>,
    account_id: AccountId,
    refresh_token: Option<&str>,
) -> Option<String> {
    let refresh_token = refresh_token?;
    let provider = oauth_registry.get(ZAI_PROVIDER_ID)?;
    match crate::oauth::TokenRefreshCoordinator::global()
        .refresh_and_store(crate::oauth::OAuthRefreshParams {
            provider_id: ZAI_PROVIDER_ID,
            provider,
            refresh_token,
            upstream_client,
            account_id,
            db: DbRef::Pool(db_pool),
            master_key: master_key.as_ref(),
            force: true,
        })
        .await
    {
        Ok(tokens) => Some(tokens.access_token),
        Err(e) => {
            tracing::warn!(
                account_id = account_id.0,
                error = %e,
                "[AutoClawCheckin] oauth refresh failed; claim deferred to next cycle"
            );
            None
        }
    }
}

/// Every `zai` OAuth account whose daily claim is still open.
///
/// Deliberately ignores `active` and `health_status`: a paused account keeps
/// stockpiling daily credits, and an Unhealthy one must re-enter the claim
/// flow without waiting for a manual health reset. Accounts authenticated
/// only by API key have no AutoClaw session and are skipped.
fn gather_candidate_accounts(
    conn: &rusqlite::Connection,
    master_key: &MasterKey,
) -> Vec<AccountId> {
    let provider_id = ProviderId::new(ZAI_PROVIDER_ID);
    let Ok(accs) = accounts::list(conn, Some(&provider_id), master_key) else {
        tracing::warn!("[AutoClawCheckin] could not list zai accounts");
        return Vec::new();
    };
    let today = today_utc();
    accs.into_iter()
        .filter(|acc| acc.auth_type.as_ref() == "oauth")
        .filter(|acc| !already_claimed_today(acc.oauth_provider_specific.as_deref(), &today))
        .map(|acc| acc.id)
        .collect()
}

/// Advisory daily gate: the persisted claim date matches today's UTC date.
///
/// `today_utc` matches the date [`perform_autoclaw_checkin`] records, so the
/// gate closes exactly when the existing quota-sync hook closes it.
fn already_claimed_today(provider_specific: Option<&str>, today: &str) -> bool {
    provider_specific
        .and_then(|raw| {
            serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(raw).ok()
        })
        .and_then(|map| {
            map.get("last_checkin_date")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        })
        .as_deref()
        == Some(today)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded_pool() -> Arc<DbPool> {
        let pool =
            DbPool::test_pool_with_prefix("openproxy-autoclaw-checkin-test").expect("open pool");
        {
            let conn = pool.open_connection().expect("open conn");
            conn.execute(
                "INSERT INTO providers(id, name, base_url, auth_type, format) \
                 VALUES ('zai', 'Z.ai', 'https://x', 'oauth', 'openai')",
                [],
            )
            .expect("seed provider");
            conn.execute(
                "INSERT INTO accounts(provider_id, label, auth_type, active, health_status) \
                 VALUES ('zai', 'active', 'oauth', 1, 'healthy')",
                [],
            )
            .expect("seed active");
            conn.execute(
                "INSERT INTO accounts(provider_id, label, auth_type, active, health_status) \
                 VALUES ('zai', 'paused', 'oauth', 0, 'healthy')",
                [],
            )
            .expect("seed paused");
            conn.execute(
                "INSERT INTO accounts(provider_id, label, auth_type, active, health_status) \
                 VALUES ('zai', 'unhealthy', 'oauth', 0, 'unhealthy')",
                [],
            )
            .expect("seed unhealthy");
            conn.execute(
                "INSERT INTO accounts(provider_id, label, auth_type, active, health_status) \
                 VALUES ('zai', 'key-only', 'api_key', 1, 'healthy')",
                [],
            )
            .expect("seed key-only");
        }
        Arc::new(pool)
    }

    #[test]
    fn candidates_include_paused_and_unhealthy_oauth_accounts() {
        let pool = seeded_pool();
        let master_key = MasterKey::generate().expect("generate key");
        let conn = pool.reader();
        let ids = gather_candidate_accounts(&conn, &master_key);
        assert_eq!(
            ids.len(),
            3,
            "active + paused + unhealthy oauth accounts; api-key-only excluded"
        );
    }

    #[test]
    fn candidates_exclude_accounts_already_claimed_today() {
        let pool = seeded_pool();
        let master_key = MasterKey::generate().expect("generate key");
        let claimed_blob = openproxy_db::accounts::encrypt_oauth_provider_specific(
            &serde_json::json!({ "last_checkin_date": today_utc() }).to_string(),
            &master_key,
        )
        .expect("encrypt blob");
        {
            let conn = pool.open_connection().expect("open conn");
            conn.execute(
                "UPDATE accounts SET oauth_provider_specific = ?1 WHERE label = 'paused'",
                rusqlite::params![claimed_blob],
            )
            .expect("mark claimed");
        }
        let conn = pool.reader();
        let ids = gather_candidate_accounts(&conn, &master_key);
        assert_eq!(ids.len(), 2, "the claimed-today account is gated out");
    }

    #[test]
    fn gate_matches_only_the_exact_today_date() {
        assert!(already_claimed_today(
            Some(r#"{"last_checkin_date":"2026-11-11"}"#),
            "2026-11-11"
        ));
        assert!(!already_claimed_today(
            Some(r#"{"last_checkin_date":"2026-11-10"}"#),
            "2026-11-11"
        ));
        assert!(!already_claimed_today(Some("not json"), "2026-11-11"));
        assert!(!already_claimed_today(None, "2026-11-11"));
        assert!(!already_claimed_today(
            Some(r#"{"last_checkin_date":7}"#),
            "2026-11-11"
        ));
    }
}
