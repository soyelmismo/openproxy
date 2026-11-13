//! Durable intent lifecycle around one admission.
//!
//! The row written here (see `openproxy_db::reservations`) is what survives a
//! crash: the in-memory pending markers of `credits_gate` die with the process,
//! the row does not. Every admission reconciles the open intents of its key
//! **after** both locks are held and **after** the journal was drained, so the
//! durable `usage` table is the only thing consulted. No intent is ever turned
//! into a charge by itself: an ambiguous outcome either reconciles against a
//! durable usage row (real cost) or closes with nothing billed.
//!
//! Policy, in order:
//!
//! 1. A durable usage row for a 2xx response exists → that is the billed cost.
//!    Already debited closes as-is; otherwise the idempotent
//!    `mark_winner_usage_row` applies the debit (`debit_usage` is guarded by
//!    `credits_debited`, so an owner settlement racing this cannot double
//!    charge).
//! 2. No usage row but a durable **checkpoint** exists (the worker recorded how
//!    much billable output it produced before the last delivered chunk) → charge
//!    exactly that, clamped to the live balance, recorded as `recovered_partial`
//!    with `charged_units`. The admitted maximum is never charged: output the
//!    model never produced is not billable.
//! 3. Anything else (never dispatched, cancelled before delivery, or a stream
//!    that started without any checkpoint) → close with nothing charged. No
//!    evidence of an amount means no invented charge; the row keeps the full
//!    audit trail of why.
//!
//! A closed `recovered_partial` row is never revisited, and no late usage row
//! can double-charge it: every usage write for a key happens while that key's
//! flock is held (admission → dispatch → settlement), the journal drain runs
//! before reconciliation under the same flock, and a process that died cannot
//! write anything afterwards. If a usage row existed at all, reconciliation
//! saw it before charging the checkpoint; if it arrives later, there is no
//! writer left to produce it. The recorded charge is therefore final, and no
//! adjustment path is needed.
//!
//! A failure to reconcile leaves the intent `held` and the admission fails
//! closed: the next admission of the key retries the same reconciliation, so
//! recovery is automatic and never needs operator support.

use openproxy_db::reservations as db;
use openproxy_types::error::CoreError;
use openproxy_types::ids::ComboTargetId;
use rusqlite::Connection;
use std::sync::Arc;

/// Counters for one reconciliation pass. Diagnostic: the caller only needs it
/// for tracing.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct ReconcileReport {
    pub billed: usize,
    pub unbilled: usize,
}

/// Resolve every open intent of this key from durable evidence.
///
/// # Errors
/// Fails closed: a reconciliation that cannot be completed leaves the intents
/// open, and the caller must deny the request rather than admit against a
/// balance that may still owe a charge.
pub(super) async fn reconcile(
    conn: Arc<parking_lot::Mutex<Connection>>,
    api_key_id: i64,
) -> openproxy_types::Result<ReconcileReport> {
    spawn(
        conn,
        move |conn| reconcile_blocking(conn, api_key_id),
        "credit intent reconciliation",
    )
    .await
}

fn reconcile_blocking(
    conn: &Connection,
    api_key_id: i64,
) -> openproxy_types::Result<ReconcileReport> {
    let mut report = ReconcileReport::default();
    for open in db::open(conn, api_key_id)? {
        let (state, outcome, charged, note) =
            match db::delivered_usage(conn, &open.request_id, api_key_id)? {
                Some(evidence) if evidence.debited => (
                    db::STATE_SETTLED,
                    db::OUTCOME_RECOVERED_BILLED,
                    evidence.cost_units,
                    "delivered response was already debited",
                ),
                Some(evidence) if evidence.cost_units > 0 => {
                    openproxy_db::cost::mark_winner_usage_row(
                        conn,
                        &open.request_id,
                        evidence.attempt,
                        ComboTargetId(evidence.target_id),
                    )?;
                    (
                        db::STATE_SETTLED,
                        db::OUTCOME_RECOVERED_BILLED,
                        evidence.cost_units,
                        "recovered the debit of a delivered response",
                    )
                }
                Some(_) => (
                    db::STATE_RELEASED,
                    db::OUTCOME_RECOVERED_UNBILLED,
                    0,
                    "delivered response carried no billable cost",
                ),
                None if open.generated_units > 0 => (
                    db::STATE_SETTLED,
                    db::OUTCOME_RECOVERED_PARTIAL,
                    charge_observed(conn, api_key_id, open.generated_units)?,
                    "usage row lost after checkpoints: charged what was durably produced",
                ),
                None => (
                    db::STATE_RELEASED,
                    db::OUTCOME_RECOVERED_UNBILLED,
                    0,
                    "nothing delivered or nothing verifiable: the request is not client-billable",
                ),
            };
        if db::close(conn, open.id, state, outcome, charged, note)? {
            match outcome {
                db::OUTCOME_RECOVERED_UNBILLED => report.unbilled += 1,
                _ => report.billed += 1,
            }
            tracing::warn!(
                api_key_id,
                request_id = %open.request_id,
                outcome,
                charged_units = charged,
                note,
                "credit intent recovered from durable state"
            );
        }
    }
    Ok(report)
}

