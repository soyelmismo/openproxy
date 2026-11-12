//! Credit orders: a checkout that reserves a unique micro-USDC amount.
//!
//! Uniqueness among live orders is enforced by
//! `idx_credit_orders_live_amount` (partial unique index). [`create_order`]
//! draws a jitter, inserts, and retries on constraint violation — the DB is
//! the arbiter, there is no in-process reservation state to race on.

use crate::error::Result;
use openproxy_types::ids::ApiKeyId;
use rand::RngExt;
use rusqlite::{Connection, ErrorCode, OptionalExtension, params};
use serde::Serialize;

/// Payment window of an order: how long the quoted amount stays valid.
pub const PAYMENT_WINDOW_SECS: i64 = 15 * 60;

/// Jitter stays in the SUB-CENT range: 0..9999 micro-USDC (< $0.01 on top of
/// the plan price). Ten thousand distinct values per base price is plenty to
/// identify an order: live orders share the 15-minute payment window, so two
/// buyers of the same plan rarely race, and the unique live-amount index
/// resolves the rare collision in ~1 retry.
const JITTER_SPACE: i64 = 10_000;

/// Attempts to find a free jitter before giving up. With the live-order
/// index the expected number of attempts is ~1 for any realistic backlog
/// (a full space needs 1M *concurrent* live orders).
const MAX_JITTER_ATTEMPTS: usize = 64;

/// One checkout. `amount_units` is what the buyer must send exactly;
/// `base_units` is what gets credited (amount minus jitter). A package
/// payment may credit more than it charges (leveraged credit): `credit_units`
/// carries the credited amount when it differs from `base_units`.
#[derive(Debug, Clone, Serialize)]
pub struct CreditOrder {
    pub id: i64,
    pub order_code: String,
    pub api_key_id: Option<i64>,
    pub payer_email: Option<String>,
    pub amount_units: i64,
    pub base_units: i64,
    pub credit_units: i64,
    pub ttl_days: i64,
    pub status: String,
    pub tx_hash: Option<String>,
    pub block_number: Option<i64>,
    pub key_revealed: bool,
    pub created_at: String,
    pub expires_at: String,
    pub paid_at: Option<String>,
}

/// Alphabet for `OP-XXXX-XXXX` order codes (no ambiguous glyphs: no 0/O/1/I).
const CODE_ALPHABET: &[u8] = concat!("ABCDEFGH", "JKLMNPQR", "STUVWXYZ", "23456789").as_bytes();

fn gen_order_code() -> String {
    let mut rng = rand::rng();
    let mut buf = [b'A'; 8];
    for b in &mut buf {
        *b = CODE_ALPHABET[rng.random_range(0..CODE_ALPHABET.len())];
    }
    let (a, b) = buf.split_at(4);
    format!("OP-{}-{}", String::from_utf8_lossy(a), String::from_utf8_lossy(b))
}

fn row_to_order(row: &rusqlite::Row<'_>) -> rusqlite::Result<CreditOrder> {
    let base_units: i64 = row.get(5)?;
    Ok(CreditOrder {
        id: row.get(0)?,
        order_code: row.get(1)?,
        api_key_id: row.get(2)?,
        payer_email: row.get(3)?,
        amount_units: row.get(4)?,
        base_units,
        credit_units: row.get::<_, Option<i64>>(14)?.unwrap_or(base_units),
        ttl_days: row.get(6)?,
        status: row.get(7)?,
        tx_hash: row.get(8)?,
        block_number: row.get(9)?,
        key_revealed: row.get::<_, i64>(10)? != 0,
        created_at: row.get(11)?,
        expires_at: row.get(12)?,
        paid_at: row.get(13)?,
    })
}

const ORDER_COLS: &str = "id, order_code, api_key_id, payer_email, amount_units, base_units, \
     ttl_days, status, tx_hash, block_number, key_revealed, created_at, expires_at, paid_at, \
     credit_units";

fn is_unique_violation(err: &rusqlite::Error) -> bool {
    matches!(err.sqlite_error_code(), Some(ErrorCode::ConstraintViolation))
}

