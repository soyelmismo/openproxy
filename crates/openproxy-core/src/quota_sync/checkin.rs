//! AutoClaw daily check-in: the read-modify-write persistence helper and the
//! background spawn wrapper. Split from `quota_sync.rs` to keep each file under
//! the LOC invariant.

use crate::ids::AccountId;
use openproxy_adapters::upstream::UpstreamClient;
use openproxy_db::DbPool;
use openproxy_db::secrets::MasterKey;
use std::sync::Arc;

/// Writes `last_checkin_date` / `streak_days` into the account's encrypted
/// `oauth_provider_specific` blob.
///
/// Split out of [`spawn_autoclaw_checkin`] so the read-modify-write can be
/// exercised against a real database without reaching the network.
///
/// Fail-closed on an unreadable blob: the column also holds the account's OAuth
/// tokens, so overwriting it with a two-key map would destroy the credential.
pub(crate) fn persist_autoclaw_checkin(
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
pub(crate) fn spawn_autoclaw_checkin(
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