/// Charge what the worker durably checkpointed, never more than the live
/// balance. A balance that no longer covers it (expired lots) charges what is
/// left instead of failing forever: the intent has to close, and the audit
/// records the amount actually taken.
fn charge_observed(
    conn: &Connection,
    api_key_id: i64,
    observed_units: i64,
) -> openproxy_types::Result<i64> {
    let balance = openproxy_db::credits_ledger::balance_units(conn, api_key_id)?;
    let debit = observed_units.min(balance).max(0);
    if debit == 0 {
        return Ok(0);
    }
    match openproxy_db::credits_ledger::record_spend(conn, api_key_id, debit) {
        Ok(charged) => Ok(charged),
        Err(CoreError::InsufficientCredits { .. }) => {
            tracing::warn!(
                api_key_id,
                observed_units = observed_units,
                "credit intent: checkpointed amount no longer fits the balance; closing uncharged"
            );
            Ok(0)
        }
        Err(error) => Err(error),
    }
}

/// Write and commit the intent for a request that is about to be dispatched.
/// Must complete before anything reaches the upstream: it is the only durable
/// evidence that the request existed.
///
/// # Errors
/// Propagates DB failures; callers fail closed.
pub(super) async fn hold(
    conn: Arc<parking_lot::Mutex<Connection>>,
    api_key_id: i64,
    request_id: String,
    attempt: u8,
    reserved_units: i64,
    quoted: bool,
) -> openproxy_types::Result<i64> {
    spawn(
        conn,
        move |conn| {
            db::hold(
                conn,
                api_key_id,
                &request_id,
                attempt,
                reserved_units,
                quoted,
            )
        },
        "credit intent hold",
    )
    .await
}

/// Close the intent of a request whose settlement landed (`billed`) or that
/// produced nothing billable (`unbilled`).
///
/// A failure here is not fatal: the intent stays `held` and the next admission
/// of the key reconciles it from the durable usage row (already debited, so no
/// second charge).
pub(super) async fn close_after_settle(
    conn: Arc<parking_lot::Mutex<Connection>>,
    reservation_id: i64,
    billed: bool,
) -> openproxy_types::Result<()> {
    spawn(
        conn,
        move |conn| {
            let (state, outcome, note) = if billed {
                (
                    db::STATE_SETTLED,
                    db::OUTCOME_BILLED,
                    "owner settled the request",
                )
            } else {
                (
                    db::STATE_RELEASED,
                    db::OUTCOME_UNBILLED,
                    "owner settled without a billable response",
                )
            };
            db::close(conn, reservation_id, state, outcome, 0, note).map(|_| ())
        },
        "credit intent close",
    )
    .await
}

/// Close the intent of a unary request once its usage row is durable.
///
/// A failure is only logged: the row stays `held` and the next admission of the
/// key reconciles it from the usage table (already debited, so no second
/// charge), which is why nothing here can block the key for good.
pub(super) async fn close_unary(
    db_pool: &openproxy_db::DbPool,
    lane: Option<&super::CreditsLaneGuard>,
    billed: bool,
) {
    let Some(reservation_id) = lane.and_then(super::CreditsLaneGuard::reservation_id) else {
        return;
    };
    if let Err(error) = close_after_settle(db_pool.writer_arc(), reservation_id, billed).await {
        tracing::warn!(
            %error,
            "credit settlement: unary intent left open for reconciliation"
        );
    }
}

/// Record that the first client-visible byte was sent. Public entry point for
/// the dispatcher: after this, a crash with no usage row is no longer assumed
/// to be "nothing delivered".
///
/// # Errors
/// Propagates DB failures.
pub async fn note_delivery_started(
    conn: Arc<parking_lot::Mutex<Connection>>,
    request_id: String,
    attempt: u8,
) -> openproxy_types::Result<()> {
    spawn(
        conn,
        move |conn| db::mark_delivery_started(conn, &request_id, attempt),
        "credit intent delivery start",
    )
    .await
}

async fn spawn<R, F>(
    conn: Arc<parking_lot::Mutex<Connection>>,
    operation: F,
    what: &'static str,
) -> openproxy_types::Result<R>
where
    R: Send + 'static,
    F: FnOnce(&Connection) -> openproxy_types::Result<R> + Send + 'static,
{
    match tokio::task::spawn_blocking(move || operation(&conn.lock())).await {
        Ok(result) => result,
        Err(error) => Err(CoreError::Internal(format!(
            "{what} failed to run: {error}"
        ))),
    }
}
