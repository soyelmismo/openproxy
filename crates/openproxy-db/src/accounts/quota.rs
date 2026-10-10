//! Account quota persistence.

use openproxy_types::quota::AccountQuota;
use openproxy_types::{AccountId, CoreError, Result};
use rusqlite::{Connection, params};

/// Serialize the optional JSON sidecars of a quota snapshot.
///
/// Both sidecars share one rule: an absent payload and an empty payload are
/// indistinguishable on disk (`NULL`), and a serialization failure is a hard
/// error the caller must see — a pool that cannot be written must not be
/// silently dropped, because the next read would report it as gone.
fn quota_pools_json(pools: Option<&[openproxy_types::quota::QuotaPool]>) -> Result<Option<String>> {
    let Some(pools) = pools else {
        return Ok(None);
    };
    let json = serde_json::to_string(pools)
        .map_err(|e| CoreError::Parse(format!("serialize quota_pools: {e}")))?;
    if json.is_empty() || json == "null" || json == "[]" {
        return Ok(None);
    }
    Ok(Some(json))
}

/// Stamp a fresh quota snapshot onto the account row.
///
/// Every field — legacy aggregate columns and the new `quota_pools` JSON — is
/// written in a single UPDATE so the row stays consistent: a partial write can
/// never leave fresh session numbers next to a stale pool snapshot (or vice
/// versa). Serialization of the pools happens *before* the statement runs, so
/// an unserializable pool aborts the whole snapshot instead of persisting the
/// legacy half of it. A failure to find the row surfaces as
/// [`CoreError::AccountNotFound`].
pub fn set_quota(conn: &Connection, id: AccountId, q: &AccountQuota) -> Result<()> {
    use rusqlite::OptionalExtension;
    let prior: Option<String> = conn
        .query_row(
            "SELECT quota_pools FROM accounts WHERE id = ?1",
            [id.0],
            |row| row.get(0),
        )
        .optional()
        .map_err(crate::error::map_db_error_ctx("read previous quota pools"))?
        .flatten();
    let prior = AccountQuota {
        pools: prior
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|_| CoreError::Parse("stored quota pools could not be read".into()))?,
        ..AccountQuota::empty()
    };
    let q = q.clone().merge_over_previous(Some(&prior));
    let model_details_json: Option<String> = q
        .model_details
        .as_ref()
        .and_then(|d| serde_json::to_string(d).ok())
        .filter(|s| s != "null" && s != "[]");

    let quota_pools_json = quota_pools_json(q.pools.as_deref())?;

    let affected = conn
        .execute(
            "UPDATE accounts SET \
                quota_session_used       = ?1, \
                quota_session_limit      = ?2, \
                quota_session_reset_at   = ?3, \
                quota_weekly_used        = ?4, \
                quota_weekly_limit       = ?5, \
                quota_weekly_reset_at    = ?6, \
                quota_plan_name          = ?7, \
                quota_last_fetched_at    = ?8, \
                quota_fetch_error        = ?9, \
                quota_model_details      = ?10, \
                quota_pools              = ?11 \
             WHERE id = ?12",
            params![
                q.session_used,
                q.session_limit,
                q.session_reset_at,
                q.weekly_used,
                q.weekly_limit,
                q.weekly_reset_at,
                q.plan_name,
                q.last_fetched_at,
                q.fetch_error,
                model_details_json,
                quota_pools_json,
                id.0,
            ],
        )
        .map_err(crate::error::map_db_error_ctx(format!(
            "update quota for account {}",
            id.0
        )))?;
    if affected == 0 {
        return Err(CoreError::AccountNotFound(id.0));
    }
    Ok(())
}
