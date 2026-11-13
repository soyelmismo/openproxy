//! Upgrade and FIFO regressions against isolated in-memory SQLite only.
use rusqlite::Connection;

fn db() -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    crate::migrations::run(&mut conn).unwrap();
    conn
}

#[test]
fn finite_lots_are_spent_before_nonexpiring_lots() {
    let conn = db();
    conn.execute_batch("INSERT INTO credit_transactions(api_key_id,kind,amount_units,remaining_units,expires_at) VALUES
        (1,'grant',100,100,NULL), (1,'grant',100,100,datetime('now','+1 day'));").unwrap();
    crate::credits_ledger::record_spend(&conn, 1, 75).unwrap();
    let left: Vec<i64> = conn
        .prepare("SELECT remaining_units FROM credit_transactions ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(left, vec![100, 25]);
}

#[test]
fn nonfunding_rows_cannot_fabricate_spendable_balance() {
    let conn = db();
    conn.execute_batch("INSERT INTO credit_transactions(api_key_id,kind,amount_units,remaining_units) VALUES(1,'spend',100,100)").unwrap();
    assert_eq!(crate::credits_ledger::balance_units(&conn, 1).unwrap(), 0);
}

#[test]
fn checkout_migration_links_historical_mints_and_prevents_usage_recharge() {
    let mut conn = Connection::open_in_memory().unwrap();
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.extension().is_some_and(|x| x == "sql")
                && p.file_name().unwrap().to_string_lossy().starts_with("000")
                && p.file_name().unwrap().to_string_lossy().as_ref() < "000121"
        })
        .collect();
    paths.sort();
    conn.execute_batch("PRAGMA foreign_keys=OFF; CREATE TABLE schema_migrations(version INTEGER PRIMARY KEY,applied_at TEXT NOT NULL DEFAULT(datetime('now')))").unwrap();
    for path in paths {
        conn.execute_batch(&std::fs::read_to_string(&path).unwrap())
            .unwrap();
        let version: i64 = path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .split('_')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        conn.execute(
            "INSERT INTO schema_migrations(version) VALUES(?1)",
            [version],
        )
        .unwrap();
    }
    conn.execute_batch("INSERT INTO credit_orders(order_code,amount_units,base_units,ttl_days,status,expires_at) VALUES('OP-UPGRADE',5000000,5000000,7,'minted',datetime('now'));
        INSERT INTO credit_transactions(api_key_id,order_id,kind,amount_units,remaining_units) VALUES(42,1,'topup',5000000,5000000);
        INSERT INTO usage(request_id,trace_id,provider_id,upstream_model_id,attempt,race_total,race_lost,total_ms,status_code,client_response) VALUES('migration-usage','fixture','fixture','fixture',1,1,0,1,200,1)").unwrap();
    crate::migrations::run(&mut conn).unwrap();
    assert_eq!(
        conn.query_row("SELECT api_key_id FROM credit_orders", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        42
    );
    assert_eq!(
        conn.query_row("SELECT credits_debited FROM usage", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    crate::migrations::run(&mut conn).unwrap();
    assert!(
        crate::credits_ledger::reconcile(&conn)
            .unwrap()
            .is_consistent()
    );
}
