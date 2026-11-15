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

/// Distinct sold identities: every `customer` key is one registration
/// (checkout-created or plan-granted). The registration quota
/// (`credits_registration_quota` in `app_config`) compares against this
/// count — it is the "registrados" figure the dashboard shows.
///
/// # Errors
/// Propagates DB failures.
pub fn count_customer_keys(conn: &Connection) -> openproxy_types::Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM api_keys WHERE kind = 'customer'",
        [],
        |row| row.get(0),
    )
    .map_err(crate::error::map_db_error)
}

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
    /// ROOT FIX (blind spot, audit A6-05): `balance_units` also sums
    /// `adjust` and `grant` rows, but the audit only looked at `topup`
    /// rows — an operator-granted (or directly injected) balance was
    /// invisible to the self-audit. The totals are now part of the report
    /// so any adjust/grant activity is visible in the periodic CRITICAL
    /// check instead of silently passing as 'consistent'.
    pub adjust_rows: i64,
    pub adjust_units: i64,
    pub grant_rows: i64,
    pub grant_units: i64,
}

impl ReconcileReport {
    #[must_use]
    pub fn is_consistent(&self) -> bool {
        self.orphans == 0
            && self.paid_orders == self.topup_rows
            && self.credited_expected_units == self.credited_actual_units
    }
}

/// Credit actually owed for a payment, pro-rated to what was received.
///
/// Root fix for the fee-tolerance discount: the tolerance window exists to
/// RECOGNIZE a transfer short by an exchange withdrawal fee, not to grant
/// free credit. A short payment credits `credit × received / quoted`
/// (integer, truncating), an exact payment credits the full promise and an
/// overpayment never credits more than the plan promises.
///
/// All amounts are micro-USDC; the multiplication is done in `i128` so no
/// realistic ledger value can overflow.
#[must_use]
pub fn prorated_credit_units(credit_units: i64, received_units: i64, quoted_units: i64) -> i64 {
    let credit = credit_units.max(0);
    if quoted_units <= 0 {
        return credit;
    }
    let received = received_units.max(0);
    if received >= quoted_units {
        return credit;
    }
    let prorated = (i128::from(credit) * i128::from(received)) / i128::from(quoted_units);
    let prorated = i64::try_from(prorated).unwrap_or(i64::MAX);
    prorated.clamp(0, credit)
}

/// Verify the ledger invariant: every paid/minted order maps to exactly one
/// topup ledger row and the credited totals match to the micro-unit.
/// Called periodically by the expiry service; any mismatch is a CRITICAL
/// (money appeared or vanished outside the matcher's CAS transaction).
///
/// The expected credit of an order is the PRO-RATED amount the matcher owed
/// for the transfer that paid it (see [`prorated_credit_units`]): the order
/// row keeps the promised credit, while the ledger row stores what the buyer
/// actually received credit for. Legacy orders without a `matched_payments`
/// row are checked against the promised amount.
///
/// # Errors
/// Propagates DB failures.
pub fn reconcile(conn: &Connection) -> openproxy_types::Result<ReconcileReport> {
    let mut stmt = conn
        .prepare(
            "SELECT id, amount_units, COALESCE(credit_units, base_units), tx_hash, api_key_id \
             FROM credit_orders WHERE status IN ('paid', 'minted')",
        )
        .map_err(crate::error::map_db_error)?;
    type PaidOrder = (i64, i64, i64, Option<String>, Option<i64>);
    let orders: Vec<PaidOrder> = stmt
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .map_err(crate::error::map_db_error)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(crate::error::map_db_error)?;
    drop(stmt);

    let mut paid_orders: i64 = 0;
    let mut credited_expected: i64 = 0;
    let mut orphans: i64 = 0;
    for (order_id, quoted_units, promised_units, tx_hash, order_key_id) in orders {
        paid_orders += 1;
        // The received amount of the transfer that paid this order: prefer
        // the matcher's own linkage, fall back to the tx hash (legacy rows).
        let received: Option<i64> = conn
            .query_row(
                "SELECT amount_units FROM matched_payments \
                 WHERE order_id = ?1 OR (order_id IS NULL AND tx_hash = ?2) LIMIT 1",
                params![order_id, tx_hash],
                |row| row.get(0),
            )
            .optional()
            .map_err(crate::error::map_db_error)?;
        let expected = match received {
            Some(recv) => prorated_credit_units(promised_units, recv, quoted_units),
            None => promised_units,
        };
        credited_expected += expected;
        let row: Option<(i64, i64, Option<i64>)> = conn
            .query_row(
                "SELECT amount_units, remaining_units, api_key_id \
                 FROM credit_transactions WHERE order_id = ?1 AND kind = 'topup'",
                params![order_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(crate::error::map_db_error)?;
        match row {
            Some((amount, remaining, row_key_id)) => {
                if amount != expected
                    || row_key_id != order_key_id
                    || remaining < 0
                    || remaining > amount
                {
                    orphans += 1;
                }
            }
            None => orphans += 1,
        }
    }

    let (topup_rows, credited_actual): (i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(amount_units), 0) FROM credit_transactions              WHERE kind = 'topup'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(crate::error::map_db_error)?;
    let orphan_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM credit_transactions t \
             LEFT JOIN credit_orders o ON o.id = t.order_id \
             WHERE t.kind = 'topup' AND (o.id IS NULL OR o.status NOT IN ('paid', 'minted'))",
            [],
            |row| row.get(0),
        )
        .map_err(crate::error::map_db_error)?;
    let (adjust_rows, adjust_units): (i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(amount_units), 0) FROM credit_transactions \
             WHERE kind = 'adjust'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(crate::error::map_db_error)?;
    let (grant_rows, grant_units): (i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(amount_units), 0) FROM credit_transactions \
             WHERE kind = 'grant'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(crate::error::map_db_error)?;
    Ok(ReconcileReport {
        paid_orders,
        topup_rows,
        credited_expected_units: credited_expected,
        credited_actual_units: credited_actual,
        orphans: orphans + orphan_rows,
        adjust_rows,
        adjust_units,
        grant_rows,
        grant_units,
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
