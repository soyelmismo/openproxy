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
    let hash = receipt_hash(receipt)?;
    let tx =
        Transaction::new_unchecked(conn, TransactionBehavior::Immediate).map_err(map_db_error)?;
    // The bound hash is the ONLY capability: NULL (legacy, unbound) matches
    // nothing, so `reveal_receipt_hash = ?2` fails closed by construction.
    let blob: Option<Vec<u8>> = tx
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
    tx.execute(
        "UPDATE credit_orders SET key_revealed = 1 \
        WHERE order_code = ?1 AND reveal_receipt_hash = ?2",
        params![code, hash],
    )
    .map_err(map_db_error)?;
    tx.commit().map_err(map_db_error)?;
    Ok(Some(plaintext))
}

/// Acknowledge receipt and erase the encrypted delivery copy. Idempotent: a
/// repeated ACK with the SAME receipt succeeds even when the ciphertext is
/// already gone, while an unknown receipt, an unrevealed order or a mismatched
/// receipt report `false` and never wipe anything.
///
/// # Errors
/// Invalid receipt or database failure.
pub fn acknowledge_minted_key(conn: &Connection, code: &str, receipt: &str) -> Result<bool> {
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
