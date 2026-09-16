use super::{fresh_pool, make_input};
use crate::api_keys::*;
use crate::error::CoreError;
use crate::ids::ApiKeyId;
use chrono::Utc;
use openproxy_types::UpdateField;
use rusqlite::params;

#[test]
fn hash_key_is_deterministic() {
    let h1 = hash_key("op_live_abc");
    let h2 = hash_key("op_live_abc");
    assert_eq!(h1, h2);
    // SHA-256 hex is 64 chars.
    assert_eq!(h1.len(), 64);
    // Different inputs → different hashes.
    assert_ne!(h1, hash_key("op_live_abd"));
}

#[test]
fn generate_plaintext_has_correct_format() {
    let p = generate_plaintext();
    assert!(p.starts_with("op_live_"), "got: {p}");
    // "op_live_" = 8 chars + 32 random = 40 total.
    assert_eq!(p.len(), 40);
    // Two calls produce different outputs (32 chars of randomness).
    assert_ne!(p, generate_plaintext());
}

#[test]
fn is_expired_returns_false_when_none() {
    let now = chrono::Utc::now();
    assert!(!is_expired(None, now).expect("ok"));
}

#[test]
fn is_expired_returns_true_when_now_is_after_rfc3339() {
    let stored = "2020-01-01T00:00:00Z";
    let now = "2025-06-18T12:00:00Z"
        .parse::<chrono::DateTime<Utc>>()
        .unwrap();
    assert!(
        is_expired(Some(stored), now).expect("ok"),
        "2020 timestamp with a 2025 now must be expired"
    );
}

#[test]
fn is_expired_returns_false_when_future_rfc3339() {
    let stored = "2099-12-31T23:59:59Z";
    let now = chrono::Utc::now();
    assert!(!is_expired(Some(stored), now).expect("ok"));
}

#[test]
fn is_expired_treats_malformed_as_error() {
    let now = chrono::Utc::now();
    let err = is_expired(Some("not-a-date"), now).expect_err("parse error");
    assert!(matches!(err, CoreError::Database { .. }));
}

#[test]
fn is_expired_handles_offset_timezone() {
    let stored = "2025-06-18T15:00:00+02:00";
    let now_just_before = "2025-06-18T12:59:59Z"
        .parse::<chrono::DateTime<Utc>>()
        .unwrap();
    let now_just_after = "2025-06-18T13:00:01Z"
        .parse::<chrono::DateTime<Utc>>()
        .unwrap();

    assert!(!is_expired(Some(stored), now_just_before).expect("ok"));
    assert!(is_expired(Some(stored), now_just_after).expect("ok"));
}

#[test]
fn create_returns_plaintext_once() {
    let (conn, _p) = fresh_pool();
    let (key, plaintext) = create(&conn, make_input("test"), "admin").expect("create");
    assert!(plaintext.starts_with("op_live_"));
    assert_eq!(key.key_hash, hash_key(&plaintext));
    assert_eq!(key.key_prefix.as_deref(), Some(&plaintext[..12]));
    assert_eq!(key.scopes, vec!["chat".to_string()]);
    assert!(key.is_active);
    assert!(key.revoked_at.is_none());
    assert!(key.last_used_at.is_none());
    assert_eq!(key.created_by.as_deref(), Some("admin"));
}

#[test]
fn create_rejects_empty_scopes() {
    let (conn, _p) = fresh_pool();
    let input = CreateApiKeyInput {
        label: None,
        scopes: vec![],
        ..Default::default()
    };
    let err = create(&conn, input, "admin").expect_err("empty scopes");
    assert!(matches!(err, CoreError::Validation(_)));
}

#[test]
fn get_by_hash_roundtrip() {
    let (conn, _p) = fresh_pool();
    let (key, plaintext) = create(&conn, make_input("rt"), "admin").expect("create");

    let by_hash = get_by_hash(&conn, &hash_key(&plaintext)).expect("by hash");
    assert!(by_hash.is_some());
    let by_hash = by_hash.unwrap();
    assert_eq!(by_hash.id, key.id);
    assert_eq!(by_hash.label.as_deref(), Some("rt"));

    let miss = get_by_hash(&conn, "deadbeef").expect("miss");
    assert!(miss.is_none());

    let by_id = get_by_id(&conn, key.id).expect("by id").expect("present");
    assert_eq!(by_id.id, key.id);
}

