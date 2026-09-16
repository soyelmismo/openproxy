use super::{fresh_pool, seed_provider};
use crate::accounts::*;
use openproxy_db::secrets::MasterKey;
use openproxy_types::ids::ProviderId;
use openproxy_types::{AccountId, CoreError};

#[test]
fn set_quota_roundtrip() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    seed_provider(&conn, "minimax");

    let mk = MasterKey::generate().unwrap();
    let id = create(
        &conn,
        &ProviderId::new("minimax"),
        Some("sk-quota"),
        &mk,
        Some("quota-test"),
        10,
        None,
    )
    .expect("create");

    // Initially: every quota_* column is NULL.
    let a = get(&conn, id, &mk).expect("get").expect("present");
    assert!(a.quota_session_used.is_none());
    assert!(a.quota_session_limit.is_none());
    assert!(a.quota_session_reset_at.is_none());
    assert!(a.quota_weekly_used.is_none());
    assert!(a.quota_weekly_limit.is_none());
    assert!(a.quota_weekly_reset_at.is_none());
    assert!(a.quota_plan_name.is_none());
    assert!(a.quota_last_fetched_at.is_none());
    assert!(a.quota_fetch_error.is_none());

    // Stamp a snapshot.
    let q = crate::quota::AccountQuota {
        session_used: Some(1234),
        session_limit: Some(5000),
        session_reset_at: Some("1700000000".into()),
        weekly_used: Some(80000),
        weekly_limit: Some(500_000),
        weekly_reset_at: Some("1700003600".into()),
        plan_name: Some("Coding Plan".into()),
        last_fetched_at: "1700000001".into(),
        fetch_error: None,
        model_details: None,
    };
    set_quota(&conn, id, &q).expect("set_quota");

    // Re-read: every field survives the round-trip.
    let a = get(&conn, id, &mk).expect("get").expect("present");
    assert_eq!(a.quota_session_used, Some(1234));
    assert_eq!(a.quota_session_limit, Some(5000));
    assert_eq!(a.quota_session_reset_at.as_deref(), Some("1700000000"));
    assert_eq!(a.quota_weekly_used, Some(80000));
    assert_eq!(a.quota_weekly_limit, Some(500_000));
    assert_eq!(a.quota_weekly_reset_at.as_deref(), Some("1700003600"));
    assert_eq!(a.quota_plan_name.as_deref(), Some("Coding Plan"));
    assert_eq!(a.quota_last_fetched_at.as_deref(), Some("1700000001"));
    assert!(a.quota_fetch_error.is_none());

    // Also visible through `list`.
    let all = list(&conn, Some(&ProviderId::new("minimax")), &mk).expect("list");
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].quota_session_used, Some(1234));
}

#[test]
fn set_quota_records_error() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    seed_provider(&conn, "minimax");

    let mk = MasterKey::generate().unwrap();
    let id = create(
        &conn,
        &ProviderId::new("minimax"),
        Some("sk-x"),
        &mk,
        None,
        100,
        None,
    )
    .expect("create");

    // A failed quota fetch: all numeric fields stay None, the
    // error message is stamped on, and last_fetched_at is set so
    // the UI can distinguish "tried" from "never tried".
    let q = crate::quota::AccountQuota {
        session_used: None,
        session_limit: None,
        session_reset_at: None,
        weekly_used: None,
        weekly_limit: None,
        weekly_reset_at: None,
        plan_name: None,
        last_fetched_at: "1700000099".into(),
        fetch_error: Some("minimax 401".into()),
        model_details: None,
    };
    set_quota(&conn, id, &q).expect("set_quota");

    let a = get(&conn, id, &mk).expect("get").expect("present");
    assert_eq!(a.quota_fetch_error.as_deref(), Some("minimax 401"));
    assert_eq!(a.quota_last_fetched_at.as_deref(), Some("1700000099"));
    assert!(a.quota_session_used.is_none());
}

#[test]
fn set_quota_missing_account_errors() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    seed_provider(&conn, "minimax");

    let q = crate::quota::AccountQuota {
        session_used: None,
        session_limit: None,
        session_reset_at: None,
        weekly_used: None,
        weekly_limit: None,
        weekly_reset_at: None,
        plan_name: None,
        last_fetched_at: "0".into(),
        fetch_error: None,
        model_details: None,
    };
    let err = set_quota(&conn, AccountId(99999), &q).expect_err("missing");
    assert!(matches!(err, CoreError::AccountNotFound(99999)));
}
