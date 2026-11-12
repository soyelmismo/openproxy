//! The credit ledger: top-up lots with individual expiry, FIFO consumption,
//! balance queries and the audit trail.
//!
//! Invariant: `balance_units(key) == SUM(remaining_units)` over rows with
//! `status='active' AND (expires_at IS NULL OR expires_at > now)`. Spend
//! decrements lots FIFO (soonest expiry first) inside one immediate
//! transaction; a lot whose remaining hits zero becomes `exhausted`, a lot
//! whose expiry passes becomes `expired` and its remaining stays recorded as
//! the amount that died (no refunds — the expiry is advertised upfront).

use crate::error::Result;
use openproxy_types::ids::ApiKeyId;
use rusqlite::{Connection, params};
use serde::Serialize;

use crate::credits::UNITS_PER_USD;

/// One ledger entry. For `topup` rows `remaining_units` tracks what is left
/// of the deposit; for `expire` rows it records how much died.
#[derive(Debug, Clone, Serialize)]
pub struct TxRow {
    pub id: i64,
    pub api_key_id: i64,
    pub order_id: Option<i64>,
    pub kind: String,
    pub amount_units: i64,
    pub remaining_units: i64,
    pub tx_hash: Option<String>,
    pub status: String,
    pub created_at: String,
    pub expires_at: Option<String>,
}

fn row_to_tx(row: &rusqlite::Row<'_>) -> rusqlite::Result<TxRow> {
    Ok(TxRow {
        id: row.get(0)?,
        api_key_id: row.get(1)?,
        order_id: row.get(2)?,
        kind: row.get(3)?,
        amount_units: row.get(4)?,
        remaining_units: row.get(5)?,
        tx_hash: row.get(6)?,
        status: row.get(7)?,
        created_at: row.get(8)?,
        expires_at: row.get(9)?,
    })
}

/// A top-up lot about to die.
#[derive(Debug, Clone, Serialize)]
pub struct ExpiringLot {
    pub amount_usd: f64,
    pub remaining_usd: f64,
    pub expires_at: String,
}

/// Balance summary for the portal.
#[derive(Debug, Clone, Serialize)]
pub struct BalanceView {
    pub credit_based: bool,
    pub balance_usd: f64,
    /// Active lots ordered by expiry (soonest first).
    pub lots: Vec<ExpiringLot>,
}

/// Result of a FIFO spend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpendOutcome {
    /// Units actually consumed (== requested when the balance sufficed).
    pub spent_units: i64,
}

/// Whether the key carries at least one ledger entry, i.e. is governed by
/// the prepaid balance instead of being free/unlimited.
///
/// # Errors
/// Propagates DB failures.
pub fn is_credit_based(conn: &Connection, key_id: ApiKeyId) -> Result<bool> {
    openproxy_db::credits_ledger::is_credit_based(conn, key_id.0).map_err(Into::into)
}

/// Effective balance in micro-USDC units.
///
/// # Errors
/// Propagates DB failures.
pub fn balance_units(conn: &Connection, key_id: ApiKeyId) -> Result<i64> {
    openproxy_db::credits_ledger::balance_units(conn, key_id.0).map_err(Into::into)
}

/// Balance view for the portal.
///
/// # Errors
/// Propagates DB failures.
pub fn balance_view(conn: &Connection, key_id: ApiKeyId) -> Result<BalanceView> {
    let credit_based = is_credit_based(conn, key_id)?;
    let units = balance_units(conn, key_id)?;
    let mut stmt = conn
        .prepare(
            "SELECT amount_units, remaining_units, expires_at FROM credit_transactions \
             WHERE api_key_id = ?1 AND status = 'active' AND kind = 'topup' \
               AND expires_at IS NOT NULL AND expires_at > datetime('now') \
             ORDER BY expires_at ASC, id ASC",
        )
        .map_err(openproxy_db::error::map_db_error)?;
    let lots = stmt
        .query_map(params![key_id.0], |row| {
            Ok(ExpiringLot {
                amount_usd: row.get::<_, i64>(0)? as f64 / UNITS_PER_USD as f64,
                remaining_usd: row.get::<_, i64>(1)? as f64 / UNITS_PER_USD as f64,
                expires_at: row.get(2)?,
            })
        })
        .map_err(openproxy_db::error::map_db_error)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(openproxy_db::error::map_db_error)?;
    Ok(BalanceView {
        credit_based,
        balance_usd: units as f64 / UNITS_PER_USD as f64,
        lots,
    })
}

/// Credit a paid top-up as an active lot.
///
/// # Errors
/// Propagates DB failures.
pub fn credit_topup(
    conn: &Connection,
    key_id: ApiKeyId,
    order_id: i64,
    base_units: i64,
    ttl_days: i64,
    tx_hash: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO credit_transactions \
             (api_key_id, order_id, kind, amount_units, remaining_units, tx_hash, status, \
              expires_at) \
         VALUES (?1, ?2, 'topup', ?3, ?3, ?4, 'active', \
                 datetime('now', ?5))",
        params![
            key_id.0,
            order_id,
            base_units,
            tx_hash,
            format!("+{ttl_days} days"),
        ],
    )
    .map_err(openproxy_db::error::map_db_error)?;
    Ok(conn.last_insert_rowid())
}

/// Consume `cost_units` from the key's balance, FIFO by expiry.
///
/// Runs inside one immediate transaction: the lot decrements and the status
/// transitions are atomic, and an overdraw rolls back entirely (the caller
/// is expected to have pre-flighted, but the ledger never goes negative).
///
/// # Errors
/// - [`openproxy_types::CoreError::InsufficientCredits`] when the effective
///   balance cannot cover the cost (nothing is consumed).
/// - Propagates DB failures.
pub fn record_spend(conn: &Connection, key_id: ApiKeyId, cost_units: i64) -> Result<SpendOutcome> {
    openproxy_db::credits_ledger::record_spend(conn, key_id.0, cost_units)
        .map(|spent| SpendOutcome { spent_units: spent })
}

/// Kill expired lots: `active` rows past their `expires_at` become `expired`
/// (their `remaining_units` is left as the historical record of what died).
/// Returns the number of rows transitioned.
///
/// # Errors
/// Propagates DB failures.
pub fn expire_due(conn: &Connection) -> Result<usize> {
    openproxy_db::credits_ledger::expire_due(conn).map_err(Into::into)
}

/// Ledger history for one key, newest first.
///
/// # Errors
/// Propagates DB failures.
pub fn history(conn: &Connection, key_id: ApiKeyId, limit: u32) -> Result<Vec<TxRow>> {
    let limit = limit.clamp(1, 500) as i64;
    let mut stmt = conn
        .prepare(&format!(
            "SELECT id, api_key_id, order_id, kind, amount_units, remaining_units, \
                    tx_hash, status, created_at, expires_at \
             FROM credit_transactions WHERE api_key_id = ?1 \
             ORDER BY id DESC LIMIT {limit}"
        ))
        .map_err(openproxy_db::error::map_db_error)?;
    let rows = stmt
        .query_map(params![key_id.0], row_to_tx)
        .map_err(openproxy_db::error::map_db_error)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(openproxy_db::error::map_db_error)?;
    Ok(rows)
}
