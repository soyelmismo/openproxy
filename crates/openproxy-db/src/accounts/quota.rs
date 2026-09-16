//! Account quota persistence.

use openproxy_types::quota::AccountQuota;
use openproxy_types::{AccountId, CoreError, Result};
use rusqlite::{Connection, params};

/// Stamp a fresh quota snapshot onto the account row.
///
/// Every field is written in a single UPDATE so the row stays consistent.
/// A failure to find the row surfaces as [`CoreError::AccountNotFound`].
pub fn set_quota(conn: &Connection, id: AccountId, q: &AccountQuota) -> Result<()> {
    let model_details_json: Option<String> = q
        .model_details
        .as_ref()
        .and_then(|d| serde_json::to_string(d).ok())
        .filter(|s| s != "null" && s != "[]");

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
                quota_model_details      = ?10 \
             WHERE id = ?11",
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
