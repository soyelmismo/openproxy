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

pub use super::delivery::{acknowledge_minted_key, reveal_minted_key_with_receipt};
use crate::api_keys;
use crate::error::Result;
use openproxy_db::secrets::MasterKey;
use openproxy_types::ids::ApiKeyId;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::Serialize;

/// Result of [`apply_payment`].
#[derive(Debug, Clone, Serialize)]
pub enum PaymentOutcome {
    /// This exact transfer was already consumed.
    AlreadySeen { tx_hash: String },
    /// A transfer to the collector that matches no live order. Stays in the
    /// wallet as an unmatched deposit for a human to resolve (settlement
    /// collision, late payment against an expired order, short beyond the
    /// configured tolerance, etc.).
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
        /// Micro-USDC actually received (may sit below the quoted amount by up
        /// to the configured tolerance: exchange withdrawal fees).
        received_units: i64,
    },
}

/// Inclusion coordinates from the verified chain block.
#[derive(Debug, Clone, Copy)]
pub struct PaymentBlock {
    pub number: i64,
    pub timestamp: i64,
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
    tolerance_units: i64,
) -> Result<PaymentOutcome> {
    apply_payment_at(
        conn,
        master_key,
        tx_hash,
        log_index,
        PaymentBlock {
            number: block_number,
            timestamp: chrono::Utc::now().timestamp(),
        },
        amount_units,
        from_address,
        tolerance_units,
    )
}

/// Settle using the inclusion timestamp, not the watcher's processing time.
/// This preserves payments made during an outage without accepting old
/// transfers for newer orders that reused the same amount.
pub fn apply_payment_at(
    conn: &Connection,
    master_key: &MasterKey,
    tx_hash: &str,
    log_index: i64,
    block: PaymentBlock,
    amount_units: i64,
    from_address: Option<&str>,
    tolerance_units: i64,
) -> Result<PaymentOutcome> {
    // The pool serializes writers but does NOT start a SQL transaction.
    // Own the transaction here so every caller gets rollback on any failure.
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .map_err(openproxy_db::error::map_db_error)?;
    let outcome = settle_and_record(
        &tx,
        master_key,
        tx_hash,
        log_index,
        block,
        amount_units,
        from_address,
        tolerance_units,
    )?;
    tx.commit().map_err(openproxy_db::error::map_db_error)?;
    Ok(outcome)
}

/// One verified transfer, in chain order. Group settlement keeps FULL durability
/// while sharing the commit across transfers. No outcome escapes a failed commit.
pub struct PaymentInput {
    pub tx_hash: String,
    pub log_index: i64,
    pub block: PaymentBlock,
    pub amount_units: i64,
    pub from_address: Option<String>,
}

/// Apply a bounded group atomically. Any error rolls back the entire group;
/// replay is safe through the persistent (tx_hash, log_index) identity.
pub fn apply_payments_batch(
    conn: &Connection,
    master: &MasterKey,
    payments: &[PaymentInput],
    tolerance_units: i64,
) -> Result<Vec<PaymentOutcome>> {
    if payments.is_empty() {
        return Ok(Vec::new());
    }
    // A busy writer (a long usage drain, a checkpoint) must not surface as a
    // failed payment batch: the whole group retries with backoff. Replaying
    // is safe — `matched_payments` PK + the CAS make each payment land once,
    // and a rolled-back attempt wrote nothing.
    openproxy_db::error::with_busy_retry("apply_payments_batch", || {
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
            .map_err(openproxy_db::error::map_db_error)?;
        let mut outcomes = Vec::with_capacity(payments.len());
        for p in payments {
            outcomes.push(settle_and_record(
                &tx,
                master,
                &p.tx_hash,
                p.log_index,
                p.block,
                p.amount_units,
                p.from_address.as_deref(),
                tolerance_units,
            )?);
        }
        tx.commit().map_err(openproxy_db::error::map_db_error)?;
        Ok(outcomes)
    })
}

fn settle_and_record(
    conn: &Connection,
    master_key: &MasterKey,
    tx_hash: &str,
    log_index: i64,
    block: PaymentBlock,
    amount_units: i64,
    from_address: Option<&str>,
    tolerance_units: i64,
) -> Result<PaymentOutcome> {
    let outcome = settle_payment(
        conn,
        master_key,
        tx_hash,
        log_index,
        block,
        amount_units,
        from_address,
        tolerance_units,
    )?;
    let (kind, code) = match &outcome {
        PaymentOutcome::Credited { order_code, .. } => ("credited", Some(order_code.as_str())),
        PaymentOutcome::Unmatched { .. } => ("unmatched", None),
        PaymentOutcome::OrderExpired { order_code } => ("unmatched", Some(order_code.as_str())),
        PaymentOutcome::AlreadySeen { .. } => ("", None),
    };
    if !kind.is_empty() {
        conn.execute(
            "UPDATE matched_payments SET outcome = ?1, order_id = \
            (SELECT id FROM credit_orders WHERE order_code = ?2) \
            WHERE tx_hash = ?3 AND log_index = ?4",
            rusqlite::params![kind, code, tx_hash, log_index],
        )
        .map_err(openproxy_db::error::map_db_error)?;
    }
    Ok(outcome)
}

