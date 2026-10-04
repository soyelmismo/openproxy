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
