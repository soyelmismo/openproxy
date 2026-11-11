//! Prepaid credits funded by on-chain USDC top-ups (Base L2).
//!
//! Thin-funnel design: every top-up is sent to ONE collector address. Each
//! [`orders::CreditOrder`] reserves a unique micro-USDC amount (a base amount
//! plus a jitter in the low six decimals) that acts as its identifier on a
//! shared address, so a single wallet can serve every buyer without
//! sub-addresses or memos.
//!
//! Concurrency model — the database is the only arbiter:
//!
//! - **Order creation** relies on a partial unique index over the amounts of
//!   live orders (`status IN ('pending','paid')`). Two concurrent creations
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

pub mod ledger;
pub mod matcher;
pub mod orders;

pub use ledger::{BalanceView, ExpiringLot, SpendOutcome, TxRow};
pub mod packages;
pub use matcher::{PaymentOutcome, apply_payment};
pub use packages::{CreditPackage, PackageConfig};
pub use orders::{CreditOrder, create_order, expire_stale_orders, get_order};

/// Micro-USDC units in one whole USD.
pub const UNITS_PER_USD: i64 = 1_000_000;

/// Convert whole USD (f64, API surface) into micro-USDC units, rejecting
/// non-finite input. The jitter needs the six decimals below the unit point,
/// so base amounts are whole dollars.
#[must_use]
pub fn usd_to_base_units(usd: f64) -> Option<i64> {
    if !usd.is_finite() || usd <= 0.0 {
        return None;
    }
    let whole = usd.floor() as i64;
    if (usd - whole as f64).abs() > f64::EPSILON || whole <= 0 {
        None
    } else {
        Some(whole * UNITS_PER_USD)
    }
}

/// Render micro-USDC units as a USD string with six decimals.
#[must_use]
pub fn units_to_usd_str(units: i64) -> String {
    let sign = if units < 0 { "-" } else { "" };
    let abs = units.abs();
    format!("{sign}{}.{:06}", abs / UNITS_PER_USD, abs % UNITS_PER_USD)
}

#[cfg(test)]
mod tests;
