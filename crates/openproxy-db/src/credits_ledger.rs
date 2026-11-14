//! Prepaid credits ledger (SQL layer).
//!
//! Single source of truth for balance arithmetic, shared by the payment
//! matcher (`openproxy-core::credits`) and the spend hook
//! (`db::cost::mark_client_response`). All amounts are integer micro-USDC
//! (`1e-6 USD`) — no floats inside the ledger.
//!
//! Balance invariant: `balance(key) == SUM(remaining_units)` over rows with
//! `status='active' AND (expires_at IS NULL OR expires_at > now)`. Spend
//! decrements top-up lots FIFO by expiry inside one immediate transaction;
//! a lot drained to zero becomes `exhausted`, a lot past its expiry becomes
//! `expired` with its remaining recorded as what died (no refunds).

use openproxy_types::CoreError;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

/// Effective balance in micro-USDC units.
///
/// # Errors
/// Propagates DB failures.
pub fn balance_units(conn: &Connection, api_key_id: i64) -> openproxy_types::Result<i64> {
    let sum: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(remaining_units), 0) FROM credit_transactions \
             WHERE api_key_id = ?1 AND status = 'active' AND kind IN ('topup', 'adjust', 'grant') \
               AND (expires_at IS NULL OR expires_at > datetime('now'))",
            params![api_key_id],
            |row| row.get(0),
        )
        .map_err(crate::error::map_db_error)?;
    Ok(sum)
}

/// Whether the key carries at least one ledger entry, i.e. is governed by
/// the prepaid balance instead of being free/unlimited.
///
/// # Errors
/// Propagates DB failures.
pub fn is_credit_based(conn: &Connection, api_key_id: i64) -> openproxy_types::Result<bool> {
    let exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM credit_transactions WHERE api_key_id = ?1)",
            params![api_key_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(crate::error::map_db_error)?
        .unwrap_or(false);
    Ok(exists)
}

/// Consume `cost_units` from the key's balance, FIFO by expiry.
///
/// Runs inside one immediate transaction: the lot decrements and the status
/// transitions are atomic, and an overdraw rolls back entirely.
///
/// # Errors
/// - [`CoreError::InsufficientCredits`] when the effective balance cannot
///   cover the cost (nothing is consumed).
/// - Propagates DB failures.
pub fn record_spend(
    conn: &Connection,
    api_key_id: i64,
    cost_units: i64,
) -> openproxy_types::Result<i64> {
    if cost_units <= 0 {
        return Ok(0);
    }
    // The caller may already hold a transaction (the usage-marking path runs
    // inside one). Join it instead of opening a nested one: BEGIN inside a
    // live transaction fails outright ("cannot start a transaction within a
    // transaction"), which would silently drop every spend record.
    let owned_tx = if conn.is_autocommit() {
        Some(
            rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)
                .map_err(crate::error::map_db_error)?,
        )
    } else {
        None
    };
    let tx: &Connection = owned_tx.as_deref().unwrap_or(conn);
    let balance = balance_units(tx, api_key_id)?;
    // LAST-DRAW: charge whatever the plan still holds instead of failing. The
    // gate admits requests while the balance is positive, so a request whose
    // real cost exceeds the balance pays with the remainder and the plan
    // closes at exactly 0; the middleware refuses everything after that. A
    // concurrent sibling may have drained it first: charge nothing, close.
    let cost_units = cost_units.min(balance.max(0));
    if cost_units <= 0 {
        return Ok(0);
    }
    let mut need = cost_units;
    {
        let mut stmt = tx
            .prepare(
                "SELECT id, remaining_units FROM credit_transactions \
                 WHERE api_key_id = ?1 AND status = 'active' AND kind IN ('topup', 'adjust', 'grant') \
                   AND remaining_units > 0 \
                   AND (expires_at IS NULL OR expires_at > datetime('now')) \
                 ORDER BY (expires_at IS NULL), expires_at ASC, id ASC",
            )
            .map_err(crate::error::map_db_error)?;
        let lots: Vec<(i64, i64)> = stmt
            .query_map(params![api_key_id], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(crate::error::map_db_error)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(crate::error::map_db_error)?;
        drop(stmt);
        for (id, remaining) in lots {
            if need == 0 {
                break;
            }
            let take = remaining.min(need);
            let new_remaining = remaining - take;
            let new_status = if new_remaining == 0 {
                "exhausted"
            } else {
                "active"
            };
            tx.execute(
                "UPDATE credit_transactions SET remaining_units = ?1, status = ?2 WHERE id = ?3",
                params![new_remaining, new_status, id],
            )
            .map_err(crate::error::map_db_error)?;
            need -= take;
        }
    }
    if need > 0 {
        // Defensive: the pre-check passed but lots moved underneath. Roll
        // everything back rather than credit a negative balance.
        return Err(CoreError::InsufficientCredits {
            balance_usd: balance as f64 / 1_000_000.0,
        });
    }
    if let Some(t) = owned_tx {
        t.commit().map_err(crate::error::map_db_error)?;
    }
    Ok(cost_units)
}

