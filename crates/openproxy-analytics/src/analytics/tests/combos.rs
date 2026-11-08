//! Combo health aggregate: request-level success vs client-visible errors.

use super::fresh_conn;
use crate::usage::analytics::filter::UsageFilter;
use crate::usage::analytics::{ByComboRow, by_combo};
use rusqlite::params;

fn insert_row(conn: &rusqlite::Connection, request_id: &str, combo_id: Option<i64>, status: i64) {
    insert_row_ms(conn, request_id, combo_id, status, 10);
}

fn insert_row_ms(
    conn: &rusqlite::Connection,
    request_id: &str,
    combo_id: Option<i64>,
    status: i64,
    total_ms: i64,
) {
    conn.execute(
        "INSERT INTO usage (\
            request_id, trace_id, attempt, provider_id, account_id, combo_id, \
            upstream_model_id, prompt_tokens, completion_tokens, cost_usd, \
            total_ms, status_code, race_total, race_lost, created_at\
         ) VALUES (?1, 't', 1, 'prov', NULL, ?2, 'model', 0, 0, 0.0, ?4, ?3, 1, 0, datetime('now'))",
        params![request_id, combo_id, status, total_ms],
    )
    .expect("insert usage row");
}

fn row_for(rows: &[ByComboRow], combo_id: i64) -> &ByComboRow {
    rows.iter()
        .find(|r| r.combo_id == combo_id)
        .unwrap_or_else(|| panic!("combo {combo_id} missing from {rows:?}"))
}

#[test]
fn retries_are_not_errors_and_uncovered_requests_are() {
    let (conn, _) = fresh_conn();
    conn.execute("INSERT INTO combos (id, name, strategy) VALUES (1, 'ninja', 'priority')", [])
        .expect("insert combo");

    // req-a: one retry, then success -> successful request, 2 attempts.
    insert_row(&conn, "req-a", Some(1), 429);
    insert_row(&conn, "req-a", Some(1), 200);
    // req-b: never covered -> client-visible error, 1 attempt.
    insert_row(&conn, "req-b", Some(1), 500);
    // req-c: two retries, then success -> successful request, 3 attempts.
    insert_row(&conn, "req-c", Some(1), 429);
    insert_row(&conn, "req-c", Some(1), 503);
    insert_row(&conn, "req-c", Some(1), 200);

    let rows = by_combo(&conn, &UsageFilter::default()).expect("by_combo");
    assert_eq!(rows.len(), 1, "only combo rows aggregate: {rows:?}");
    let r = row_for(&rows, 1);
    assert_eq!(r.combo_name, "ninja");
    assert_eq!(r.requests, 3);
    assert_eq!(r.ok_requests, 2, "retried-then-successful requests count as ok");
    assert_eq!(r.error_requests, 1, "only the uncovered request is an error");
    assert_eq!(r.total_attempts, 6);
    assert!((r.success_rate_pct - 66.67).abs() < 0.01, "{}", r.success_rate_pct);
    assert!((r.error_rate_pct - 33.33).abs() < 0.01, "{}", r.error_rate_pct);
    assert!((r.avg_attempts - 2.0).abs() < 0.01, "{}", r.avg_attempts);
}

#[test]
fn combos_are_isolated_and_direct_rows_are_excluded() {
    let (conn, _) = fresh_conn();
    conn.execute(
        "INSERT INTO combos (id, name, strategy) VALUES (1, 'a', 'priority'), (2, 'b', 'priority')",
        [],
    )
    .expect("insert combos");

    insert_row(&conn, "req-a", Some(1), 200);
    insert_row(&conn, "req-b", Some(2), 500);
    // Direct (non-combo) traffic must not appear in the combo table.
    insert_row(&conn, "req-c", None, 200);
    insert_row(&conn, "req-d", Some(0), 200);

    let rows = by_combo(&conn, &UsageFilter::default()).expect("by_combo");
    assert_eq!(rows.len(), 2, "one row per combo: {rows:?}");
    let a = row_for(&rows, 1);
    assert_eq!((a.requests, a.ok_requests, a.error_requests), (1, 1, 0));
    assert!((a.success_rate_pct - 100.0).abs() < 0.01);
    let b = row_for(&rows, 2);
    assert_eq!((b.requests, b.ok_requests, b.error_requests), (1, 0, 1));
    assert!((b.error_rate_pct - 100.0).abs() < 0.01);
}

#[test]
fn date_filter_applies_before_request_grouping() {
    let (conn, _) = fresh_conn();
    conn.execute("INSERT INTO combos (id, name, strategy) VALUES (1, 'c', 'priority')", [])
        .expect("insert combo");

    insert_row(&conn, "old-ok", Some(1), 200);
    conn.execute(
        "UPDATE usage SET created_at = datetime('now', '-10 days') WHERE request_id = 'old-ok'",
        [],
    )
    .expect("age row");
    insert_row(&conn, "new-fail", Some(1), 500);

    let filter = UsageFilter {
        from: Some(
            chrono::Utc::now()
                .checked_sub_signed(chrono::Duration::days(2))
                .unwrap()
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string(),
        ),
        ..UsageFilter::default()
    };
    let rows = by_combo(&conn, &filter).expect("by_combo");
    let r = row_for(&rows, 1);
    assert_eq!(r.requests, 1, "old request filtered out: {r:?}");
    assert_eq!(r.error_requests, 1);
}

#[test]
fn average_times_separate_success_from_absolute_failure() {
    let (conn, _) = fresh_conn();
    conn.execute("INSERT INTO combos (id, name, strategy) VALUES (1, 'c', 'priority')", [])
        .expect("insert combo");

    // Successful request: 2 attempts of 1000ms + 2000ms = 3000ms waited.
    insert_row_ms(&conn, "ok-1", Some(1), 429, 1_000);
    insert_row_ms(&conn, "ok-1", Some(1), 200, 2_000);
    // Successful request: single 4000ms attempt.
    insert_row_ms(&conn, "ok-2", Some(1), 200, 4_000);
    // Absolute failure: 3 attempts of 10s + 20s + 30s = 60s before giving up.
    insert_row_ms(&conn, "fail-1", Some(1), 429, 10_000);
    insert_row_ms(&conn, "fail-1", Some(1), 503, 20_000);
    insert_row_ms(&conn, "fail-1", Some(1), 529, 30_000);

    let rows = by_combo(&conn, &UsageFilter::default()).expect("by_combo");
    let r = row_for(&rows, 1);
    assert_eq!((r.ok_requests, r.error_requests), (2, 1));
    // Successes: (3000 + 4000) / 2.
    assert_eq!(r.avg_ok_ms, Some(3_500.0), "median of successful waits");
    // The failure carries the whole exhausting path, not one attempt.
    assert_eq!(r.avg_error_ms, Some(60_000.0), "full time until the client error");
}

#[test]
fn a_combo_with_only_successes_reports_no_failure_time() {
    let (conn, _) = fresh_conn();
    conn.execute("INSERT INTO combos (id, name, strategy) VALUES (1, 'c', 'priority')", [])
        .expect("insert combo");
    insert_row_ms(&conn, "ok", Some(1), 200, 1_500);

    let rows = by_combo(&conn, &UsageFilter::default()).expect("by_combo");
    let r = row_for(&rows, 1);
    assert_eq!(r.avg_ok_ms, Some(1_500.0));
    assert_eq!(r.avg_error_ms, None, "no failures -> no failure time");
}