#[test]
fn list_returns_all_keys_newest_first() {
    let (conn, _p) = fresh_pool();
    let (k1, _) = create(&conn, make_input("a"), "admin").expect("a");
    let (k2, _) = create(&conn, make_input("b"), "admin").expect("b");
    let (k3, _) = create(&conn, make_input("c"), "admin").expect("c");

    let all = list(&conn).expect("list");
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].id, k3.id);
    assert_eq!(all[1].id, k2.id);
    assert_eq!(all[2].id, k1.id);
}

#[test]
fn revoke_deactivates() {
    let (conn, _p) = fresh_pool();
    let (key, _) = create(&conn, make_input("rev"), "admin").expect("create");

    revoke(&conn, key.id).expect("revoke");
    let after = get_by_id(&conn, key.id).expect("get").expect("present");
    assert!(!after.is_active, "is_active flipped to 0");
    assert!(after.revoked_at.is_some(), "revoked_at stamped");

    let first_ts = after.revoked_at.as_deref().unwrap();
    revoke(&conn, key.id).expect("revoke 2");
    let after2 = get_by_id(&conn, key.id).expect("get 2").expect("present");
    assert_eq!(after2.revoked_at.as_deref(), Some(first_ts));
}

#[test]
fn hard_delete_removes_row() {
    let (conn, _p) = fresh_pool();
    let (key, _) = create(&conn, make_input("del"), "admin").expect("create");
    hard_delete(&conn, key.id).expect("delete");
    assert!(get_by_id(&conn, key.id).expect("get").is_none());
    hard_delete(&conn, key.id).expect("delete again");
}

#[test]
fn regenerate_changes_hash_and_keeps_id() {
    let (conn, _p) = fresh_pool();
    let (key, plaintext) = create(&conn, make_input("regen"), "admin").expect("create");

    let (regen, new_plaintext) = regenerate(&conn, key.id).expect("regenerate");
    assert_ne!(new_plaintext, plaintext, "new plaintext differs");
    assert_ne!(regen.key_hash, key.key_hash, "hash changed");
    assert_ne!(regen.key_prefix, key.key_prefix, "prefix changed");
    assert_eq!(regen.id, key.id, "id preserved");
    assert!(regen.is_active, "regenerate re-activates");
    assert!(regen.revoked_at.is_none(), "regenerate clears revoked_at");

    let miss = get_by_hash(&conn, &hash_key(&plaintext)).expect("miss");
    assert!(miss.is_none(), "old plaintext is invalidated");

    let hit = get_by_hash(&conn, &hash_key(&new_plaintext))
        .expect("hit")
        .expect("present");
    assert_eq!(hit.id, key.id);
}

#[test]
fn update_patches_subset() {
    let (conn, _p) = fresh_pool();
    let (key, _) = create(&conn, make_input("u"), "admin").expect("create");

    update(
        &conn,
        key.id,
        UpdateParams {
            label: Some("renamed"),
            ..Default::default()
        },
    )
    .expect("update label");
    let after = get_by_id(&conn, key.id).expect("get").expect("present");
    assert_eq!(after.label.as_deref(), Some("renamed"));
    assert_eq!(after.scopes, vec!["chat".to_string()], "scopes unchanged");

    update(
        &conn,
        key.id,
        UpdateParams {
            scopes: Some(&["manage".to_string(), "read".to_string()]),
            ..Default::default()
        },
    )
    .expect("update scopes");
    let after = get_by_id(&conn, key.id).expect("get").expect("present");
    assert_eq!(after.scopes, vec!["manage".to_string(), "read".to_string()]);

    update(
        &conn,
        key.id,
        UpdateParams {
            allowed_models: UpdateField::Set(&["openai/gpt-4o".to_string()]),
            ..Default::default()
        },
    )
    .expect("update allowed_models");
    let after = get_by_id(&conn, key.id).expect("get").expect("present");
    assert_eq!(
        after.allowed_models,
        Some(vec!["openai/gpt-4o".to_string()])
    );

    update(
        &conn,
        key.id,
        UpdateParams {
            allowed_models: UpdateField::Reset,
            ..Default::default()
        },
    )
    .expect("clear allowed_models");
    let after = get_by_id(&conn, key.id).expect("get").expect("present");
    assert!(after.allowed_models.is_none(), "allowed_models cleared");

    let err = update(
        &conn,
        key.id,
        UpdateParams {
            scopes: Some(&[]),
            ..Default::default()
        },
    )
    .expect_err("empty scopes");
    assert!(matches!(err, CoreError::Validation(_)));

    let err = update(
        &conn,
        ApiKeyId(9999),
        UpdateParams {
            label: Some("x"),
            ..Default::default()
        },
    )
    .expect_err("missing");
    assert!(matches!(err, CoreError::Internal(_)));
}

