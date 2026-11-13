//! Credit orders: a checkout that reserves a unique micro-USDC amount.
//!
//! Uniqueness among live orders is enforced by
//! `idx_credit_orders_live_amount` (partial unique index). [`create_order`]
//! draws a jitter, inserts, and retries on constraint violation — the DB is
//! the arbiter, there is no in-process reservation state to race on.
//!
//! Anonymous checkouts MUST bind a delivery receipt at creation
//! ([`create_order_with_receipt`]): the receipt hash is stored on the order
//! row and the minted key is only revealed to that same receipt later. A
//! code-only caller can therefore never claim a fresh key, and a leaked order
//! code is harmless without the receipt.

use crate::error::Result;
use openproxy_types::ids::ApiKeyId;
use rand::RngExt;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

/// Payment window of an order: how long the quoted amount stays valid.
pub const PAYMENT_WINDOW_SECS: i64 = 15 * 60;
/// A transfer included during this grace period still belongs to the order.
pub const CONFIRMATION_GRACE_SECS: i64 = 10 * 60;

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

/// URL-safe delivery receipts are 32..=128 ASCII `[A-Za-z0-9_-]`. UUIDs and
/// base64url tokens both satisfy this; a receipt is an unguessable capability
/// (128 bits of entropy minimum), never a human-readable label.
pub const MIN_RECEIPT_LEN: usize = 32;
pub const MAX_RECEIPT_LEN: usize = 128;

