use super::{seed_key, test_conn};
use rusqlite::params;

#[test]
fn sweep_deletes_keys_idle_for_six_months() {
    let conn = test_conn();
    let idle = seed_key(&conn, "idle-8mo");
    let used_recently = seed_key(&conn, "used-3mo-ago");
    let young = seed_key(&conn, "young");
    let operator = seed_key(&conn, "operator");

    conn.execute(
        "UPDATE api_keys SET scopes_json = '[\"chat\",\"manage\"]' WHERE id = ?1",
        params![operator.0],
    )
    .unwrap();

    // Age three keys beyond the window; keep `young` inside it.
    conn.execute(
        "UPDATE api_keys SET created_at = datetime('now', '-8 months')
         WHERE id IN (?1, ?2, ?3)",
        params![idle.0, used_recently.0, operator.0],
    )
    .unwrap();

    // `used_recently` has a request 3 months ago (inside the window) plus a
    // stale one from 9 months ago: the recent one saves it.
    for (rid, age) in [("recent", "-3 months"), ("stale", "-9 months")] {
        conn.execute(
            "INSERT INTO usage (api_key_id, request_id, trace_id, attempt, provider_id, 
             upstream_model_id, status_code, total_ms, created_at)
             VALUES (?1, ?2, 't', 1, 'p', 'm', 200, 10, datetime('now', ?3))",
            params![used_recently.0, rid, age],
        )
        .unwrap();
    }

    // The idle key holds a small balance that must die with it.
    conn.execute(
        "INSERT INTO credit_transactions (api_key_id, kind, amount_units, remaining_units, status)
         VALUES (?1, 'topup', 500, 500, 'active')",
        params![idle.0],
    )
    .unwrap();
    // ...and carries a quota row (must cascade).
    crate::api_key_quotas::upsert(
        &conn,
        idle,
        &crate::api_key_quotas::QuotaCaps {
            weekly_usd_cap: Some(5.0),
            five_hour_usd_cap: None,
        },
    )
    .unwrap();

    let swept = crate::credits::sweep::sweep_idle_keys(&conn, 100).expect("sweep");
    assert_eq!(swept.len(), 1, "only the truly idle key is swept");
    assert_eq!(swept[0].id, idle.0);
    assert_eq!(
        swept[0].balance_units, 500,
        "the report carries the dead balance"
    );

    // Gone: key row, quota row, ledger rows.
    let alive: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM api_keys WHERE id = ?1",
            params![idle.0],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(alive, 0);
    let quotas: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM api_key_quotas WHERE api_key_id = ?1",
            params![idle.0],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(quotas, 0);
    let ledger: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM credit_transactions WHERE api_key_id = ?1",
            params![idle.0],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(ledger, 1, "accounting evidence survives the key");
    assert_eq!(
        crate::credits::ledger::balance_units(&conn, idle).unwrap(),
        0
    );

    // Survivors: young, recently-used, and the manage-scoped operator key.
    let survivors: i64 = conn
        .query_row("SELECT COUNT(*) FROM api_keys", [], |r| r.get(0))
        .unwrap();
    assert_eq!(survivors, 3);
    assert!(!crate::credits::sweep::is_idle(&conn, used_recently.0).unwrap());
    assert!(!crate::credits::sweep::is_idle(&conn, young.0).unwrap());
    assert!(!crate::credits::sweep::is_idle(&conn, operator.0).unwrap());
}

#[test]
fn sweep_respects_limit_per_pass() {
    let conn = test_conn();
    let mut ids = Vec::new();
    for n in 0..3 {
        let key = seed_key(&conn, &format!("old-{n}"));
        conn.execute(
            "UPDATE api_keys SET created_at = datetime('now', '-8 months') WHERE id = ?1",
            params![key.0],
        )
        .unwrap();
        ids.push(key);
    }
    let swept = crate::credits::sweep::sweep_idle_keys(&conn, 2).expect("sweep");
    assert_eq!(swept.len(), 2, "limit caps the pass");
    let left: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM api_keys WHERE created_at <= datetime('now', '-6 months')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(left, 1, "the remaining idle key waits for the next pass");
}
