//! Retryable, receipt-bound key delivery. Acknowledgement wipes ciphertext.
//!
//! Every anonymous order created through
//! [`super::orders::create_order_with_receipt`] carries a `reveal_receipt_hash`
//! from the moment it is created. The reveal surface only ever decrypts for
//! that exact receipt — there is no first-reveal claim by a code-only caller,
//! and legacy rows whose hash is NULL (created before the binding) fail closed
//! instead of being claimable by whoever knows the order code.
use crate::error::Result;
use openproxy_db::{MasterKey, error::map_db_error};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use super::orders::receipt_hash;

/// Decrypt and mark the delivery for one order inside an existing transaction.
/// No BEGIN/COMMIT here: the single-order wrapper and the group batch both
/// call this and own the transaction (or savepoint) themselves.
fn reveal_in_tx(
    conn: &Connection,
    master: &MasterKey,
    code: &str,
    receipt: &str,
) -> Result<Option<String>> {
    let hash = receipt_hash(receipt)?;
    // The bound hash is the ONLY capability: NULL (legacy, unbound) matches
    // nothing, so `reveal_receipt_hash = ?2` fails closed by construction.
    let blob: Option<Vec<u8>> = conn
        .query_row(
            "SELECT minted_key_encrypted FROM credit_orders \
         WHERE order_code = ?1 AND status = 'minted' \
           AND minted_key_encrypted IS NOT NULL \
           AND reveal_receipt_hash = ?2",
            params![code, hash],
            |r| r.get(0),
        )
        .optional()
        .map_err(map_db_error)?;
    let Some(blob) = blob else {
        return Ok(None);
    };
    let plaintext = master.decrypt(&blob)?;
    // First successful delivery for this receipt marks the order revealed.
    // The hash never changes afterwards, so a later reveal cannot widen it.
    conn.execute(
        "UPDATE credit_orders SET key_revealed = 1 \
        WHERE order_code = ?1 AND reveal_receipt_hash = ?2",
        params![code, hash],
    )
    .map_err(map_db_error)?;
    Ok(Some(plaintext))
}

/// Retry the delivery of a minted key. The same receipt may read the key
/// repeatedly until the buyer acknowledges (the ciphertext is kept for
/// exactly that), but a different receipt — including one that knows the
/// order code — can never read it, and a receipt-less (legacy) order can
/// never be claimed at all.
///
/// # Errors
/// Invalid receipt, database or decryption failure.
pub fn reveal_minted_key_with_receipt(
    conn: &Connection,
    master: &MasterKey,
    code: &str,
    receipt: &str,
) -> Result<Option<String>> {
    let tx =
        Transaction::new_unchecked(conn, TransactionBehavior::Immediate).map_err(map_db_error)?;
    let out = reveal_in_tx(&tx, master, code, receipt)?;
    tx.commit().map_err(map_db_error)?;
    Ok(out)
}

/// Mark the delivery acknowledged and erase the encrypted copy for one order
/// inside an existing transaction. See [`acknowledge_minted_key`] for the
/// idempotency contract.
fn ack_in_tx(conn: &Connection, code: &str, receipt: &str) -> Result<bool> {
    let hash = receipt_hash(receipt)?;
    let changed = conn
        .execute(
            "UPDATE credit_orders SET minted_key_encrypted = NULL \
        WHERE order_code = ?1 AND key_revealed = 1 AND reveal_receipt_hash = ?2",
            params![code, hash],
        )
        .map_err(map_db_error)?;
    if changed == 1 {
        return Ok(true);
    }
    // Already acknowledged by this same receipt: the wipe happened earlier,
    // the delivery identity is still verified, so this is a success — that is
    // what makes the client's retry-after-network-drop safe.
    let delivered: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM credit_orders \
         WHERE order_code = ?1 AND key_revealed = 1 AND reveal_receipt_hash = ?2)",
            params![code, hash],
            |row| row.get(0),
        )
        .map_err(map_db_error)?;
    Ok(delivered)
}

/// Acknowledge receipt and erase the encrypted delivery copy. Idempotent: a
/// repeated ACK with the SAME receipt succeeds even when the ciphertext is
/// already gone, while an unknown receipt, an unrevealed order or a mismatched
/// receipt report `false` and never wipe anything.
///
/// # Errors
/// Invalid receipt or database failure.
pub fn acknowledge_minted_key(conn: &Connection, code: &str, receipt: &str) -> Result<bool> {
    ack_in_tx(conn, code, receipt)
}