/// Kill expired lots: `active` rows past their `expires_at` become `expired`
/// (their `remaining_units` is left as the historical record of what died).
/// Returns the number of rows transitioned.
///
/// # Errors
/// Propagates DB failures.
pub fn expire_due(conn: &Connection) -> openproxy_types::Result<usize> {
    conn.execute(
        "UPDATE credit_transactions SET status = 'expired' \
         WHERE status = 'active' AND expires_at IS NOT NULL \
           AND expires_at <= datetime('now')",
        [],
    )
    .map_err(crate::error::map_db_error)
}

/// Last block fully scanned by the watcher.
///
/// # Errors
/// Propagates DB failures.
pub fn last_scanned_block(conn: &Connection) -> openproxy_types::Result<i64> {
    let block: i64 = conn
        .query_row(
            "SELECT last_scanned_block FROM credits_watcher_state WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .map_err(crate::error::map_db_error)?;
    Ok(block)
}

/// Persist the last scanned block.
///
/// # Errors
/// Propagates DB failures.
pub fn set_last_scanned_block(conn: &Connection, block: i64) -> openproxy_types::Result<()> {
    conn.execute(
        "UPDATE credits_watcher_state SET last_scanned_block = ?1, updated_at = datetime('now')          WHERE id = 1",
        params![block],
    )
    .map_err(crate::error::map_db_error)?;
    Ok(())
}

/// Result of the periodic ledger self-audit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileReport {
    pub paid_orders: i64,
    pub topup_rows: i64,
    /// Sum of promised credit over paid/minted orders (micro-USDC).
    pub credited_expected_units: i64,
    /// Sum of `amount_units` over topup ledger rows (micro-USDC).
    pub credited_actual_units: i64,
    /// Orders marked paid/minted without a ledger row, or ledger rows
    /// without a paid/minted order. Non-zero means corruption: a credit
    /// appeared or vanished outside the matcher's transaction.
    pub orphans: i64,
}

impl ReconcileReport {
    #[must_use]
    pub fn is_consistent(&self) -> bool {
        self.orphans == 0
            && self.paid_orders == self.topup_rows
            && self.credited_expected_units == self.credited_actual_units
    }
}

