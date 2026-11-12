//! Payment matcher: assign one on-chain USDC transfer to a live order and
//! credit its balance, or mint the anonymous order's fresh key.
//!
//! Idempotency and correctness (the two properties money code must have):
//!
//! 1. **Each log counts once.** `matched_payments (tx_hash, log_index)` PK
//!    makes `apply_payment` idempotent across watcher restarts, re-scans and
//!    duplicate deliveries — the INSERT either happens or tells us the log
//!    was already processed.
//! 2. **Each order pays once.** The CAS UPDATE
//!    `SET status='paid' WHERE id=? AND status='pending'` is the arbitrating
//!    instruction: N concurrent deliveries of the same amount, or even the
//!    same log replayed after a crash before the payment was recorded, all
//!    see 0 rows except exactly one caller. The credit (and the anonymous
//!    key mint) happen inside that caller's transaction, so a credited
//!    order can never be credited again by definition — the status guards it.
//! 3. **No partial states.** Either the payment is recorded, the order is
//!    paid and the balanced is credited (one commit), or nothing happened.

use crate::api_keys;
use crate::error::Result;
use openproxy_db::secrets::{MasterKey};
use openproxy_types::ids::ApiKeyId;
use rusqlite::Connection;
use serde::Serialize;

/// Result of [`apply_payment`].
#[derive(Debug, Clone, Serialize)]
pub enum PaymentOutcome {
    /// This exact transfer was already consumed.
    AlreadySeen { tx_hash: String },
    /// A transfer to the collector that matches no live order. Stays in the
    /// wallet as an unmatched deposit for a human to resolve (settlement
    /// collision, late payment against an expired order, etc.).
    Unmatched { tx_hash: String, amount_units: i64 },
    /// Matched an order, but its payment window had already run out. The
    /// order stays `expired`; the deposit is recoverable via a manual
    /// adjustment.
    OrderExpired { order_code: String },
    /// Order paid and balance credited (or key minted for anonymous orders).
    Credited {
        order_code: String,
        api_key_id: i64,
        credited_units: i64,
        /// Plaintext key, present only for anonymous checkouts at mint time.
        minted_key: Option<String>,
    },
}

