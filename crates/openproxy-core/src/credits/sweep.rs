//! Maintenance sweep: fully delete API keys that have been idle for months.
//!
//! A key is idle when it has NO usage rows inside the idle window AND it
//! already existed when the window started — it had the whole window to be
//! used and was not. Idle keys with a positive balance are still swept: the
//! balance of a dead key should not sit on the books forever. The sweep is
//! free-amount-safe: `manage`-scoped operator keys are never swept.

use crate::error::Result;
use openproxy_db::error::map_db_error;
use openproxy_types::ids::ApiKeyId;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

/// Months of silence after which a key is swept.
pub const SWEEP_IDLE_MONTHS: i64 = 6;

#[derive(Debug, Serialize)]
pub struct IdleKey {
    pub id: i64,
    pub label: Option<String>,
    /// Remaining credit balance in units at sweep time (reporting only).
    pub balance_units: i64,
}

/// Window start for the idle check, as a SQLite modifier string.
fn idle_window() -> String {
    format!("-{SWEEP_IDLE_MONTHS} months")
}

/// Keys idle for `SWEEP_IDLE_MONTHS`, oldest first. Operator keys
/// (`manage` scope) and keys created inside the window are excluded.
pub fn find_idle_keys(conn: &Connection) -> Result<Vec<IdleKey>> {
    let window = idle_window();
    let mut stmt = conn
        .prepare(
            "WITH idle AS (
               SELECT k.id, k.label
               FROM api_keys k
               WHERE k.created_at <= datetime('now', ?1)
                 AND NOT EXISTS (
                   SELECT 1 FROM usage u
                   WHERE u.api_key_id = k.id AND u.created_at > datetime('now', ?1)
                 )
                 AND NOT EXISTS (
                   SELECT 1 FROM json_each(k.scopes_json) je WHERE je.value = 'manage'
                 )
             )
             SELECT idle.id, idle.label,
                    COALESCE(SUM(ct.amount_units), 0)
             FROM idle
             LEFT JOIN credit_transactions ct ON ct.api_key_id = idle.id
             GROUP BY idle.id
             ORDER BY idle.id",
        )
        .map_err(|e| map_db_error(e))?;
    let rows = stmt
        .query_map(params![window], |r| {
            Ok(IdleKey {
                id: r.get(0)?,
                label: r.get(1)?,
                balance_units: r.get(2)?,
            })
        })
        .map_err(|e| map_db_error(e))?
        .collect::<std::result::Result<Vec<_>, rusqlite::Error>>()
        .map_err(map_db_error)?;
    Ok(rows)
}

/// Fully delete every idle key (up to `limit` per pass, oldest first).
/// Returns what was swept. Deleting the key cascades the quota row; the
/// credit ledger rows are removed so no dead balance stays on the books;
/// usage rows survive with `api_key_id = NULL` (analytics history).
pub fn sweep_idle_keys(
    conn: &Connection,
    limit: usize,
) -> Result<Vec<IdleKey>> {
    let candidates = find_idle_keys(conn)?;
    let mut swept = Vec::new();
    for key in candidates.into_iter().take(limit) {
        conn.execute(
            "DELETE FROM credit_transactions WHERE api_key_id = ?1",
            params![key.id],
        )
        .map_err(|e| map_db_error(e))?;
        crate::api_keys::hard_delete(conn, ApiKeyId(key.id))?;
        swept.push(key);
    }
    Ok(swept)
}

/// True when the key would be swept right now (used by tests and tooling).
pub fn is_idle(conn: &Connection, key_id: i64) -> Result<bool> {
    let window = idle_window();
    conn.query_row(
        "SELECT EXISTS (
           SELECT 1 FROM api_keys k
           WHERE k.id = ?1
             AND k.created_at <= datetime('now', ?2)
             AND NOT EXISTS (
               SELECT 1 FROM usage u
               WHERE u.api_key_id = k.id AND u.created_at > datetime('now', ?2)
             )
             AND NOT EXISTS (
               SELECT 1 FROM json_each(k.scopes_json) je WHERE je.value = 'manage'
             )
         )",
        params![key_id, window],
        |r| r.get::<_, i64>(0),
    )
    .optional()
    .map(|v| v.map(|n| n != 0).unwrap_or(false))
    .map_err(|e| map_db_error(e))
}