/// Create a checkout order. `base_units` is the plan price in micro-USDC;
/// the drawn jitter (sub-cent) rides on top only to identify the order.
///
/// # Errors
/// - validation: non-whole-dollar `base_units`, non-positive `ttl_days`.
/// - database: exhausted jitter space or an unexpected DB failure.
pub fn create_order(
    conn: &Connection,
    api_key_id: Option<ApiKeyId>,
    payer_email: Option<&str>,
    base_units: i64,
    ttl_days: i64,
    tier_id: Option<i64>,
) -> Result<CreditOrder> {
    // Plan prices may carry cents: amounts stay unique because matching is
    // exact per order and the live-order index enforces one order per amount.
    // Free-amount checkouts (no tier) must be whole USD (the jitter owns the
    // cents).
    let whole_ok = base_units % crate::credits::UNITS_PER_USD == 0;
    if base_units <= 0 || (!whole_ok && tier_id.is_none()) {
        return Err(crate::error::CoreError::Validation(format!(
            "base amount must be a whole number of USD in micro units, got {base_units}"
        )));
    }
    if ttl_days <= 0 {
        return Err(crate::error::CoreError::Validation(
            "ttl_days must be positive".into(),
        ));
    }

    let mut last_err: Option<rusqlite::Error> = None;
    for _ in 0..MAX_JITTER_ATTEMPTS {
        let jitter = rand::rng().random_range(0..JITTER_SPACE);
        let amount_units = base_units + jitter;
        // A leveraged plan credits more than it charges; the tier row is the
        // single source of truth for both sides of the funnel.
        let credit_units = match tier_id {
            Some(tid) => match crate::credits::tiers::get(conn, tid)? {
                Some(t) => t
                    .credit_usd
                    .and_then(|c| {
                        if c.is_finite() && c > 0.0 {
                            Some((c * f64::from(crate::credits::UNITS_PER_USD as u32)).round() as i64)
                        } else {
                            None
                        }
                    })
                    .unwrap_or(base_units),
                None => base_units,
            },
            None => base_units,
        };
        let code = gen_order_code();
        let res = conn.execute(
            "INSERT INTO credit_orders \
                 (order_code, api_key_id, payer_email, amount_units, base_units, ttl_days, \
                  status, expires_at, tier_id, credit_units) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', \
                     datetime('now', ?7), ?8, ?9)",
            params![
                code,
                api_key_id.map(|k| k.0),
                payer_email,
                amount_units,
                base_units,
                ttl_days,
                format!("+{PAYMENT_WINDOW_SECS} seconds"),
                tier_id,
                credit_units,
            ],
        );
        match res {
            Ok(_) => {
                let id = conn.last_insert_rowid();
                let order = conn
                    .query_row(
                        &format!("SELECT {ORDER_COLS} FROM credit_orders WHERE id = ?1"),
                        params![id],
                        row_to_order,
                    )
                    .optional()
                    .map_err(openproxy_db::error::map_db_error)?
                    .ok_or_else(|| {
                        crate::error::CoreError::Internal("credit order vanished after insert".into())
                    })?;
                return Ok(order);
            }
            // Jitter collision with a live order (or a code collision): retry.
            Err(e) if is_unique_violation(&e) => {
                last_err = Some(e);
                continue;
            }
            Err(e) => return Err(openproxy_db::error::map_db_error(e)),
        }
    }
    Err(crate::error::CoreError::Internal(format!(
        "could not allocate a unique order amount after {MAX_JITTER_ATTEMPTS} attempts: {}",
        last_err.map(|e| e.to_string()).unwrap_or_default()
    )))
}

/// Fetch one order by its public code.
///
/// # Errors
/// Propagates DB failures.
pub fn get_order(conn: &Connection, order_code: &str) -> Result<Option<CreditOrder>> {
    conn.query_row(
        &format!("SELECT {ORDER_COLS} FROM credit_orders WHERE order_code = ?1"),
        params![order_code],
        row_to_order,
    )
    .optional()
    .map_err(openproxy_db::error::map_db_error)
    .map_err(Into::into)
}

/// Expire payment-window-dead orders: `pending` past `expires_at` becomes
/// `expired`. Returns the number of rows transitioned.
///
/// # Errors
/// Propagates DB failures.
pub fn expire_stale_orders(conn: &Connection) -> Result<usize> {
    conn.execute(
        "UPDATE credit_orders SET status = 'expired' \
         WHERE status = 'pending' AND expires_at <= datetime('now')",
        [],
    )
    .map_err(openproxy_db::error::map_db_error)
    .map_err(Into::into)
}
