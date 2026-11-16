//! Durable per-request credit intents (SQL layer).
//!
//! One row per admitted prepaid request, committed **before** anything is
//! dispatched and closed by that request's settlement. It is the cross-process
//! half of the credit gate: the process-wide lane map only serializes requests
//! of one process, and the per-key interprocess lock only lasts while the owner
//! is alive. When the owner dies (crash, `SIGKILL`, task abort), the OS drops
//! the lock but the row stays, and the next admission of that key — which is
//! guaranteed to be the only live owner — reconciles it from durable evidence
//! (`usage` rows) instead of trusting a stale in-memory marker.
//!
//! The row is **not** a charge and **not** a ledger reservation of lot units:
//! the balance is only read, never split, so an unused hold needs no refund and
//! keeps the original lot expiry. See migration `000124_credit_reservations.sql`
//! for the exact reconciliation policy.

use openproxy_types::CoreError;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

/// Open: the request may still turn into a charge.
pub const STATE_HELD: &str = "held";
/// Closed by the owner after its debit (or its clean no-charge outcome) landed.
pub const STATE_SETTLED: &str = "settled";
/// Closed without any charge, from evidence that nothing was delivered.
pub const STATE_RELEASED: &str = "released";

/// The delivered response was accounted for.
pub const OUTCOME_BILLED: &str = "billed";
/// The owner finished without a billable response.
pub const OUTCOME_UNBILLED: &str = "unbilled";
/// A recovering admission charged the durable usage row it found.
pub const OUTCOME_RECOVERED_BILLED: &str = "recovered_billed";
/// A recovering admission charged the durable delivered-cost checkpoint after a
/// started stream lost its usage row. Always carries `charged_units` in the
/// audit columns; never charges output the model did not produce.
pub const OUTCOME_RECOVERED_PARTIAL: &str = "recovered_partial";
/// A recovering admission closed the intent without a charge.
pub const OUTCOME_RECOVERED_UNBILLED: &str = "recovered_unbilled";

/// One intent that was never closed by its owner.
#[derive(Clone, Debug, Serialize)]
pub struct OpenReservation {
    pub id: i64,
    pub request_id: String,
    pub attempt: u8,
    pub reserved_units: i64,
    pub delivery_started: bool,
    /// Highest amount of produced output the worker durably checkpointed before a
    /// client-visible chunk. The only quantity a partial recovery may charge.
    pub generated_units: i64,
    pub created_at: String,
}

/// What the durable `usage` table proves about one request id.
#[derive(Clone, Debug)]
pub struct UsageEvidence {
    pub row_id: i64,
    pub attempt: u8,
    pub target_id: Option<i64>,
    pub cost_units: i64,
    pub debited: bool,
    /// The row was already confirmed as reaching the client.
    pub client_response: bool,
}

