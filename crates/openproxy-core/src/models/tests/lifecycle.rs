use super::*;
use crate::models::{
    get_by_row_id, list_active, list_active_all, list_all, mark_expired, set_active,
    set_active_bulk, set_test_status, upsert_many,
};
use openproxy_types::TargetFormat;

fn insert_model_sql(
    conn: &Connection,
    prov: &str,
    model: &str,
    fmt: &str,
    active: i32,
    expires_sql: &str,
) {
    conn.execute(
        &format!("INSERT INTO models (provider_id, model_id, display_name, target_format, active, expires_at) VALUES ('{prov}', '{model}', '{model}', '{fmt}', {active}, {expires_sql})"),
        [],
    ).unwrap();
}

fn seed_single_model(conn: &Connection) -> (ProviderId, ModelRowId) {
    let provider = ProviderId::new("provA");
    upsert_many(
        conn,
        &provider,
        &[discovered("m1", TargetFormat::Openai)],
        Duration::from_hours(1),
    )
    .unwrap();
    let row_id = list_all(conn).unwrap()[0].row_id;
    (provider, row_id)
}

#[test]
fn list_active_excludes_expired() {
    let conn = fresh_db();
    let provider = ProviderId::new("provA");
    insert_model_sql(
        &conn,
        "provA",
        "live",
        "openai",
        1,
        "datetime('now', '+1 hour')",
    );
    insert_model_sql(
        &conn,
        "provA",
        "stale",
        "openai",
        1,
        "datetime('now', '-1 hour')",
    );
    insert_model_sql(&conn, "provA", "null_expiry", "openai", 1, "NULL");
    let active = list_active(&conn, &provider).expect("list_active");
    assert_eq!(active.len(), 3);
    let ids: Vec<&str> = active.iter().map(|m| m.model_id.as_str()).collect();
    assert!(ids.contains(&"live") && ids.contains(&"stale") && ids.contains(&"null_expiry"));
}

#[test]
fn list_active_all_excludes_disabled_and_expired_across_providers() {
    let conn = fresh_db();
    add_provider(&conn, "provB");
    insert_model_sql(&conn, "provA", "a_active", "openai", 1, "NULL");
    insert_model_sql(&conn, "provA", "a_disabled", "openai", 0, "NULL");
    insert_model_sql(&conn, "provB", "b_active", "anthropic", 1, "NULL");
    let all = list_active_all(&conn).unwrap();
    let ids: Vec<&str> = all.iter().map(|m| m.model_id.as_str()).collect();
    assert_eq!(ids, vec!["a_active", "b_active"]);
}

#[test]
fn list_active_all_excludes_models_from_deactivated_provider() {
    let conn = fresh_db();
    add_provider(&conn, "provB");
    insert_model_sql(&conn, "provA", "a_active", "openai", 1, "NULL");
    insert_model_sql(&conn, "provB", "b_active", "anthropic", 1, "NULL");
    conn.execute("UPDATE providers SET active = 0 WHERE id = 'provB'", [])
        .unwrap();
    let all = list_active_all(&conn).unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].model_id.as_str(), "a_active");
}

#[test]
fn expires_at_in_the_past_with_active_1_is_visible() {
    let conn = fresh_db();
    insert_model_sql(
        &conn,
        "provA",
        "old-ttl",
        "openai",
        1,
        "datetime('now', '-2 hours')",
    );
    let active = list_active(&conn, &ProviderId::new("provA")).unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].model_id.as_str(), "old-ttl");
}

#[test]
fn mark_expired_deletes_old() {
    let conn = fresh_db();
    insert_model_sql(
        &conn,
        "provA",
        "recent",
        "openai",
        1,
        "datetime('now', '+1 hour')",
    );
    insert_model_sql(
        &conn,
        "provA",
        "very_old",
        "openai",
        1,
        "datetime('now', '-8 days')",
    );
    insert_model_sql(
        &conn,
        "provA",
        "barely_old",
        "openai",
        1,
        "datetime('now', '-2 days')",
    );
    assert_eq!(mark_expired(&conn).unwrap(), 1);
    let ids: Vec<String> = list_all(&conn)
        .unwrap()
        .into_iter()
        .map(|m| m.model_id.0)
        .collect();
    assert_eq!(ids.len(), 2);
    assert!(
        ids.contains(&"recent".into())
            && ids.contains(&"barely_old".into())
            && !ids.contains(&"very_old".into())
    );
}

#[test]
fn get_by_row_id_returns_some_and_none() {
    let conn = fresh_db();
    let (_, row_id) = seed_single_model(&conn);
    assert_eq!(
        get_by_row_id(&conn, row_id)
            .unwrap()
            .unwrap()
            .model_id
            .as_str(),
        "m1"
    );
    assert!(
        get_by_row_id(&conn, ModelRowId(99_999_999))
            .unwrap()
            .is_none()
    );
}

#[test]
fn set_active_toggles_visibility() {
    let conn = fresh_db();
    let (provider, row_id) = seed_single_model(&conn);
    set_active(&conn, row_id, false).unwrap();
    assert!(list_active(&conn, &provider).unwrap().is_empty());
    set_active(&conn, row_id, true).unwrap();
    assert_eq!(list_active(&conn, &provider).unwrap().len(), 1);
}

#[test]
fn set_active_false_sets_manually_disabled_at() {
    let conn = fresh_db();
    let (_, row_id) = seed_single_model(&conn);
    let before = now_epoch(&conn);
    set_active(&conn, row_id, false).unwrap();
    let after = now_epoch(&conn);
    let m = get_by_row_id(&conn, row_id).unwrap().unwrap();
    assert!(!m.active);
    let stamped = manually_disabled_epoch(&conn, row_id).unwrap().unwrap();
    assert!(stamped >= before && stamped <= after + 2);
}