/// How long the encrypted plaintext of a minted key stays recoverable after
/// the mint: the re-reveal (and with it the recovery link) stops working at
/// that point even without an ack. The key itself stays fully valid — only
/// the checkout's copy is erased, matching the "shown once" contract.
pub const MINTED_KEY_TTL_SECS: i64 = 7 * 86_400;

/// Null the ciphertext of minted keys whose delivery TTL ran out.
///
/// Without an ack the encrypted plaintext was re-revealable for ever: a
/// leaked recovery link (?receipt=) could re-show the key weeks later when
/// the buyer never completed the reveal. The sweep bounds that exposure to
/// [`MINTED_KEY_TTL_SECS`]. Rows never minted, already acked (blob NULL) and
/// rows whose TTL has not passed are untouched. Returns the number of blobs
/// erased.
///
/// ROOT FIX (audit A3-01): the ciphertext is the buyer's ONLY self-service
/// copy of the paid key, so it must outlive the fixed 7-day TTL whenever
/// the balance the buyer paid for is still alive. A row is only wiped when
/// BOTH the TTL has passed AND the order's credited lot is no longer active
/// (expired, exhausted or missing): a buyer who paid for a 30-day plan and
/// revealed on day 8 keeps the recovery path open for the whole plan.
///
/// # Errors
/// Propagates DB failures.
pub fn sweep_expired_minted_ciphertexts(conn: &Connection) -> Result<usize> {
    conn.execute(
        "UPDATE credit_orders SET minted_key_encrypted = NULL \
         WHERE minted_key_encrypted IS NOT NULL \
           AND minted_at IS NOT NULL \
           AND datetime(minted_at, ?1) < datetime('now') \
           AND NOT EXISTS ( \
               SELECT 1 FROM credit_transactions t \
                WHERE t.order_id = credit_orders.id \
                  AND t.status = 'active' \
                  AND (t.expires_at IS NULL \
                       OR datetime(t.expires_at) > datetime('now')))",
        rusqlite::params![format!("+{MINTED_KEY_TTL_SECS} seconds")],
    )
    .map_err(openproxy_db::error::map_db_error)
}

/// One item of a grouped delivery write.
#[derive(Debug, Clone)]
pub enum DeliveryOp {
    /// Retry the delivery of the minted key (returns the plaintext).
    Reveal { receipt: String },
    /// Acknowledge the delivery and wipe the ciphertext.
    Ack { receipt: String },
}

/// Per-item result of a grouped delivery write.
#[derive(Debug, Clone)]
pub enum DeliveryOutcome {
    /// Plaintext, `None` when the receipt does not open this order.
    Revealed(Option<String>),
    /// `true` when this call performed (or had already performed) the wipe.
    Acked(bool),
}

/// Group commit for the delivery path: N simultaneous reveals/acks, ONE
/// fsync. Each item runs inside its own SAVEPOINT, so one poisoned item (bad
/// receipt, decrypt failure) rolls back only itself and still reports its
/// error; the neighbours commit. A COMMIT failure invalidates the WHOLE
/// batch: no caller receives a plaintext or an ack, because replies are only
/// sent after the commit returned `Ok` — every caller can simply retry.
pub fn apply_delivery_batch(
    conn: &Connection,
    master: &MasterKey,
    items: &[(String, DeliveryOp)],
) -> Result<Vec<Result<DeliveryOutcome>>> {
    // A busy writer must not fail the batch: the whole group retries with
    // backoff. Replaying is safe — a rolled-back attempt wrote nothing, a
    // committed reveal re-decrypts, and the ack is idempotent by receipt.
    openproxy_db::error::with_busy_retry("apply_delivery_batch", || {
        let mut tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
            .map_err(map_db_error)?;
        let mut out = Vec::with_capacity(items.len());
        for (code, op) in items {
            // Dropping an uncommitted savepoint rolls back to it (rusqlite Drop).
            let sp = tx.savepoint().map_err(map_db_error)?;
            let result = match op {
                DeliveryOp::Reveal { receipt } => {
                    reveal_in_tx(&sp, master, code, receipt).map(DeliveryOutcome::Revealed)
                }
                DeliveryOp::Ack { receipt } => {
                    ack_in_tx(&sp, code, receipt).map(DeliveryOutcome::Acked)
                }
            };
            match result {
                Ok(value) => {
                    sp.commit().map_err(map_db_error)?;
                    out.push(Ok(value));
                }
                Err(e) => {
                    drop(sp);
                    out.push(Err(e));
                }
            }
        }
        tx.commit().map_err(map_db_error)?;
        Ok(out)
    })
}
