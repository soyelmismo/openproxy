use super::*;
use std::time::Duration;

#[test]
fn test_sync_inferred_embedding_modalities_and_family_persist() {
    let conn = fresh_db();
    let provider = ProviderId::new("provA");

    let models = [minimal("qwen3-embedding")];
    let diff = crate::models::sync::compute_diff(&conn, &provider, &models).expect("diff");
    let (res, _events) = crate::models::sync::execute_sync_transaction(
        &conn,
        &provider,
        &models,
        &diff,
        Duration::from_hours(1),
    )
    .expect("sync");
    assert_eq!(res.touched, 1);

    let (m_type, family, in_mods): (String, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT model_type, family, input_modalities_json FROM models WHERE provider_id = ?1 AND model_id = ?2",
            rusqlite::params![provider.as_str(), "qwen3-embedding"],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("query row");

    assert_eq!(m_type, "embedding", "model_type inferred as embedding");
    assert_eq!(family.as_deref(), Some("qwen3"), "family inferred as qwen3");
    assert!(in_mods.is_some() && in_mods.unwrap().contains("text"));
}

#[test]
fn test_sync_updates_preserve_custom_model_type() {
    let conn = fresh_db();
    let provider = ProviderId::new("provA");

    conn.execute(
        "INSERT INTO models (provider_id, model_id, display_name, target_format, custom, model_type) \
         VALUES (?1, 'custom_model', 'Custom Display', 'openai', 1, 'custom_embedding')",
        rusqlite::params![provider.as_str()],
    )
    .expect("insert custom");

    let models = [minimal("custom_model")];
    let diff = crate::models::sync::compute_diff(&conn, &provider, &models).expect("diff");
    let (res, _events) = crate::models::sync::execute_sync_transaction(
        &conn,
        &provider,
        &models,
        &diff,
        Duration::from_hours(1),
    )
    .expect("sync");
    assert_eq!(res.touched, 1);

    let (m_type, custom): (String, i64) = conn
        .query_row(
            "SELECT model_type, custom FROM models WHERE provider_id = ?1 AND model_id = 'custom_model'",
            [provider.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("query row");

    assert_eq!(custom, 1);
    assert_eq!(m_type, "custom_embedding", "custom type preserved");
}

#[test]
fn test_sync_failure_after_pruning_rolls_back_modifications() {
    let conn = fresh_db();
    let provider = ProviderId::new("provA");

    let initial = [minimal("old_model")];
    let diff0 = crate::models::sync::compute_diff(&conn, &provider, &initial).expect("diff0");
    crate::models::sync::execute_sync_transaction(
        &conn,
        &provider,
        &initial,
        &diff0,
        Duration::from_hours(1),
    )
    .expect("sync initial");

    conn.execute(
        "CREATE TRIGGER trigger_fail_on_prune AFTER DELETE ON models \
         BEGIN SELECT RAISE(ABORT, 'forced abort on prune delete'); END;",
        [],
    )
    .expect("create trigger");

    let new_models = [minimal("new_model")];
    let diff = crate::models::sync::compute_diff(&conn, &provider, &new_models).expect("diff");
    let res = crate::models::sync::execute_sync_transaction(
        &conn,
        &provider,
        &new_models,
        &diff,
        Duration::from_hours(1),
    );
    assert!(res.is_err(), "must fail due to trigger error");

    let (old_survives, new_missing): (bool, bool) = conn
        .query_row(
            "SELECT \
                EXISTS(SELECT 1 FROM models WHERE provider_id = ?1 AND model_id = 'old_model'), \
                NOT EXISTS(SELECT 1 FROM models WHERE provider_id = ?1 AND model_id = 'new_model')",
            [provider.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("query rollback");
    assert!(
        old_survives && new_missing,
        "rollback restores old, discards new"
    );
}

#[test]
fn test_sync_calls_shared_prune_obsolete_models() {
    let conn = fresh_db();
    let provider = ProviderId::new("provA");

    let initial = [minimal("keep_me"), minimal("remove_me")];
    let diff0 = crate::models::sync::compute_diff(&conn, &provider, &initial).expect("diff0");
    crate::models::sync::execute_sync_transaction(
        &conn,
        &provider,
        &initial,
        &diff0,
        Duration::from_hours(1),
    )
    .expect("sync initial");

    let next = [minimal("keep_me")];
    let diff = crate::models::sync::compute_diff(&conn, &provider, &next).expect("diff");
    let (res, _) = crate::models::sync::execute_sync_transaction(
        &conn,
        &provider,
        &next,
        &diff,
        Duration::from_hours(1),
    )
    .expect("sync next");
    assert_eq!(res.touched, 1);

    let (keep_exists, remove_gone): (bool, bool) = conn
        .query_row(
            "SELECT \
                EXISTS(SELECT 1 FROM models WHERE provider_id = ?1 AND model_id = 'keep_me'), \
                NOT EXISTS(SELECT 1 FROM models WHERE provider_id = ?1 AND model_id = 'remove_me')",
            [provider.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("query prune");
    assert!(
        keep_exists && remove_gone,
        "keep_me remains and remove_me pruned"
    );
}

#[test]
fn test_sync_with_prune_models_disabled_emits_no_model_gone_events() {
    let conn = fresh_db();
    let provider = ProviderId::new("prov_no_prune_sync");
    conn.execute(
        "INSERT INTO providers (id, display_name, base_url, auth_kind, prune_models) \
         VALUES (?1, 'prov_no_prune_sync', 'http://127.0.0.1', 'none', 0)",
        [provider.as_str()],
    )
    .expect("insert provider with prune_models=0");

    let initial = [minimal("m1"), minimal("m2")];
    let diff0 = crate::models::sync::compute_diff(&conn, &provider, &initial).expect("diff0");
    crate::models::sync::execute_sync_transaction(
        &conn,
        &provider,
        &initial,
        &diff0,
        Duration::from_hours(1),
    )
    .expect("sync initial");

    let next = [minimal("m1")];
    let diff = crate::models::sync::compute_diff(&conn, &provider, &next).expect("diff");
    let (_res, events) = crate::models::sync::execute_sync_transaction(
        &conn,
        &provider,
        &next,
        &diff,
        Duration::from_hours(1),
    )
    .expect("sync next");

    let has_gone_event = events
        .iter()
        .any(|(_, kind, _)| *kind == crate::notifications::KIND_MODEL_GONE);
    assert!(
        !has_gone_event,
        "no model_gone events should be emitted when prune_models=0"
    );

    let m2_exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM models WHERE provider_id = ?1 AND model_id = 'm2')",
            [provider.as_str()],
            |r| r.get(0),
        )
        .expect("check m2 exists");
    assert!(m2_exists, "m2 must be retained in DB");
}

#[test]
fn test_sync_with_model_in_combo_emits_no_model_gone_and_preserves_target() {
    let mut conn = Connection::open_in_memory().unwrap();
    openproxy_db::migrations::run(&mut conn).unwrap();
    let provider = ProviderId::new("prov_combo_sync");
    conn.execute(
        "INSERT INTO providers (id, name, base_url, auth_type, format, prune_models) \
         VALUES (?1, 'prov_combo_sync', 'http://127.0.0.1', 'none', 'openai', 0)",
        [provider.as_str()],
    )
    .expect("insert provider");

    let initial = [minimal("pinned"), minimal("unpinned")];
    let diff0 = crate::models::sync::compute_diff(&conn, &provider, &initial).expect("diff0");
    crate::models::sync::execute_sync_transaction(
        &conn,
        &provider,
        &initial,
        &diff0,
        Duration::from_hours(1),
    )
    .expect("sync initial");

    let pinned_id: i64 = conn
        .query_row(
            "SELECT id FROM models WHERE provider_id = ?1 AND model_id = 'pinned'",
            [provider.as_str()],
            |r| r.get(0),
        )
        .expect("get pinned id");

    conn.execute(
        "INSERT INTO combos (name, strategy) VALUES ('c_test', 'priority')",
        [],
    )
    .expect("insert combo");
    let combo_id: i64 = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO combo_targets (combo_id, provider_id, model_row_id, priority_order) \
         VALUES (?1, ?2, ?3, 1)",
        rusqlite::params![combo_id, provider.as_str(), pinned_id],
    )
    .expect("insert combo target");

    // Next sync discovers empty list
    let next: [crate::models::DiscoveredModel; 0] = [];
    let diff = crate::models::sync::compute_diff(&conn, &provider, &next).expect("diff");
    let (_res, events) = crate::models::sync::execute_sync_transaction(
        &conn,
        &provider,
        &next,
        &diff,
        Duration::from_hours(1),
    )
    .expect("sync empty");

    // Events should NOT contain model_gone when prune_models=0
    let any_gone = events
        .iter()
        .any(|(_, kind, _)| *kind == crate::notifications::KIND_MODEL_GONE);
    assert!(!any_gone, "no model_gone events should be emitted when prune_models=0");

    let target_exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM combo_targets WHERE combo_id = ?1 AND model_row_id = ?2)",
            rusqlite::params![combo_id, pinned_id],
            |r| r.get(0),
        )
        .expect("check target exists");
    assert!(target_exists, "combo target must survive when prune_models=0");
}