/// Open the intent for a request that is about to be dispatched, returning its
/// id.
///
/// Idempotent per `(request_id, attempt)` and **never destructive**: a retry of an
/// open intent of the same key may refresh its budget, but a closed intent stays
/// closed (and is returned as-is) and an intent owned by another key is refused
/// instead of being adopted. Evidence that was already written can therefore not
/// be reset by a repeated or foreign admission.
///
/// # Errors
/// Propagates DB failures and refuses a `(request_id, attempt)` already owned by
/// a different `api_key_id`; callers fail closed.
pub fn hold(
    conn: &Connection,
    api_key_id: i64,
    request_id: &str,
    attempt: u8,
    reserved_units: i64,
    quoted: bool,
) -> openproxy_types::Result<i64> {
    let reopened: Option<i64> = conn
        .query_row(
            "INSERT INTO credit_reservations(api_key_id, request_id, attempt, reserved_units, quoted) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(request_id, attempt) DO UPDATE SET \
                 reserved_units = excluded.reserved_units, \
                 quoted = excluded.quoted \
             WHERE credit_reservations.api_key_id = excluded.api_key_id \
               AND credit_reservations.state = 'held' \
             RETURNING id",
            params![
                api_key_id,
                request_id,
                i64::from(attempt),
                reserved_units,
                i64::from(quoted)
            ],
            |row| row.get(0),
        )
        .optional()
        .map_err(crate::error::map_db_error_ctx("credit reservation hold"))?;
    if let Some(id) = reopened {
        return Ok(id);
    }

    // No update: the row is closed (idempotent retry) or belongs to another key.
    let existing: Option<(i64, i64)> = conn
        .query_row(
            "SELECT id, api_key_id FROM credit_reservations \
             WHERE request_id = ?1 AND attempt = ?2",
            params![request_id, i64::from(attempt)],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(crate::error::map_db_error_ctx("credit reservation lookup"))?;
    match existing {
        Some((id, owner)) if owner == api_key_id => Ok(id),
        Some((_, owner)) => Err(CoreError::Internal(format!(
            "credit reservation for request {request_id} belongs to key {owner}, not {api_key_id}"
        ))),
        None => Err(CoreError::Internal(format!(
            "credit reservation for request {request_id} vanished while holding it"
        ))),
    }
}

/// Every intent of this key that its owner never closed.
///
/// # Errors
/// Propagates DB failures.
pub fn open(conn: &Connection, api_key_id: i64) -> openproxy_types::Result<Vec<OpenReservation>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, request_id, attempt, reserved_units, delivery_started, generated_units, \
                    created_at \
             FROM credit_reservations WHERE api_key_id = ?1 AND state = 'held' ORDER BY id ASC",
        )
        .map_err(crate::error::map_db_error)?;
    let rows = stmt
        .query_map(params![api_key_id], |row| {
            Ok(OpenReservation {
                id: row.get(0)?,
                request_id: row.get(1)?,
                attempt: u8::try_from(row.get::<_, i64>(2)?).unwrap_or(1),
                reserved_units: row.get(3)?,
                delivery_started: row.get::<_, i64>(4)? != 0,
                generated_units: row.get(5)?,
                created_at: row.get(6)?,
            })
        })
        .map_err(crate::error::map_db_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(crate::error::map_db_error)?;
    Ok(rows)
}

/// Close an open intent exactly once. Returns `false` when it was already
/// closed, so a double close can never account the same row twice.
///
/// # Errors
/// Propagates DB failures.
pub fn close(
    conn: &Connection,
    id: i64,
    state: &str,
    outcome: &str,
    charged_units: i64,
    note: &str,
) -> openproxy_types::Result<bool> {
    let changed = conn
        .execute(
            "UPDATE credit_reservations SET state = ?1, outcome = ?2, charged_units = ?3, \
             note = ?4, closed_at = datetime('now') WHERE id = ?5 AND state = 'held'",
            params![state, outcome, charged_units, note, id],
        )
        .map_err(crate::error::map_db_error_ctx("credit reservation close"))?;
    Ok(changed != 0)
}

/// Durable proof that the client already received something from this request.
///
/// Called before the first client-visible chunk (streaming) or response
/// body/unary result. After a crash this is what separates "nothing was
/// delivered, release" from "a stream started, a lost usage row is possible".
///
/// # Errors
/// Propagates DB failures; a caller that cannot record the flag must treat the
/// request as non-deliverable rather than dispatch without it.
pub fn mark_delivery_started(
    conn: &Connection,
    request_id: &str,
    attempt: u8,
) -> openproxy_types::Result<()> {
    conn.execute(
        "UPDATE credit_reservations SET delivery_started = 1, \
         delivery_started_at = datetime('now') \
         WHERE request_id = ?1 AND attempt = ?2 AND state = 'held' AND delivery_started = 0",
        params![request_id, i64::from(attempt)],
    )
    .map_err(crate::error::map_db_error_ctx(
        "credit reservation delivery_started",
    ))?;
    Ok(())
}

