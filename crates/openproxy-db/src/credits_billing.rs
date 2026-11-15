//! Exactly-once debit for billable usage rows, shared by insert and callbacks.
use crate::error::map_db_error;
use openproxy_types::Result;
#[cfg(feature = "payments")]
use rusqlite::OptionalExtension;
use rusqlite::{Connection, Transaction, TransactionBehavior};

pub(crate) fn transaction<T>(
    conn: &Connection,
    op: impl FnOnce(&Connection) -> Result<T>,
) -> Result<T> {
    let owned = if conn.is_autocommit() {
        Some(
            Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
                .map_err(map_db_error)?,
        )
    } else {
        None
    };
    let result = op(conn)?;
    if let Some(tx) = owned {
        tx.commit().map_err(map_db_error)?;
    }
    Ok(result)
}

/// Caller owns the transaction. A paid response may exhaust, but never leave
/// a positive balance on an overrun: otherwise oversize requests are free forever.
///
/// Returns the amount **actually debited**, in micro-USD, or `None` when no
/// debit was attempted for this row (not billable, already marked, no key, zero
/// cost). The value is what the ledger moved — clamped to the live balance, and
/// `0` for a key that holds no prepaid credits — so it, not `cost_usd`, is the
/// only amount the dashboard may call "charged". It is written to
/// `usage.charged_units` in this same transaction as the marker, so a row can
/// never claim a debit without its amount.
#[cfg(feature = "payments")]
pub(crate) fn debit_usage(conn: &Connection, usage_id: i64) -> Result<Option<i64>> {
    let row: Option<(i64, i64)> = conn
        .query_row(
            "SELECT api_key_id, CAST(ROUND(cost_usd * 1000000) AS INTEGER) FROM usage \
         WHERE id = ?1 AND client_response = 1 AND credits_debited = 0 \
           AND api_key_id IS NOT NULL AND cost_usd > 0",
            [usage_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(map_db_error)?;
    let Some((key, cost)) = row else {
        return Ok(None);
    };
    // Zero is a real answer ("nothing was charged"), not a missing one: a key
    // that is not credit-based and an exhausted balance both land here.
    let mut charged = 0i64;
    if crate::credits_ledger::is_credit_based(conn, key)? {
        let balance = crate::credits_ledger::balance_units(conn, key)?;
        let debit = cost.min(balance).max(0);
        if debit > 0 {
            // The ledger clamps again and returns what it consumed; that is
            // the amount that actually left the balance.
            charged = crate::credits_ledger::record_spend(conn, key, debit)?;
        }
        if cost > balance {
            tracing::warn!(
                api_key_id = key,
                cost_units = cost,
                balance_units = balance,
                charged_units = charged,
                "credit overrun: remaining balance exhausted; further prepaid requests denied"
            );
        }
    }
    conn.execute(
        "UPDATE usage SET credits_debited = 1, charged_units = ?2 WHERE id = ?1",
        rusqlite::params![usage_id, charged],
    )
    .map_err(map_db_error)?;
    Ok(Some(charged))
}

/// Payments-disabled build: usage rows are recorded (and priced, for
/// analytics) but nothing debits a credit balance, because no credit tables
/// exist and no key is credit-based. `None` keeps `charged_units` NULL and the
/// marker clear, which the API reports as "not charged" — the truth for a build
/// that has no money at all.
#[cfg(not(feature = "payments"))]
pub(crate) fn debit_usage(_conn: &Connection, _usage_id: i64) -> Option<i64> {
    None
}

#[cfg(all(test, feature = "payments"))]
mod tests {
    use super::*;
    fn db() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::migrations::run(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO api_keys(key_hash) VALUES ('billing-fixture')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO credit_transactions(api_key_id,kind,amount_units,remaining_units) VALUES(1,'grant',1000000,1000000)", []).unwrap();
        conn.execute("INSERT INTO usage(request_id,trace_id,provider_id,upstream_model_id,attempt,race_total,race_lost,total_ms,status_code,client_response,cost_usd,api_key_id) VALUES('bill-fixture','fixture','fixture','fixture',1,1,0,1,200,1,0.25,1)", []).unwrap();
        conn
    }
    fn db_without_ledger() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::migrations::run(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO api_keys(key_hash) VALUES ('billing-no-ledger')",
            [],
        )
        .unwrap();
        // Real-world shape of the row that exposed the gap: a priced 200 with
        // a key that never had a credit_transactions entry.
        conn.execute("INSERT INTO usage(request_id,trace_id,provider_id,upstream_model_id,attempt,race_total,race_lost,total_ms,status_code,client_response,cost_usd,api_key_id) VALUES('bill-no-ledger','fixture','fixture','fixture',1,1,0,1,200,1,0.0146004,1)", []).unwrap();
        conn
    }
    fn charge_pair(conn: &Connection) -> (i64, Option<i64>, Option<f64>) {
        conn.query_row(
            "SELECT credits_debited, charged_units, cost_usd FROM usage",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    }
    #[test]
    fn callback_failure_rolls_back_billable_transition_too() {
        let conn = db();
        conn.execute("UPDATE usage SET client_response=0", [])
            .unwrap();
        conn.execute_batch("CREATE TRIGGER fail_callback BEFORE UPDATE ON credit_transactions BEGIN SELECT RAISE(ABORT, 'injected callback failure'); END").unwrap();
        assert!(
            crate::cost::mark_client_response(&conn, openproxy_types::ids::UsageId(1)).is_err()
        );
        let pair: (i64, i64) = conn
            .query_row(
                "SELECT client_response,credits_debited FROM usage",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(pair, (0, 0));
    }
    #[test]
    fn debit_and_repeated_callbacks_are_idempotent() {
        let conn = db();
        transaction(&conn, |c| debit_usage(c, 1)).unwrap();
        transaction(&conn, |c| debit_usage(c, 1)).unwrap();
        assert_eq!(
            crate::credits_ledger::balance_units(&conn, 1).unwrap(),
            750000
        );
    }
    #[test]
    fn overrun_drains_balance_instead_of_allowing_infinite_free_requests() {
        let conn = db();
        conn.execute("UPDATE usage SET cost_usd = 2", []).unwrap();
        transaction(&conn, |c| debit_usage(c, 1)).unwrap();
        assert_eq!(crate::credits_ledger::balance_units(&conn, 1).unwrap(), 0);
    }
    #[test]
    fn callbacks_share_one_debit_marker() {
        let conn = db();
        conn.execute("UPDATE usage SET client_response=0, combo_target_id=42", [])
            .unwrap();
        crate::cost::mark_winner_usage_row(
            &conn,
            "bill-fixture",
            1,
            openproxy_types::ids::ComboTargetId(42),
        )
        .unwrap();
        crate::cost::mark_client_response(&conn, openproxy_types::ids::UsageId(1)).unwrap();
        crate::cost::mark_winner_usage_row(
            &conn,
            "bill-fixture",
            1,
            openproxy_types::ids::ComboTargetId(42),
        )
        .unwrap();
        assert_eq!(
            crate::credits_ledger::balance_units(&conn, 1).unwrap(),
            750000
        );
    }
    #[test]
    fn debit_records_the_exact_amount_the_ledger_moved() {
        let conn = db();
        assert_eq!(
            transaction(&conn, |c| debit_usage(c, 1)).unwrap(),
            Some(250_000),
            "0.25 USD is 250000 micro-USD, not the price string"
        );
        assert_eq!(charge_pair(&conn), (1, Some(250_000), Some(0.25)));
        // Idempotent: the second callback returns "no attempt" and leaves the
        // recorded amount exactly as the first one wrote it.
        assert_eq!(transaction(&conn, |c| debit_usage(c, 1)).unwrap(), None);
        assert_eq!(charge_pair(&conn).1, Some(250_000));
        assert_eq!(
            crate::credits_ledger::balance_units(&conn, 1).unwrap(),
            750_000
        );
    }
    #[test]
    fn overrun_records_the_clamped_amount_not_the_price() {
        let conn = db();
        conn.execute("UPDATE usage SET cost_usd = 2", []).unwrap();
        assert_eq!(
            transaction(&conn, |c| debit_usage(c, 1)).unwrap(),
            Some(1_000_000),
            "the ledger only moved the remaining balance"
        );
        let (marker, charged, cost) = charge_pair(&conn);
        assert_eq!(marker, 1);
        assert_eq!(charged, Some(1_000_000));
        assert_eq!(cost, Some(2.0), "the calculated price is never rewritten");
    }
    #[test]
    fn key_without_prepaid_credits_records_zero_instead_of_the_price() {
        let conn = db_without_ledger();
        // This is the row shape the dashboard used to mislabel: the marker said
        // "debited" and only the (non-existent) amount could tell the truth.
        assert_eq!(transaction(&conn, |c| debit_usage(c, 1)).unwrap(), Some(0));
        let (marker, charged, cost) = charge_pair(&conn);
        assert_eq!(marker, 1, "the debit path did run");
        assert_eq!(charged, Some(0), "nothing was charged");
        assert!(
            cost.unwrap() > 0.0,
            "cost_usd still carries the calculated price"
        );
        assert_eq!(
            crate::credits_ledger::balance_units(&conn, 1).unwrap(),
            0,
            "no ledger rows were created"
        );
    }
    #[test]
    fn unbillable_row_records_no_attempt_and_no_amount() {
        let conn = db();
        conn.execute("UPDATE usage SET client_response = 0", [])
            .unwrap();
        assert_eq!(transaction(&conn, |c| debit_usage(c, 1)).unwrap(), None);
        assert_eq!(charge_pair(&conn), (0, None, Some(0.25)));
        assert_eq!(
            crate::credits_ledger::balance_units(&conn, 1).unwrap(),
            1_000_000
        );
    }
    #[test]
    fn failed_debit_rolls_back_marker_and_can_retry() {
        let conn = db();
        conn.execute_batch("CREATE TRIGGER fail_debit BEFORE UPDATE ON credit_transactions BEGIN SELECT RAISE(ABORT, 'injected debit failure'); END").unwrap();
        assert!(transaction(&conn, |c| debit_usage(c, 1)).is_err());
        assert_eq!(
            conn.query_row("SELECT credits_debited FROM usage", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            charge_pair(&conn).1,
            None,
            "a rolled-back debit leaves no amount behind"
        );
        conn.execute_batch("DROP TRIGGER fail_debit").unwrap();
        transaction(&conn, |c| debit_usage(c, 1)).unwrap();
        assert_eq!(
            crate::credits_ledger::balance_units(&conn, 1).unwrap(),
            750000
        );
    }
}