/// Process one USDC `Transfer` log. See the module docs for the guarantees.
///
/// # Errors
/// - database failures;
/// - master-key crypto failures while minting an anonymous key.
pub fn apply_payment(
    conn: &Connection,
    master_key: &MasterKey,
    tx_hash: &str,
    log_index: i64,
    block_number: i64,
    amount_units: i64,
    from_address: Option<&str>,
) -> Result<PaymentOutcome> {
    // 1. Idempotency gate: record the log. Already recorded → done.
    let inserted = conn
        .execute(
            "INSERT OR IGNORE INTO matched_payments \
                 (tx_hash, log_index, block_number, amount_units, from_address) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![tx_hash, log_index, block_number, amount_units, from_address],
        )
        .map_err(openproxy_db::error::map_db_error)?;
    if inserted == 0 {
        return Ok(PaymentOutcome::AlreadySeen {
            tx_hash: tx_hash.to_string(),
        });
    }

    // 2. Find the live order advertising exactly this amount.
    let order: Option<(
        i64,
        String,
        Option<i64>,
        i64,
        i64,
        Option<i64>,
        i64,
    )> = conn
        .query_row(
            "SELECT id, order_code, api_key_id, base_units, ttl_days, package_id, \
             COALESCE(credit_units, base_units) \
             FROM credit_orders \
             WHERE amount_units = ?1 AND status IN ('pending', 'expired') \
               AND expires_at > datetime('now', '-10 minutes')",
            rusqlite::params![amount_units],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .ok();
    let Some((order_id, order_code, api_key_id, _base_units, ttl_days, package_id, credit_units)) =
        order
    else {
        tracing::warn!(
            %tx_hash,
            amount_units,
            block_number,
            "unmatched USDC transfer to collector: no live order with this amount"
        );
        return Ok(PaymentOutcome::Unmatched {
            tx_hash: tx_hash.to_string(),
            amount_units,
        });
    };

    // 3. CAS: exactly one concurrent caller can flip pending/expired→paid.
    // A payment broadcast before the deadline but confirmed after it still
    // credits (10-minute grace): money received for an issued order must
    // never strand in the collector.
    let flipped = conn
        .execute(
            "UPDATE credit_orders SET status = 'paid', tx_hash = ?1, block_number = ?2, \
             paid_at = datetime('now') \
             WHERE id = ?3 AND status IN ('pending', 'expired')",
            rusqlite::params![tx_hash, block_number, order_id],
        )
        .map_err(openproxy_db::error::map_db_error)?;
    if flipped == 0 {
        return Ok(PaymentOutcome::OrderExpired { order_code });
    }

    // 4. Anonymous checkout: mint the buyer's key now. The plaintext is
    //    encrypted at rest (reveal-once, survives restarts) and the order
    //    moves to `minted`; session-bound orders just credit the tenant key.
    let mut minted_key: Option<String> = None;
    let mut final_key_id = api_key_id.unwrap_or(0);
    match api_key_id {
        Some(existing) => final_key_id = existing,
        None => {
            let created_by = format!("credits-checkout:{order_code}");
            let input = api_keys::models::CreateApiKeyInput {
                label: Some(format!("Checkout {order_code}")),
                scopes: vec!["chat".into()],
                allowed_models: None,
                allowed_combos: None,
                blacklisted_providers: None,
                allowed_providers: None,
                blacklisted_models: None,
                only_whitelisted: false,
                expires_at: None,
            };
            let (key, plaintext) = api_keys::crud::create(conn, input, &created_by)?;
            final_key_id = key.id.0;
            minted_key = Some(plaintext.clone());
            let blob = master_key.encrypt(&plaintext)?;
            conn.execute(
                "UPDATE credit_orders SET minted_key_encrypted = ?1, status = 'minted' \
                 WHERE id = ?2",
                rusqlite::params![blob, order_id],
            )
            .map_err(openproxy_db::error::map_db_error)?;
        }
    }
    let key_id = ApiKeyId(final_key_id);

    // 5. Credit the balance. A leveraged package credits more than it charged
    //    (credit_units > amount minus jitter); free amounts credit 1:1.
    crate::credits::ledger::credit_topup(conn, key_id, order_id, credit_units, ttl_days, tx_hash)?;

    // 5b. Shaping contract: a package tied to a tier applies the tier's
    //     allowlists + quota caps; a legacy package applies its own config.
    //     Either way the paid order must leave the key shaped like the preset.
    if let Some(pkg_id) = package_id {
        if let Some(pkg) = crate::credits::packages::get(conn, pkg_id)? {
            if let Some(cfg) = crate::credits::packages::resolved_config(conn, &pkg)? {
                crate::credits::packages::apply_to_key(conn, key_id, &cfg)?;
            }
        }
    }

    tracing::info!(
        %tx_hash,
        order_code = %order_code,
        credited_units = credit_units,
        charged_units = amount_units,
        key_id = final_key_id,
        "top-up credited"
    );
    Ok(PaymentOutcome::Credited {
        order_code,
        api_key_id: final_key_id,
        credited_units: credit_units,
        minted_key,
    })
}

/// Reveal (once) the minted plaintext of an anonymous order, then wipe the
/// stored ciphertext so a second read cannot happen.
///
/// # Errors
/// - database failures;
/// - master-key crypto failures.
pub fn reveal_minted_key(
    conn: &Connection,
    master_key: &MasterKey,
    order_code: &str,
) -> Result<Option<String>> {
    let row: Option<(Option<Vec<u8>>, i64)> = conn
        .query_row(
            "SELECT minted_key_encrypted, key_revealed FROM credit_orders \
             WHERE order_code = ?1",
            rusqlite::params![order_code],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok();
    let Some((Some(blob), 0)) = row else {
        return Ok(None);
    };
    let plaintext = master_key.decrypt(&blob)?;
    conn.execute(
        "UPDATE credit_orders SET key_revealed = 1, minted_key_encrypted = NULL \
         WHERE order_code = ?1",
        rusqlite::params![order_code],
    )
    .map_err(openproxy_db::error::map_db_error)?;
    Ok(Some(plaintext))
}