struct PayableOrder {
    id: i64,
    code: String,
    key_id: Option<i64>,
    ttl_days: i64,
    tier_id: Option<i64>,
    credit_units: i64,
}

fn settle_payment(
    conn: &Connection,
    master_key: &MasterKey,
    tx_hash: &str,
    log_index: i64,
    block: PaymentBlock,
    amount_units: i64,
    from_address: Option<&str>,
    tolerance_units: i64,
) -> Result<PaymentOutcome> {
    let block_number = block.number;
    if amount_units <= 0 || block_number < 0 || log_index < 0 || block.timestamp < 0 {
        return Err(crate::error::CoreError::Validation(
            "invalid payment coordinates".into(),
        ));
    }
    let tolerance_units = tolerance_units.max(0);
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

    // 2. Find the live order this payment pays. Exact amount first (the common
    //    path: the buyer reproduced the quoted jitter to the micro-unit — the
    //    live-amount unique index makes that match unambiguous), then the
    //    tolerance path: a custodial exchange deducts its withdrawal fee from
    //    the sent amount, so a $5 transfer can land as 4.80. A short payment
    //    is only credited when exactly ONE live order sits inside the
    //    configured window: with two candidates the fee reorders distances
    //    (4.809 is closer to 5.004 than to 5.009) and picking one would guess
    //    which buyer sent it — that deposit stays unmatched for a human. The
    //    acceptance window uses the same grace constant as order allocation so
    //    a payment can never match an order that the allocator already
    //    considers closed.
    let grace = format!("+{} seconds", super::orders::CONFIRMATION_GRACE_SECS);
    let exact: Option<(PayableOrder, i64)> = conn
        .query_row(
            "SELECT id, order_code, api_key_id, ttl_days, tier_id, \
             COALESCE(credit_units, base_units), amount_units \
             FROM credit_orders \
             WHERE amount_units = ?1 AND status IN ('pending', 'expired') \
               AND created_at <= datetime(?2, 'unixepoch') \
               AND datetime(expires_at, ?3) >= datetime(?2, 'unixepoch') \
               AND (SELECT COUNT(*) FROM credit_orders c WHERE c.amount_units = ?1 \
                    AND c.status != 'cancelled' \
                    AND c.created_at <= datetime(?2, 'unixepoch') \
                    AND datetime(c.expires_at, ?3) >= datetime(?2, 'unixepoch')) = 1",
            rusqlite::params![amount_units, block.timestamp, grace],
            |row| {
                Ok((
                    PayableOrder {
                        id: row.get(0)?,
                        code: row.get(1)?,
                        key_id: row.get(2)?,
                        ttl_days: row.get(3)?,
                        tier_id: row.get(4)?,
                        credit_units: row.get(5)?,
                    },
                    row.get(6)?,
                ))
            },
        )
        .optional()
        .map_err(openproxy_db::error::map_db_error)?;
    // An exact, unambiguous match wins by contract. Do not scan every other
    // pending order for tolerance candidates when the exact match exists.
    let near = if exact.is_none() {
        let mut stmt = conn
            .prepare(
                "SELECT id, order_code, api_key_id, ttl_days, tier_id, \
             COALESCE(credit_units, base_units), amount_units \
             FROM credit_orders \
             WHERE status IN ('pending', 'expired') \
               AND ABS(amount_units - ?1) <= ?4 \
               AND ABS(amount_units - ?1) > 0 \
               AND created_at <= datetime(?2, 'unixepoch') \
               AND datetime(expires_at, ?3) >= datetime(?2, 'unixepoch')",
            )
            .map_err(openproxy_db::error::map_db_error)?;
        let near: Vec<(PayableOrder, i64)> = stmt
            .query_map(
                rusqlite::params![amount_units, block.timestamp, grace, tolerance_units],
                |row| {
                    Ok((
                        PayableOrder {
                            id: row.get(0)?,
                            code: row.get(1)?,
                            key_id: row.get(2)?,
                            ttl_days: row.get(3)?,
                            tier_id: row.get(4)?,
                            credit_units: row.get(5)?,
                        },
                        row.get(6)?,
                    ))
                },
            )
            .map_err(openproxy_db::error::map_db_error)?
            .collect::<std::result::Result<Vec<_>, rusqlite::Error>>()
            .map_err(openproxy_db::error::map_db_error)?;
        drop(stmt);
        near
    } else {
        Vec::new()
    };
    // Exact first; otherwise the tolerance path credits only an unambiguous
    // single candidate.
    let had_exact = exact.is_some();
    let had_candidates = !near.is_empty();
    let chosen: Option<(PayableOrder, i64)> = match exact {
        Some(hit) => Some(hit),
        None => {
            if near.len() > 1 {
                tracing::warn!(
                    %tx_hash,
                    amount_units,
                    block_number,
                    candidates = near.len(),
                    "ambiguous short payment near several live orders: no order credited"
                );
                None
            } else {
                near.into_iter().next()
            }
        }
    };
    let Some((payable, quoted_units)) = chosen else {
        if !had_exact && !had_candidates {
            tracing::warn!(
                %tx_hash,
                amount_units,
                block_number,
                "unmatched USDC transfer to collector: no live order within tolerance"
            );
        }
        return Ok(PaymentOutcome::Unmatched {
            tx_hash: tx_hash.to_string(),
            amount_units,
        });
    };
    let PayableOrder {
        id: order_id,
        code: order_code,
        key_id: api_key_id,
        ttl_days,
        tier_id,
        credit_units,
    } = payable;
    let short_units = quoted_units - amount_units; // negative when overpaid

    if let Some(existing) = api_key_id
        && crate::api_keys::crud::get_by_id(conn, ApiKeyId(existing))?.is_none()
    {
        tracing::error!(%tx_hash, %order_code, existing, "payment target was deleted; deposit requires operator review");
        return Ok(PaymentOutcome::Unmatched {
            tx_hash: tx_hash.into(),
            amount_units,
        });
    }
    let tier = tier_id
        .map(|id| crate::credits::tiers::get(conn, id))
        .transpose()?
        .flatten();
    if tier_id.is_some() && tier.is_none() {
        tracing::error!(%tx_hash, %order_code, "paid plan was deleted; deposit requires operator review");
        return Ok(PaymentOutcome::Unmatched {
            tx_hash: tx_hash.into(),
            amount_units,
        });
    }
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
    let final_key_id;
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
                "UPDATE credit_orders SET minted_key_encrypted = ?1, status = 'minted', \
                 minted_at = datetime('now'), api_key_id = ?3 \
                 WHERE id = ?2",
                rusqlite::params![blob, order_id, final_key_id],
            )
            .map_err(openproxy_db::error::map_db_error)?;
        }
    }
    let key_id = ApiKeyId(final_key_id);

    if let Some(tier) = &tier {
        let previous: Option<i64> = conn
            .query_row(
                "SELECT tier_id FROM api_keys WHERE id = ?1",
                [key_id.0],
                |r| r.get(0),
            )
            .map_err(openproxy_db::error::map_db_error)?;
        conn.execute(
            "UPDATE credit_transactions SET status = 'expired' \
             WHERE api_key_id = ?1 AND status = 'active' \
               AND (kind = 'grant' OR ?2 IS NOT ?3)",
            rusqlite::params![key_id.0, previous, tier.id],
        )
        .map_err(openproxy_db::error::map_db_error)?;
    }
    // 5. Credit the balance. A leveraged plan credits more than it charged
    //    (credit_units > amount minus jitter); 1:1 plans credit the price.
    crate::credits::ledger::credit_topup(conn, key_id, order_id, credit_units, ttl_days, tx_hash)?;

    // 5b. Shaping contract: a plan purchase applies the tier's allowlists +
    //     quota caps and LINKS the key to the tier (future plan edits
    //     propagate to every linked key). The paid order must leave the key
    //     shaped exactly like the plan that was bought.
    if let Some(tier) = tier {
        crate::credits::tiers::apply_to_key_forced(
            conn,
            key_id,
            &crate::credits::tiers::tier_own_config(&tier),
        )?;
        crate::credits::tiers::set_key_tier(conn, key_id, Some(tier.id))?;
    }

    tracing::info!(
        %tx_hash,
        order_code = %order_code,
        credited_units = credit_units,
        quoted_units,
        received_units = amount_units,
        short_units,
        key_id = final_key_id,
        "top-up credited"
    );
    Ok(PaymentOutcome::Credited {
        order_code,
        api_key_id: final_key_id,
        credited_units: credit_units,
        minted_key,
        received_units: amount_units,
    })
}