#[test]
fn set_active_true_clears_manually_disabled_at() {
    let conn = fresh_db();
    let (_, row_id) = seed_single_model(&conn);
    set_active(&conn, row_id, false).unwrap();
    assert!(
        get_by_row_id(&conn, row_id)
            .unwrap()
            .unwrap()
            .manually_disabled_at
            .is_some()
    );
    set_active(&conn, row_id, true).unwrap();
    let m = get_by_row_id(&conn, row_id).unwrap().unwrap();
    assert!(m.manually_disabled_at.is_none() && m.active);
}

#[test]
fn set_active_bulk_false_sets_manually_disabled_at() {
    let conn = fresh_db();
    let provider = ProviderId::new("provA");
    upsert_many(
        &conn,
        &provider,
        &[
            discovered("a", TargetFormat::Openai),
            discovered("b", TargetFormat::Openai),
            discovered("c", TargetFormat::Openai),
        ],
        Duration::from_hours(1),
    )
    .unwrap();
    insert_model_sql(
        &conn,
        "provA",
        "z",
        "openai",
        1,
        "datetime('now', '+1 hour')",
    );
    conn.execute("UPDATE models SET custom = 1 WHERE model_id = 'z'", [])
        .unwrap();

    let before = now_epoch(&conn);
    assert_eq!(set_active_bulk(&conn, &provider, false).unwrap(), 3);
    let after = now_epoch(&conn);

    for id in ["a", "b", "c"] {
        let row_id = list_all(&conn)
            .unwrap()
            .into_iter()
            .find(|m| m.model_id.as_str() == id)
            .unwrap()
            .row_id;
        let m = get_by_row_id(&conn, row_id).unwrap().unwrap();
        assert!(!m.active && m.manually_disabled_at.is_some());
        let stamped = manually_disabled_epoch(&conn, row_id).unwrap().unwrap();
        assert!(stamped >= before && stamped <= after + 2);
    }
    let z = list_all(&conn)
        .unwrap()
        .into_iter()
        .find(|m| m.model_id.as_str() == "z")
        .unwrap();
    assert!(z.active && z.manually_disabled_at.is_none());
}

#[test]
fn set_active_bulk_true_clears_manually_disabled_at() {
    let conn = fresh_db();
    let (provider, row_id) = seed_single_model(&conn);
    set_active(&conn, row_id, false).unwrap();
    assert_eq!(set_active_bulk(&conn, &provider, true).unwrap(), 1);
    let m = get_by_row_id(&conn, row_id).unwrap().unwrap();
    assert!(m.active && m.manually_disabled_at.is_none());
}

#[test]
fn set_active_bulk_updates_non_custom_only() {
    let conn = fresh_db();
    let provider = ProviderId::new("provA");
    upsert_many(
        &conn,
        &provider,
        &[
            discovered("d1", TargetFormat::Openai),
            discovered("d2", TargetFormat::Openai),
        ],
        Duration::from_hours(1),
    )
    .unwrap();
    crate::models::create_custom(
        &conn,
        &provider,
        &ModelId::new("c1"),
        Some("Custom 1"),
        TargetFormat::Openai,
        3600,
        Some("chat"),
    )
    .unwrap();
    assert_eq!(set_active_bulk(&conn, &provider, false).unwrap(), 2);
    let all = list_all(&conn).unwrap();
    assert!(
        all.iter()
            .find(|m| m.model_id.as_str() == "c1")
            .unwrap()
            .active
    );
    assert!(
        !all.iter()
            .find(|m| m.model_id.as_str() == "d1")
            .unwrap()
            .active
    );
}

#[test]
fn refresh_does_not_reactivate_manually_disabled_model() {
    let conn = fresh_db();
    let (provider, row_id) = seed_single_model(&conn);
    set_active(&conn, row_id, false).unwrap();
    let stamp = get_by_row_id(&conn, row_id)
        .unwrap()
        .unwrap()
        .manually_disabled_at
        .unwrap()
        .to_string();

    let m1 = discovered("m1", TargetFormat::Openai);
    upsert_many(
        &conn,
        &provider,
        std::slice::from_ref(&m1),
        Duration::from_hours(1),
    )
    .unwrap();
    let m = get_by_row_id(&conn, row_id).unwrap().unwrap();
    assert_eq!(m.manually_disabled_at.as_deref(), Some(stamp.as_ref()));
    assert!(!m.active);

    assert_eq!(
        crate::models::apply_auto_activation(&conn, &provider, None).unwrap(),
        0
    );
    let m = get_by_row_id(&conn, row_id).unwrap().unwrap();
    assert!(!m.active && m.manually_disabled_at.is_some());
}

#[test]
fn set_test_status_stamps_both_columns() {
    let conn = fresh_db();
    let (_, row_id) = seed_single_model(&conn);
    assert_eq!(
        get_by_row_id(&conn, row_id)
            .unwrap()
            .unwrap()
            .last_test_status,
        None
    );
    set_test_status(&conn, row_id, 200).unwrap();
    assert_eq!(
        get_by_row_id(&conn, row_id)
            .unwrap()
            .unwrap()
            .last_test_status,
        Some(200)
    );
    set_test_status(&conn, row_id, 503).unwrap();
    assert_eq!(
        get_by_row_id(&conn, row_id)
            .unwrap()
            .unwrap()
            .last_test_status,
        Some(503)
    );
}
