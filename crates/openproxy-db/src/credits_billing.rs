//! Exactly-once debit for billable usage rows, shared by insert and callbacks.
use crate::error::map_db_error;
use openproxy_types::Result;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};

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
pub(crate) fn debit_usage(conn: &Connection, usage_id: i64) -> Result<()> {
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
        return Ok(());
    };
    if crate::credits_ledger::is_credit_based(conn, key)? {
        let balance = crate::credits_ledger::balance_units(conn, key)?;
        let debit = cost.min(balance).max(0);
        if debit > 0 {
            crate::credits_ledger::record_spend(conn, key, debit)?;
        }
        if cost > balance {
            tracing::warn!(
                api_key_id = key,
                cost_units = cost,
                balance_units = balance,
                "credit overrun: remaining balance exhausted; further prepaid requests denied"
            );
        }
    }
    conn.execute(
        "UPDATE usage SET credits_debited = 1 WHERE id = ?1",
        [usage_id],
    )
    .map_err(map_db_error)?;
    Ok(())
}

#[cfg(test)]
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
        conn.execute_batch("DROP TRIGGER fail_debit").unwrap();
        transaction(&conn, |c| debit_usage(c, 1)).unwrap();
        assert_eq!(
            crate::credits_ledger::balance_units(&conn, 1).unwrap(),
            750000
        );
    }
}