/// Verify the ledger invariant: every paid/minted order maps to exactly one
/// topup ledger row and the credited totals match to the micro-unit.
/// Called periodically by the expiry service; any mismatch is a CRITICAL
/// (money appeared or vanished outside the matcher's CAS transaction).
///
/// # Errors
/// Propagates DB failures.
pub fn reconcile(conn: &Connection) -> openproxy_types::Result<ReconcileReport> {
    let (paid_orders, credited_expected): (i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(COALESCE(credit_units, base_units)), 0) FROM credit_orders              WHERE status IN ('paid', 'minted')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(crate::error::map_db_error)?;
    let (topup_rows, credited_actual): (i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(amount_units), 0) FROM credit_transactions              WHERE kind = 'topup'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(crate::error::map_db_error)?;
    let orphan_orders: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM credit_orders o              WHERE o.status IN ('paid', 'minted')                AND (SELECT COUNT(*) FROM credit_transactions t                     WHERE t.order_id = o.id AND t.kind = 'topup') != 1",
            [],
            |row| row.get(0),
        )
        .map_err(crate::error::map_db_error)?;
    let orphan_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM credit_transactions t \
             LEFT JOIN credit_orders o ON o.id = t.order_id \
             WHERE t.kind = 'topup' AND (o.id IS NULL OR o.status NOT IN ('paid', 'minted') \
               OR t.amount_units != COALESCE(o.credit_units, o.base_units) \
               OR t.api_key_id IS NOT o.api_key_id OR t.remaining_units < 0 \
               OR t.remaining_units > t.amount_units)",
            [],
            |row| row.get(0),
        )
        .map_err(crate::error::map_db_error)?;
    Ok(ReconcileReport {
        paid_orders,
        topup_rows,
        credited_expected_units: credited_expected,
        credited_actual_units: credited_actual,
        orphans: orphan_orders + orphan_rows,
    })
}

/// One row of the buyer-facing transaction history.
#[derive(Debug, Clone, Serialize)]
pub struct CreditTxRow {
    pub kind: String,
    pub amount_units: i64,
    pub remaining_units: i64,
    pub tx_hash: Option<String>,
    pub status: String,
    pub order_code: Option<String>,
    pub created_at: String,
}

/// Transaction history for one key, newest first. `tx_hash` is the pointer
/// into the Base chain explorer; `order_code` links a topup back to its
/// checkout order.
///
/// # Errors
/// Propagates DB failures.
pub fn list_transactions(
    conn: &Connection,
    api_key_id: i64,
    limit: i64,
) -> openproxy_types::Result<Vec<CreditTxRow>> {
    let mut stmt = conn
        .prepare(
            "SELECT t.kind, t.amount_units, t.remaining_units, t.tx_hash, t.status, \
             o.order_code, t.created_at \
             FROM credit_transactions t LEFT JOIN credit_orders o ON o.id = t.order_id \
             WHERE t.api_key_id = ?1 ORDER BY t.created_at DESC, t.id DESC LIMIT ?2",
        )
        .map_err(crate::error::map_db_error)?;
    let rows = stmt
        .query_map(rusqlite::params![api_key_id, limit], |row| {
            Ok(CreditTxRow {
                kind: row.get(0)?,
                amount_units: row.get(1)?,
                remaining_units: row.get(2)?,
                tx_hash: row.get(3)?,
                status: row.get(4)?,
                order_code: row.get(5)?,
                created_at: row.get(6)?,
            })
        })
        .map_err(crate::error::map_db_error)?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(crate::error::map_db_error)
}

/// One active credit lot (a top-up with its own expiry).
#[derive(Debug, Clone, Serialize)]
pub struct CreditLot {
    pub remaining_units: i64,
    pub expires_at: Option<String>,
    pub created_at: String,
}

/// Active lots for one key, soonest expiry first (FIFO consumption order).
///
/// # Errors
/// Propagates DB failures.
pub fn active_lots(conn: &Connection, api_key_id: i64) -> openproxy_types::Result<Vec<CreditLot>> {
    let mut stmt = conn
        .prepare(
            "SELECT remaining_units, expires_at, created_at FROM credit_transactions \
             WHERE api_key_id = ?1 AND kind IN ('topup', 'adjust', 'grant') AND status = 'active' \
               AND (expires_at IS NULL OR expires_at > datetime('now')) \
             ORDER BY (expires_at IS NULL), expires_at ASC, id ASC",
        )
        .map_err(crate::error::map_db_error)?;
    let rows = stmt
        .query_map(rusqlite::params![api_key_id], |row| {
            Ok(CreditLot {
                remaining_units: row.get(0)?,
                expires_at: row.get(1)?,
                created_at: row.get(2)?,
            })
        })
        .map_err(crate::error::map_db_error)?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(crate::error::map_db_error)
}