/// One checkout. `amount_units` is what the buyer must send exactly;
/// `base_units` is what gets credited (amount minus jitter). A package
/// payment may credit more than it charges (leveraged credit): `credit_units`
/// carries the credited amount when it differs from `base_units`.
#[derive(Debug, Clone, Serialize)]
pub struct CreditOrder {
    pub id: i64,
    pub order_code: String,
    pub api_key_id: Option<i64>,
    /// Original checkout target. Unlike api_key_id, never changes on mint.
    pub checkout_key_id: Option<i64>,
    pub payment_window_secs: i64,
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

/// Checkout target for [`create_order_with_receipt`]. Exactly one delivery
/// model: `api_key_id: None` mints a fresh anonymous key (receipt-bound), and
/// `Some(id)` tops up an existing key. The caller resolves tenant/auth
/// before constructing the request; this module only validates the money.
#[derive(Debug, Clone)]
pub struct CreateOrderRequest {
    pub api_key_id: Option<ApiKeyId>,
    pub payer_email: Option<String>,
    /// Whole-USD price in micro-units (ignored when `tier_id` resolves the
    /// commerce: the tier's own price wins, as in `create_order_with_window`).
    pub base_units: i64,
    /// Ignored when `tier_id` is set: the tier's TTL wins.
    pub ttl_days: i64,
    pub tier_id: Option<i64>,
}

/// Validate a delivery receipt and return its stored hash. The same helper is
/// used at creation, reveal and acknowledgement so the three never disagree.
///
/// # Errors
/// Length out of `32..=128` or a non-URL-safe byte.
pub(crate) fn receipt_hash(receipt: &str) -> Result<String> {
    if !(MIN_RECEIPT_LEN..=MAX_RECEIPT_LEN).contains(&receipt.len())
        || receipt
            .bytes()
            .any(|b| !(b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
    {
        return Err(crate::error::CoreError::Validation(
            "delivery receipt must be 32..=128 URL-safe characters".into(),
        ));
    }
    Ok(crate::api_keys::models::hash_key(receipt))
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
    format!(
        "OP-{}-{}",
        String::from_utf8_lossy(a),
        String::from_utf8_lossy(b)
    )
}

fn row_to_order(row: &rusqlite::Row<'_>) -> rusqlite::Result<CreditOrder> {
    let base_units: i64 = row.get(5)?;
    Ok(CreditOrder {
        id: row.get(0)?,
        order_code: row.get(1)?,
        api_key_id: row.get(2)?,
        checkout_key_id: row.get(15)?,
        payment_window_secs: row.get(16)?,
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
     credit_units, checkout_key_id, unixepoch(expires_at)-unixepoch(created_at)";

fn is_unique_violation(err: &rusqlite::Error) -> bool {
    err.sqlite_error().is_some_and(|e| {
        matches!(
            e.extended_code,
            rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE | rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY
        )
    })
}

/// Create a key-bound checkout order (legacy/internal surface). `base_units`
/// is the plan price in micro-USDC; the drawn jitter (sub-cent) rides on top
/// only to identify the order.
///
/// Anonymous orders are REFUSED here: a fresh key must be delivery-receipt
/// bound from creation via [`create_order_with_receipt`], so this function
/// fails closed rather than mint a key that a leaked code could claim.
///
/// # Errors
/// - validation: non-whole-dollar `base_units`, non-positive `ttl_days`, or an
///   anonymous target.
/// - database: exhausted jitter space or an unexpected DB failure.
pub fn create_order(
    conn: &Connection,
    api_key_id: Option<ApiKeyId>,
    payer_email: Option<&str>,
    base_units: i64,
    ttl_days: i64,
    tier_id: Option<i64>,
) -> Result<CreditOrder> {
    create_order_with_window(
        conn,
        api_key_id,
        payer_email,
        base_units,
        ttl_days,
        tier_id,
        PAYMENT_WINDOW_SECS,
    )
}

/// Create an order with the configured payment window, snapshotted at creation.
///
/// Anonymous targets are refused (see [`create_order_with_receipt`]); this is
/// the key-bound-only wrapper used by internal callers.
///
/// # Errors
/// Rejects windows outside 1 minute..=1 day, anonymous targets, and propagates
/// checkout failures.
pub fn create_order_with_window(
    conn: &Connection,
    api_key_id: Option<ApiKeyId>,
    payer_email: Option<&str>,
    base_units: i64,
    ttl_days: i64,
    tier_id: Option<i64>,
    payment_window_secs: i64,
) -> Result<CreditOrder> {
    if api_key_id.is_none() {
        return Err(crate::error::CoreError::Validation(
            "anonymous checkout requires a delivery receipt (create_order_with_receipt)".into(),
        ));
    }
    create_order_inner(
        conn,
        CreateOrderRequest {
            api_key_id,
            payer_email: payer_email.map(str::to_owned),
            base_units,
            ttl_days,
            tier_id,
        },
        payment_window_secs,
        None,
    )
}

/// Create an order with a delivery receipt bound from creation. This is the
/// only path that may mint an anonymous key; the receipt hash is persisted on
/// the row and the reveal surface only accepts that exact receipt
/// (see the credits delivery module). Key-bound top-ups also accept (and store)
/// a receipt harmlessly, so a single call shape serves every checkout.
///
/// # Errors
/// Invalid receipt, invalid window, or checkout/DB failures.
pub fn create_order_with_receipt(
    conn: &Connection,
    req: CreateOrderRequest,
    payment_window_secs: i64,
    receipt: &str,
) -> Result<CreditOrder> {
    let hash = receipt_hash(receipt)?;
    create_order_inner(conn, req, payment_window_secs, Some(hash.as_str()))
}

fn create_order_inner(
    conn: &Connection,
    req: CreateOrderRequest,
    payment_window_secs: i64,
    receipt_hash: Option<&str>,
) -> Result<CreditOrder> {
    if !(60..=86_400).contains(&payment_window_secs) {
        return Err(crate::error::CoreError::Validation(
            "payment window must be 60..=86400 seconds".into(),
        ));
    }
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)
        .map_err(openproxy_db::error::map_db_error)?;
    // Lookup and insert share the write transaction, including across pools
    // or processes. An expired/paid receipt remains bound forever: a fresh
    // purchase must use a fresh capability, never silently open another bill.
    if let Some(hash) = receipt_hash
        && let Some(order) = replay_order(&tx, &req, hash)?
    {
        tx.commit().map_err(openproxy_db::error::map_db_error)?;
        return Ok(order);
    }
    let order = allocate_order(&tx, &req, payment_window_secs, receipt_hash)?;
    tx.commit().map_err(openproxy_db::error::map_db_error)?;
    Ok(order)
}

fn replay_order(
    conn: &Connection,
    req: &CreateOrderRequest,
    hash: &str,
) -> Result<Option<CreditOrder>> {
    let mut stmt = conn
        .prepare(
            "SELECT order_code, checkout_key_id, tier_id, base_units, ttl_days \
         FROM credit_orders WHERE reveal_receipt_hash = ?1 ORDER BY id",
        )
        .map_err(openproxy_db::error::map_db_error)?;
    let mut rows = stmt
        .query([hash])
        .map_err(openproxy_db::error::map_db_error)?;
    let Some(row) = rows.next().map_err(openproxy_db::error::map_db_error)? else {
        return Ok(None);
    };
    let code: String = row.get(0).map_err(openproxy_db::error::map_db_error)?;
    let key: Option<i64> = row.get(1).map_err(openproxy_db::error::map_db_error)?;
    let tier: Option<i64> = row.get(2).map_err(openproxy_db::error::map_db_error)?;
    let base: i64 = row.get(3).map_err(openproxy_db::error::map_db_error)?;
    let ttl: i64 = row.get(4).map_err(openproxy_db::error::map_db_error)?;
    if rows
        .next()
        .map_err(openproxy_db::error::map_db_error)?
        .is_some()
        || key != req.api_key_id.map(|k| k.0)
        || tier != req.tier_id
        || (tier.is_none() && (base != req.base_units || ttl != req.ttl_days))
    {
        return Err(crate::error::CoreError::Validation(
            "Idempotency-Key is already bound to a different or ambiguous purchase".into(),
        ));
    }
    get_order(conn, &code)
}

fn allocate_order(
    conn: &Connection,
    req: &CreateOrderRequest,
    payment_window_secs: i64,
    receipt_hash: Option<&str>,
) -> Result<CreditOrder> {
    let api_key_id = req.api_key_id;
    let payer_email = req.payer_email.as_deref();
    let (base_units, ttl_days, credit_units) = if let Some(tid) = req.tier_id {
        let tier = crate::credits::tiers::get(conn, tid)?
            .filter(|tier| tier.sellable())
            .ok_or_else(|| crate::error::CoreError::Validation("plan is not sellable".into()))?;
        let price = tier
            .price_usd
            .and_then(crate::credits::usd_to_units)
            .ok_or_else(|| crate::error::CoreError::Validation("invalid plan price".into()))?;
        let credit = crate::credits::usd_to_units(
            tier.credit_usd
                .unwrap_or(tier.price_usd.unwrap_or_default()),
        )
        .ok_or_else(|| crate::error::CoreError::Validation("invalid plan credit".into()))?;
        (price, tier.ttl_days, credit)
    } else {
        (req.base_units, req.ttl_days, req.base_units)
    };
    if let Some(kid) = api_key_id
        && crate::api_keys::crud::get_by_id(conn, kid)?.is_none()
    {
        return Err(crate::error::CoreError::Validation(
            "unknown payment target key".into(),
        ));
    }
    expire_stale_orders(conn)?;
    // Plan prices may carry cents: amounts stay unique because matching is
    // exact per order and the live-order index enforces one order per amount.
    // Free-amount checkouts (no tier) must be whole USD (the jitter owns the
    // cents).
    let whole_ok = base_units % crate::credits::UNITS_PER_USD == 0;
    if base_units <= 0 || (!whole_ok && req.tier_id.is_none()) {
        return Err(crate::error::CoreError::Validation(format!(
            "base amount must be a whole number of USD in micro units, got {base_units}"
        )));
    }
    if !(1..=36_500).contains(&ttl_days) {
        return Err(crate::error::CoreError::Validation(
            "ttl_days must be between 1 and 36500".into(),
        ));
    }

    let mut last_err: Option<rusqlite::Error> = None;
    for _ in 0..MAX_JITTER_ATTEMPTS {
        let jitter = rand::rng().random_range(0..JITTER_SPACE);
        let amount_units = base_units.checked_add(jitter).ok_or_else(|| {
            crate::error::CoreError::Validation("payment amount exceeds supported range".into())
        })?;
        // Keep a reservation until the complete acceptance window closes,
        // regardless of status. This also prevents overlap after early payment.
        let reserved: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM credit_orders WHERE amount_units = ?1 \
             AND datetime(expires_at, ?2) >= datetime('now'))",
                params![amount_units, format!("+{CONFIRMATION_GRACE_SECS} seconds")],
                |row| row.get(0),
            )
            .map_err(openproxy_db::error::map_db_error)?;
        if reserved {
            continue;
        }
        let code = gen_order_code();
        let res = conn.execute(
            "INSERT INTO credit_orders \
                 (order_code, api_key_id, payer_email, amount_units, base_units, ttl_days, \
                  status, expires_at, tier_id, credit_units, reveal_receipt_hash, checkout_key_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', \
                     datetime('now', ?7), ?8, ?9, ?10, ?2)",
            params![
                code,
                api_key_id.map(|k| k.0),
                payer_email,
                amount_units,
                base_units,
                ttl_days,
                format!("+{payment_window_secs} seconds"),
                req.tier_id,
                credit_units,
                receipt_hash,
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
                        crate::error::CoreError::Internal(
                            "credit order vanished after insert".into(),
                        )
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
}
