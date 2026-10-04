use super::*;
use crate::models::sync::{compute_diff, execute_sync_transaction};
use openproxy_types::TargetFormat;
use openproxy_types::error::CoreError;
use std::time::Duration;

#[test]
fn execute_sync_transaction_reconnect_abort_rolls_back_catalog() {
    let conn = fresh_db();
    let provider = ProviderId::new("provA");

    conn.execute(
        "INSERT INTO models (provider_id, model_id, target_format) VALUES ('provA', 'old_model', 'openai')",
        [],
    )
    .unwrap();

    conn.execute_batch(
        "CREATE TABLE combo_targets (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            provider_id TEXT NOT NULL,
            model_row_id INTEGER,
            upstream_model_id TEXT NOT NULL
        );
        INSERT INTO combo_targets (provider_id, model_row_id, upstream_model_id)
            VALUES ('provA', NULL, 'new_model');
        CREATE TRIGGER abort_sync_reconnect
            BEFORE UPDATE ON combo_targets
            BEGIN
                SELECT RAISE(ABORT, 'sync reconnect aborted by trigger');
            END;",
    )
    .unwrap();

    let discovered_models = [discovered("new_model", TargetFormat::Openai)];
    let diff = compute_diff(&conn, &provider, &discovered_models).unwrap();
    assert_eq!(diff.new_models.len(), 1);

    let err = execute_sync_transaction(
        &conn,
        &provider,
        &discovered_models,
        &diff,
        Duration::from_hours(1),
    )
    .unwrap_err();

    match err {
        CoreError::Database { message, source } => {
            assert!(message.contains("reconnect orphan targets batch for provider provA"));
            assert!(message.contains("sync reconnect aborted by trigger"));
            assert!(source.is_some());
        }
        other => panic!("expected CoreError::Database, got: {other:?}"),
    }

    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM models WHERE provider_id = 'provA' AND model_id = 'new_model'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0, "new_model should not be present after rollback");

    let old_exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM models WHERE provider_id = 'provA' AND model_id = 'old_model'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(old_exists, 1, "old_model must be preserved");

    let fk: Option<i64> = conn
        .query_row(
            "SELECT model_row_id FROM combo_targets WHERE upstream_model_id = 'new_model'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(fk, None);
}

#[test]
fn execute_sync_transaction_reconnects_orphan_targets_normally() {
    let conn = fresh_db();
    let provider = ProviderId::new("provA");

    conn.execute_batch(
        "CREATE TABLE combo_targets (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            provider_id TEXT NOT NULL,
            model_row_id INTEGER,
            upstream_model_id TEXT NOT NULL
        );
        INSERT INTO combo_targets (provider_id, model_row_id, upstream_model_id)
            VALUES ('provA', NULL, 'auto_reconnect_model');",
    )
    .unwrap();

    let discovered_models = [discovered("auto_reconnect_model", TargetFormat::Openai)];
    let diff = compute_diff(&conn, &provider, &discovered_models).unwrap();

    let (res, _events) = execute_sync_transaction(
        &conn,
        &provider,
        &discovered_models,
        &diff,
        Duration::from_hours(1),
    )
    .unwrap();

    assert_eq!(res.touched, 1);
    assert_eq!(res.new_model_ids.len(), 1);

    let (reconnected_fk, up_id): (Option<i64>, String) = conn
        .query_row(
            "SELECT model_row_id, upstream_model_id FROM combo_targets WHERE provider_id = 'provA'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();

    assert!(
        reconnected_fk.is_some(),
        "target should have reconnected model_row_id"
    );
    assert_eq!(up_id, "auto_reconnect_model");

    let model_id_in_db: i64 = conn
        .query_row(
            "SELECT id FROM models WHERE provider_id = 'provA' AND model_id = 'auto_reconnect_model'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(reconnected_fk, Some(model_id_in_db));
}
