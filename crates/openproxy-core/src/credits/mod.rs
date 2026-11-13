//! Prepaid credits funded by on-chain USDC top-ups (Base L2).
//!
//! Thin-funnel design: every top-up is sent to ONE collector address. Each
//! [`orders::CreditOrder`] reserves a unique micro-USDC amount (a base amount
//! plus a sub-cent jitter) that acts as its identifier on a
//! shared address, so a single wallet can serve every buyer without
//! sub-addresses or memos.
//!
//! Concurrency model — the database is the only arbiter:
//!
//! - **Order creation** relies on a partial unique index over the amounts of
//!   live orders (`status = 'pending'`) and a transactional reservation through
//!   the confirmation grace period. Two concurrent creations
//!   cannot advertise the same amount; the loser retries with a new jitter.
//! - **Payment matching** is idempotent ([`matcher`]) and uses a
//!   compare-and-swap UPDATE: `SET status='paid' WHERE status='pending'`.
//!   No matter how many times a log is delivered or replayed, exactly one
//!   call wins the CAS and credits the balance.
//! - **Consumption** decrements top-up lots FIFO by expiry inside a single
//!   immediate transaction ([`ledger`]), so overspend is impossible.
//!
//! All amounts are integer micro-USDC (`1e-6 USD`) — no floats inside the
//! ledger. Conversion happens at the API surface only.

mod delivery;
pub mod ledger;
pub mod matcher;
pub mod orders;
mod plan_grants;
pub mod sweep;
pub mod tiers;

pub use ledger::{BalanceView, ExpiringLot, SpendOutcome, TxRow};
pub use matcher::{
    PaymentOutcome, acknowledge_minted_key, apply_payment, reveal_minted_key_with_receipt,
};
pub use orders::{
    CreateOrderRequest, CreditOrder, create_order, create_order_with_receipt,
    create_order_with_window, expire_stale_orders, get_order,
};
pub use tiers::{CreditTier, PlanCommerce, TierConfig};

/// Run a credits mutation atomically, joining an explicit caller transaction.
pub(crate) fn transaction<T>(
    conn: &rusqlite::Connection,
    operation: impl FnOnce(&rusqlite::Connection) -> crate::error::Result<T>,
) -> crate::error::Result<T> {
    let owned = if conn.is_autocommit() {
        Some(
            rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)
                .map_err(openproxy_db::error::map_db_error)?,
        )
    } else {
        None
    };
    let result = operation(conn)?;
    if let Some(tx) = owned {
        tx.commit().map_err(openproxy_db::error::map_db_error)?;
    }
    Ok(result)
}

/// Micro-USDC units in one whole USD.
pub const UNITS_PER_USD: i64 = 1_000_000;

/// Convert positive USD to exact ledger units with checked range and precision.
/// Reject sub-micro amounts, overflow and prices that silently round to zero.
#[must_use]
pub fn usd_to_units(usd: f64) -> Option<i64> {
    let scaled = usd * UNITS_PER_USD as f64;
    if !usd.is_finite() || scaled < 1.0 || scaled >= i64::MAX as f64 {
        return None;
    }
    let rounded = scaled.round();
    if (scaled - rounded).abs() > 0.000_001 {
        return None;
    }
    Some(rounded as i64)
}

/// Convert whole USD into micro-USDC without overflowing.
#[must_use]
pub fn usd_to_base_units(usd: f64) -> Option<i64> {
    if usd.fract() != 0.0 {
        return None;
    }
    usd_to_units(usd)
}

/// Render micro-USDC units as a USD string with six decimals.
#[must_use]
pub fn units_to_usd_str(units: i64) -> String {
    let sign = if units < 0 { "-" } else { "" };
    let abs = units.unsigned_abs();
    let scale = UNITS_PER_USD as u64;
    format!("{sign}{}.{:06}", abs / scale, abs % scale)
}

#[cfg(test)]
mod order_replay_tests;
#[cfg(test)]
mod receipt_tests;
#[cfg(test)]
mod resilience_tests;
#[cfg(test)]
mod temporal_tests;
#[cfg(test)]
mod tests;
