use super::{fresh_pool, seed_provider};
use crate::accounts::*;
use openproxy_db::secrets::MasterKey;
use openproxy_types::ids::ProviderId;
use openproxy_types::{AccountId, CoreError, HealthStatus};
use rusqlite::params;

fn seed_acc(
    conn: &rusqlite::Connection,
    pid: &str,
    key: Option<&str>,
    mk: &MasterKey,
) -> AccountId {
    seed_provider(conn, pid);
    create(conn, &ProviderId::new(pid), key, mk, None, 100, None).expect("create")
}

#[test]
fn create_and_get() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    seed_provider(&conn, "openrouter");
    let mk = MasterKey::generate().unwrap();
    let id = create(
        &conn,
        &ProviderId::new("openrouter"),
        Some("sk-test-123"),
        &mk,
        Some("primary"),
        10,
        Some(r#"{"org":"acme"}"#),
    )
    .expect("create");

    let acc = get(&conn, id, &mk).expect("get").expect("present");
    assert_eq!(acc.id, id);
    assert_eq!(acc.provider_id, ProviderId::new("openrouter"));
    assert_eq!(acc.label.as_deref(), Some("primary"));
    assert_eq!(acc.priority, 10);
    assert_eq!(acc.extra_config_json.as_deref(), Some(r#"{"org":"acme"}"#));
    assert_eq!(acc.health_status, HealthStatus::Healthy);
    assert!(acc.rate_limited_until.is_none());
    assert!(!acc.created_at.is_empty());
    assert_eq!(acc.auth_type.as_ref(), "api_key");

    assert!(
        get(&conn, AccountId(9999), &mk)
            .expect("get missing")
            .is_none()
    );
}

#[test]
fn create_encrypts_api_key_at_rest() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    let mk = MasterKey::generate().unwrap();
    let plaintext = "sk-supersecret-DO-NOT-LEAK-9f8a";
    let id = seed_acc(&conn, "openrouter", Some(plaintext), &mk);

    let raw: Vec<u8> = conn
        .query_row(
            "SELECT api_key_encrypted FROM accounts WHERE id = ?1",
            params![id.0],
            |r| r.get(0),
        )
        .expect("blob");
    assert!(!String::from_utf8_lossy(&raw).contains(plaintext));
    assert!(raw.len() >= 28);
}

#[test]
fn decrypt_api_key_roundtrip() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    let mk = MasterKey::generate().unwrap();
    let plaintext = "sk-roundtrip-xyz";
    let id = seed_acc(&conn, "openrouter", Some(plaintext), &mk);

    assert_eq!(decrypt_api_key(&conn, id, &mk).expect("decrypt"), plaintext);
    assert!(matches!(
        decrypt_api_key(&conn, AccountId(424_242), &mk).expect_err("missing"),
        CoreError::AccountNotFound(424_242)
    ));

    let other = MasterKey::generate().unwrap();
    assert!(matches!(
        decrypt_api_key(&conn, id, &other).expect_err("wrong key"),
        CoreError::Internal(_)
    ));
}

#[test]
fn decrypt_api_key_and_label_roundtrip() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    seed_provider(&conn, "cloudflare");
    let mk = MasterKey::generate().unwrap();
    let plaintext = "sk-roundtrip-xyz";
    let label = "my-cf-account-id";

    let id = create(
        &conn,
        &ProviderId::new("cloudflare"),
        Some(plaintext),
        &mk,
        Some(label),
        100,
        None,
    )
    .expect("create");
    let (recovered_key, recovered_label) =
        decrypt_api_key_and_label(&conn, id, &mk).expect("decrypt");
    assert_eq!(recovered_key, plaintext);
    assert_eq!(recovered_label.as_deref(), Some(label));

    assert!(matches!(
        decrypt_api_key_and_label(&conn, AccountId(424_242), &mk).expect_err("missing"),
        CoreError::AccountNotFound(424_242)
    ));
    let other = MasterKey::generate().unwrap();
    assert!(matches!(
        decrypt_api_key_and_label(&conn, id, &other).expect_err("wrong key"),
        CoreError::Internal(_)
    ));

    let id_no_label = create(
        &conn,
        &ProviderId::new("cloudflare"),
        Some("sk-roundtrip-no-label"),
        &mk,
        None,
        100,
        None,
    )
    .expect("create no label");
    let (k, l) = decrypt_api_key_and_label(&conn, id_no_label, &mk).expect("decrypt");
    assert_eq!(k, "sk-roundtrip-no-label");
    assert_eq!(l, None);
}

#[test]
fn list_filters_by_provider() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    seed_provider(&conn, "openrouter");
    seed_provider(&conn, "anthropic");
    let mk = MasterKey::generate().unwrap();

    for (pid, prio) in [("openrouter", 10), ("openrouter", 20), ("anthropic", 5)] {
        create(
            &conn,
            &ProviderId::new(pid),
            Some("sk-x"),
            &mk,
            None,
            prio,
            None,
        )
        .expect("create");
    }

    assert_eq!(list(&conn, None, &mk).expect("list all").len(), 3);
    let only_or = list(&conn, Some(&ProviderId::new("openrouter")), &mk).expect("list or");
    assert_eq!(only_or.len(), 2);
    assert_eq!(only_or[0].priority, 10);
    assert_eq!(only_or[1].priority, 20);

    let only_an = list(&conn, Some(&ProviderId::new("anthropic")), &mk).expect("list an");
    assert_eq!(only_an.len(), 1);
    assert_eq!(only_an[0].provider_id, ProviderId::new("anthropic"));
    assert!(
        list(&conn, Some(&ProviderId::new("nope")), &mk)
            .expect("list nope")
            .is_empty()
    );
}