#[test]
fn update_disable_stamps_revoked_at() {
    let (conn, _p) = fresh_pool();
    let (key, _) = create(&conn, make_input("dis"), "admin").expect("create");

    update(
        &conn,
        key.id,
        UpdateParams {
            is_active: Some(false),
            ..Default::default()
        },
    )
    .expect("disable");
    let after = get_by_id(&conn, key.id).expect("get").expect("present");
    assert!(!after.is_active);
    assert!(after.revoked_at.is_some());

    update(
        &conn,
        key.id,
        UpdateParams {
            is_active: Some(true),
            ..Default::default()
        },
    )
    .expect("enable");
    let after = get_by_id(&conn, key.id).expect("get").expect("present");
    assert!(after.is_active);
    assert!(after.revoked_at.is_none());
}

#[test]
fn touch_last_used_throttles() {
    let (conn, _p) = fresh_pool();
    let (key, _) = create(&conn, make_input("throttle"), "admin").expect("create");

    touch_last_used(&conn, key.id).expect("touch 1");
    let after1 = get_by_id(&conn, key.id).expect("get").expect("present");
    let ts1 = after1.last_used_at.as_deref().expect("stamp 1");

    touch_last_used(&conn, key.id).expect("touch 2");
    let after2 = get_by_id(&conn, key.id).expect("get").expect("present");
    let ts2 = after2.last_used_at.as_deref().expect("stamp 2");
    assert_eq!(ts1, ts2, "throttled: same timestamp");

    conn.execute(
        "UPDATE api_keys SET last_used_at = '2020-01-01 00:00:00' WHERE id = ?1",
        params![key.id.0],
    )
    .expect("manual backdate");
    touch_last_used(&conn, key.id).expect("touch 3");
    let after3 = get_by_id(&conn, key.id).expect("get").expect("present");
    assert_ne!(after3.last_used_at.as_deref(), Some("2020-01-01 00:00:00"));
}

#[test]
fn legacy_row_gets_metadata_defaults() {
    let (conn, _p) = fresh_pool();
    conn.execute(
        "INSERT INTO api_keys (key_hash, label) VALUES (?1, ?2)",
        params!["abc123hash", "legacy"],
    )
    .expect("insert legacy");

    let all = list(&conn).expect("list");
    assert_eq!(all.len(), 1);
    let k = &all[0];
    assert_eq!(k.scopes, vec!["chat".to_string()]);
    assert!(k.is_active, "is_active defaults to 1");
    assert!(k.key_prefix.is_none(), "no prefix for legacy");
    assert!(k.allowed_models.is_none());
    assert!(k.allowed_combos.is_none());
    assert!(k.revoked_at.is_none());
    assert!(k.expires_at.is_none());
    assert!(k.last_used_at.is_none());
}

#[test]
fn count_active_is_zero_on_fresh_db() {
    let (conn, _p) = fresh_pool();
    assert_eq!(count_active(&conn).expect("count"), 0);
}

#[test]
fn count_active_returns_one_after_create() {
    let (conn, _p) = fresh_pool();
    create(&conn, make_input("a"), "admin").expect("create");
    assert_eq!(count_active(&conn).expect("count"), 1);
}

#[test]
fn count_active_excludes_revoked_keys() {
    let (conn, _p) = fresh_pool();
    let (k1, _) = create(&conn, make_input("a"), "admin").expect("create");
    create(&conn, make_input("b"), "admin").expect("create");
    revoke(&conn, k1.id).expect("revoke");
    assert_eq!(count_active(&conn).expect("count"), 1);
}