/// Durable checkpoint of how much billable output this request has already
/// produced, recorded **before** the chunk that carries it is released to the
/// client. `MAX` keeps it monotonic: a late or repeated report can never lower
/// the evidence, and a reconciled (closed) intent or another tenant's request id
/// matches nothing.
///
/// This is the only quantity a partial recovery may charge. A stream that
/// started but checkpointed nothing produced nothing verifiable, so its intent
/// is released instead of being billed for the admitted maximum.
///
/// # Errors
/// Propagates DB failures. Returns `false` when no open intent matched; the
/// caller must stop delivering rather than write evidence nowhere.
pub fn checkpoint(
    conn: &Connection,
    api_key_id: i64,
    request_id: &str,
    attempt: u8,
    delivered_units: i64,
) -> openproxy_types::Result<bool> {
    let changed = conn
        .execute(
            "UPDATE credit_reservations SET \
                 generated_units = MAX(generated_units, ?4), \
                 generated_at = datetime('now'), \
                 delivery_started = 1, \
                 delivery_started_at = COALESCE(delivery_started_at, datetime('now')) \
             WHERE api_key_id = ?1 AND request_id = ?2 AND attempt = ?3 AND state = 'held'",
            params![
                api_key_id,
                request_id,
                i64::from(attempt),
                delivered_units.max(0)
            ],
        )
        .map_err(crate::error::map_db_error_ctx(
            "credit reservation checkpoint",
        ))?;
    Ok(changed != 0)
}

/// The durable usage row that proves a delivered response for this request.
///
/// Race losers are excluded (`race_lost = 1` recorded a response that never
/// reached the client) and the row must belong to this key, so a conflicting
/// request id can never bill another tenant. A row already confirmed as
/// delivered wins over a merely successful one.
///
/// # Errors
/// Propagates DB failures.
pub fn delivered_usage(
    conn: &Connection,
    request_id: &str,
    api_key_id: i64,
) -> openproxy_types::Result<Option<UsageEvidence>> {
    conn.query_row(
        "SELECT id, attempt, combo_target_id, \
                CAST(ROUND(COALESCE(cost_usd, 0) * 1000000) AS INTEGER), credits_debited, \
                client_response \
         FROM usage \
         WHERE request_id = ?1 AND api_key_id = ?2 \
           AND (status_code BETWEEN 200 AND 299 OR status_code = 499) \
           AND race_lost = 0 \
         ORDER BY client_response DESC, id DESC LIMIT 1",
        params![request_id, api_key_id],
        |row| {
            Ok(UsageEvidence {
                row_id: row.get(0)?,
                attempt: u8::try_from(row.get::<_, i64>(1)?).unwrap_or(1),
                target_id: row.get(2)?,
                cost_units: row.get(3)?,
                debited: row.get::<_, i64>(4)? != 0,
                client_response: row.get::<_, i64>(5)? != 0,
            })
        },
    )
    .optional()
    .map_err(crate::error::map_db_error_ctx(
        "credit reservation usage evidence",
    ))
}

/// Units currently held by open intents of this key. Diagnostic: after a
/// reconciliation it must be zero, because a hold never outlives the owner.
///
/// # Errors
/// Propagates DB failures.
pub fn reserved_units(conn: &Connection, api_key_id: i64) -> openproxy_types::Result<i64> {
    conn.query_row(
        "SELECT COALESCE(SUM(reserved_units), 0) FROM credit_reservations \
         WHERE api_key_id = ?1 AND state = 'held'",
        params![api_key_id],
        |row| row.get(0),
    )
    .map_err(crate::error::map_db_error)
}

