//! Tests for per-API-key dollar quotas.
//!
//! Spend rows are inserted directly into `usage` with explicit `created_at`
//! (SQLite "YYYY-MM-DD HH:MM:SS" UTC) so the window boundaries are stable
//! regardless of test runtime.

use super::*;
use crate::api_keys;
use openproxy_types::ids::ApiKeyId as ApiIdAlias;
use rusqlite::params;

fn fresh_conn() -> Connection {
    openproxy_db::testing::open_in_memory()
}

fn make_key(conn: &Connection, label: &str) -> i64 {
    let input = api_keys::CreateApiKeyInput {
        label: Some(label.to_string()),
        scopes: vec!["chat".to_string()],
        ..Default::default()
    };
    let (key, _plaintext) = api_keys::create(conn, input, "test").expect("create key");
    key.id.0
}

/// Insert a served-response usage row (`client_response = 1`) with a given
/// cost and age offset (seconds before now).
fn insert_spend(conn: &Connection, key_id: i64, cost: f64, age_secs: i64) {
    let created = (chrono::Utc::now() - chrono::Duration::seconds(age_secs))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    conn.execute(
        "INSERT INTO usage (request_id, trace_id, provider_id, upstream_model_id, \
          status_code, cost_usd, client_response, api_key_id, created_at) \
         VALUES (?1, ?1, 'test', 'test-model', 200, ?2, 1, ?3, ?4)",
        params![
            format!("req-{key_id}-{cost}-{age_secs}"),
            cost,
            key_id,
            created
        ],
    )
    .expect("insert usage");
}

/// Insert an attempt row that never reached the client.
fn insert_failed_attempt(conn: &Connection, key_id: i64, cost: f64) {
    conn.execute(
        "INSERT INTO usage (request_id, trace_id, provider_id, upstream_model_id, \
          status_code, cost_usd, client_response, api_key_id) \
         VALUES (?1, ?1, 'test', 'test-model', 500, ?2, 0, ?3)",
        params![format!("req-fail-{key_id}-{cost}"), cost, key_id],
    )
    .expect("insert usage");
}

#[test]
fn no_row_means_unlimited() {
    let conn = fresh_conn();
    let key = make_key(&conn, "no-quota");
    let caps = get_caps(&conn, ApiIdAlias(key)).expect("ok");
    assert!(caps.is_none());
    let st = status(&conn, ApiIdAlias(key)).expect("ok");
    assert!(!st.enabled);
    assert!(check(&conn, ApiIdAlias(key)).expect("ok").is_none());
}

#[test]
fn upsert_roundtrip_and_delete() {
    let conn = fresh_conn();
    let key = make_key(&conn, "quota");
    let caps = QuotaCaps {
        monthly_usd_cap: Some(25.0),
        weekly_usd_cap: None,
        five_hour_usd_cap: Some(1.5),
    };
    upsert(&conn, ApiIdAlias(key), &caps).expect("upsert");
    let stored = get_caps(&conn, ApiIdAlias(key)).expect("get").expect("row");
    assert_eq!(stored, caps);
    // Re-upsert replaces (idempotent).
    let caps2 = QuotaCaps {
        monthly_usd_cap: None,
        weekly_usd_cap: Some(10.0),
        five_hour_usd_cap: None,
    };
    upsert(&conn, ApiIdAlias(key), &caps2).expect("upsert");
    let stored = get_caps(&conn, ApiIdAlias(key)).expect("get").expect("row");
    assert_eq!(stored, caps2);
    delete(&conn, ApiIdAlias(key)).expect("delete");
    assert!(get_caps(&conn, ApiIdAlias(key)).expect("get").is_none());
}

#[test]
fn spend_counts_only_served_rows_and_respects_windows() {
    let conn = fresh_conn();
    let key = make_key(&conn, "spend");
    upsert(
        &conn,
        ApiIdAlias(key),
        &QuotaCaps {
            monthly_usd_cap: Some(25.0),
            weekly_usd_cap: None,
            five_hour_usd_cap: Some(1.0),
        },
    )
    .expect("upsert");
    insert_spend(&conn, key, 1.20, 6 * 3600); // 6h ago: outside the 5h window
    insert_spend(&conn, key, 0.80, 3600); // 1h ago: inside
    insert_failed_attempt(&conn, key, 99.0); // never served: excluded
    let st = status(&conn, ApiIdAlias(key)).expect("ok");
    assert!(st.enabled);
    assert!((st.month.spent_usd - 2.0).abs() < 1e-9);
    assert!((st.five_hours.spent_usd - 0.80).abs() < 1e-9);
    // Served 5h spend 0.80 < 1.0 cap -> allowed; month 2.0 < 25.0 -> allowed.
    assert!(check(&conn, ApiIdAlias(key)).expect("ok").is_none());
}

#[test]
fn check_reports_violated_window() {
    let conn = fresh_conn();
    let key = make_key(&conn, "violation");
    upsert(
        &conn,
        ApiIdAlias(key),
        &QuotaCaps {
            monthly_usd_cap: Some(25.0),
            weekly_usd_cap: None,
            five_hour_usd_cap: Some(0.50),
        },
    )
    .expect("upsert");
    insert_spend(&conn, key, 0.75, 60); // served 75c in the last hour
    let v = check(&conn, ApiIdAlias(key))
        .expect("ok")
        .expect("violation");
    assert_eq!(v.window, WINDOW_FIVE_HOURS);
    assert!((v.cap_usd - 0.50).abs() < 1e-9);
    assert!((v.spent_usd - 0.75).abs() < 1e-9);
    assert!(v.resets_at.is_none()); // rolling window
}

#[test]
fn month_violation_carries_reset_time() {
    let conn = fresh_conn();
    let key = make_key(&conn, "month");
    upsert(
        &conn,
        ApiIdAlias(key),
        &QuotaCaps {
            monthly_usd_cap: Some(1.0),
            weekly_usd_cap: None,
            five_hour_usd_cap: None,
        },
    )
    .expect("upsert");
    insert_spend(&conn, key, 1.0, 10);
    let v = check(&conn, ApiIdAlias(key))
        .expect("ok")
        .expect("violation");
    assert_eq!(v.window, WINDOW_MONTH);
    assert!(v.resets_at.is_some());
    // ISO-8601 shape: YYYY-MM-DDTHH:MM:SSZ
    let r = v.resets_at.unwrap();
    assert_eq!(r.len(), 20);
    assert!(r.ends_with('Z'));
}