#[test]
fn set_health_updates_status() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    let mk = MasterKey::generate().unwrap();
    let id = seed_acc(&conn, "openrouter", Some("sk-x"), &mk);

    for (status, expected) in [
        (HealthStatus::Degraded, HealthStatus::Degraded),
        (HealthStatus::Unhealthy, HealthStatus::Unhealthy),
        (HealthStatus::Healthy, HealthStatus::Healthy),
    ] {
        set_health(&conn, id, status).expect("set health");
        assert_eq!(
            get(&conn, id, &mk).expect("get").unwrap().health_status,
            expected
        );
    }
    assert!(matches!(
        set_health(&conn, AccountId(7777), HealthStatus::Healthy).expect_err("missing"),
        CoreError::AccountNotFound(7777)
    ));
}

#[test]
fn set_rate_limited_updates() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    let mk = MasterKey::generate().unwrap();
    let id = seed_acc(&conn, "openrouter", Some("sk-x"), &mk);

    assert!(
        get(&conn, id, &mk)
            .expect("get")
            .unwrap()
            .rate_limited_until
            .is_none()
    );

    set_rate_limited_until(&conn, id, Some("2026-06-13T12:34:56Z")).expect("set");
    assert_eq!(
        get(&conn, id, &mk)
            .expect("get")
            .unwrap()
            .rate_limited_until
            .as_deref(),
        Some("2026-06-13T12:34:56Z")
    );

    set_rate_limited_until(&conn, id, None).expect("clear");
    assert!(
        get(&conn, id, &mk)
            .expect("get")
            .unwrap()
            .rate_limited_until
            .is_none()
    );

    assert!(matches!(
        set_rate_limited_until(&conn, AccountId(12321), Some("x")).expect_err("missing"),
        CoreError::AccountNotFound(12321)
    ));
}

#[test]
fn delete_removes_account() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    let mk = MasterKey::generate().unwrap();
    let id = seed_acc(&conn, "openrouter", Some("sk-x"), &mk);
    assert!(get(&conn, id, &mk).expect("get").is_some());

    delete(&conn, id).expect("delete");
    assert!(get(&conn, id, &mk).expect("get").is_none());
    delete(&conn, id).expect("delete again is fine");
}

#[test]
fn update_api_key_roundtrip() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    let mk = MasterKey::generate().unwrap();
    let id = seed_acc(&conn, "openrouter", None, &mk);

    assert!(matches!(
        decrypt_api_key(&conn, id, &mk).expect_err("no key yet"),
        CoreError::Validation(_)
    ));

    let key = "sk-updated-key-abc123";
    update_api_key(&conn, id, Some(key), &mk).expect("set key");
    assert_eq!(decrypt_api_key(&conn, id, &mk).expect("decrypt"), key);

    update_api_key(&conn, id, None, &mk).expect("clear key");
    assert!(matches!(
        decrypt_api_key(&conn, id, &mk).expect_err("cleared"),
        CoreError::Validation(_)
    ));

    assert!(matches!(
        update_api_key(&conn, AccountId(99999), Some("x"), &mk).expect_err("missing"),
        CoreError::AccountNotFound(99999)
    ));
}

#[test]
fn delete_account_nulls_combo_targets_fk() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    seed_provider(&conn, "minimax");
    let mk = MasterKey::generate().unwrap();
    let account_id = create(
        &conn,
        &ProviderId::new("minimax"),
        Some("sk-test-minimax"),
        &mk,
        Some("primary"),
        10,
        None,
    )
    .expect("create");

    conn.execute("INSERT INTO combos (id, name, strategy, race_size) VALUES (1, 'test-combo', 'priority', 1)", []).expect("insert combo");
    conn.execute("INSERT INTO combo_targets (id, combo_id, provider_id, account_id, upstream_model_id, priority_order) VALUES (1, 1, 'minimax', ?1, 'model-1', 0)", params![account_id.0]).expect("insert target");

    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM combo_targets WHERE account_id = ?1",
            params![account_id.0],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);

    delete(&conn, account_id).expect("delete account");
    assert!(get(&conn, account_id, &mk).expect("get").is_none());

    let target_account_id: Option<i64> = conn
        .query_row(
            "SELECT account_id FROM combo_targets WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(target_account_id.is_none());
}

#[test]
fn health_status_parse_roundtrip() {
    for (variant, s) in [
        (HealthStatus::Healthy, "healthy"),
        (HealthStatus::Degraded, "degraded"),
        (HealthStatus::Unhealthy, "unhealthy"),
    ] {
        assert_eq!(variant.as_str(), s);
        assert_eq!(HealthStatus::parse(s).expect("parse"), variant);
    }
    assert!(HealthStatus::parse("bogus").is_err());
}