/// Fail-closed helper: a database that cannot record the intent must not
/// dispatch. Kept here so callers in both crates classify the error the same
/// way.
#[must_use]
pub fn hold_blocked(error: &CoreError) -> CoreError {
    CoreError::Internal(format!(
        "credit ledger: could not record the request intent before dispatch: {error}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::migrations::run(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO api_keys(key_hash) VALUES ('reservation-fixture')",
            [],
        )
        .unwrap();
        conn
    }

    #[test]
    fn hold_is_idempotent_and_close_is_once_only() {
        let conn = db();
        let first = hold(&conn, 1, "req-1", 1, 500, false).unwrap();
        let again = hold(&conn, 1, "req-1", 1, 400, true).unwrap();
        assert_eq!(first, again, "re-admission reuses the intent row");
        let held = open(&conn, 1).unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(
            held[0].reserved_units, 400,
            "an open intent may refresh its budget"
        );

        assert!(close(&conn, first, STATE_SETTLED, OUTCOME_BILLED, 400, "test").unwrap());
        assert!(
            !close(&conn, first, STATE_SETTLED, OUTCOME_BILLED, 400, "test").unwrap(),
            "a closed intent cannot be accounted twice"
        );
        assert!(open(&conn, 1).unwrap().is_empty());
        assert_eq!(reserved_units(&conn, 1).unwrap(), 0);

        // A retry of a finished request never re-arms its evidence.
        assert_eq!(hold(&conn, 1, "req-1", 1, 900, true).unwrap(), first);
        assert!(
            open(&conn, 1).unwrap().is_empty(),
            "a closed intent must stay closed"
        );
        let state: String = conn
            .query_row(
                "SELECT state FROM credit_reservations WHERE id = ?1",
                [first],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "settled");
    }

    #[test]
    fn an_intent_cannot_be_adopted_by_another_key() {
        let conn = db();
        hold(&conn, 1, "req-tenant", 1, 500, false).unwrap();
        let error = hold(&conn, 2, "req-tenant", 1, 500, false).expect_err("cross-tenant hold");
        assert!(error.to_string().contains("belongs to key 1"), "{error}");
        let owner: i64 = conn
            .query_row(
                "SELECT api_key_id FROM credit_reservations WHERE request_id = 'req-tenant'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(owner, 1, "ownership is immutable");
    }

    #[test]
    fn checkpoints_are_monotonic_and_imply_delivery() {
        let conn = db();
        hold(&conn, 1, "req-ckpt", 1, 900, true).unwrap();
        assert!(checkpoint(&conn, 1, "req-ckpt", 1, 300).unwrap());
        assert!(
            checkpoint(&conn, 1, "req-ckpt", 1, 120).unwrap(),
            "a lower report still matches"
        );
        let held = open(&conn, 1).unwrap();
        assert_eq!(
            held[0].generated_units, 300,
            "a later lower checkpoint cannot shrink recorded output"
        );
        assert!(held[0].delivery_started);
        assert_eq!(
            held[0].reserved_units, 900,
            "the admitted budget is never overwritten by a checkpoint"
        );

        close(
            &conn,
            held[0].id,
            STATE_SETTLED,
            OUTCOME_BILLED,
            300,
            "test",
        )
        .unwrap();
        assert!(
            !checkpoint(&conn, 1, "req-ckpt", 1, 500).unwrap(),
            "a closed intent accepts no more evidence"
        );
        assert!(
            !checkpoint(&conn, 2, "req-ckpt", 1, 500).unwrap(),
            "another key matches nothing"
        );
    }

    #[test]
    fn delivered_usage_ignores_race_losers_and_other_keys() {
        let conn = db();
        conn.execute("INSERT INTO api_keys(key_hash) VALUES ('other-tenant')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO usage(request_id, trace_id, provider_id, upstream_model_id, attempt, \
             combo_target_id, race_total, race_lost, total_ms, status_code, cost_usd, api_key_id) \
             VALUES ('req-3', 'fixture', 'fixture', 'fixture', 1, 9, 2, 1, 1, 200, 0.75, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO usage(request_id, trace_id, provider_id, upstream_model_id, attempt, \
             combo_target_id, race_total, race_lost, total_ms, status_code, cost_usd, api_key_id) \
             VALUES ('req-3', 'fixture', 'fixture', 'fixture', 2, 9, 2, 0, 1, 200, 0.25, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO usage(request_id, trace_id, provider_id, upstream_model_id, attempt, \
             combo_target_id, race_total, race_lost, total_ms, status_code, cost_usd, api_key_id) \
             VALUES ('req-3', 'fixture', 'fixture', 'fixture', 3, 9, 1, 0, 1, 200, 0.5, 2)",
            [],
        )
        .unwrap();

        let evidence = delivered_usage(&conn, "req-3", 1)
            .unwrap()
            .expect("evidence");
        assert_eq!(evidence.attempt, 2, "the race loser is not billable");
        assert_eq!(evidence.cost_units, 250_000, "not the loser's cost");
        assert!(!evidence.client_response);
        assert!(
            delivered_usage(&conn, "req-3", 3).unwrap().is_none(),
            "another tenant's rows are never billed"
        );
        assert!(delivered_usage(&conn, "missing", 1).unwrap().is_none());
    }
}
