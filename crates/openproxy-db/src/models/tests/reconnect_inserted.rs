use crate::error::{map_db_error, map_db_error_ctx};
use crate::models::reconnect_inserted_combo_targets;
use openproxy_types::ProviderId;
use openproxy_types::error::CoreError;
use rusqlite::Connection;

#[test]
fn test_empty_ids_without_tables_returns_zero() {
    let conn = Connection::open_in_memory().unwrap();
    let tx = conn.unchecked_transaction().unwrap();
    let provider = ProviderId::new("prov_empty");
    let count = reconnect_inserted_combo_targets(&tx, &provider, &[], map_db_error).unwrap();
    assert_eq!(count, 0);
}

#[test]
fn test_models_fixture_without_combo_targets_returns_zero() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE models (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            provider_id TEXT NOT NULL,
            model_id TEXT NOT NULL
        );
        INSERT INTO models (provider_id, model_id) VALUES ('provA', 'm1');",
    )
    .unwrap();
    let tx = conn.unchecked_transaction().unwrap();
    let provider = ProviderId::new("provA");
    let count = reconnect_inserted_combo_targets(&tx, &provider, &["m1"], map_db_error).unwrap();
    assert_eq!(count, 0);
}

#[test]
fn test_filters_provider_ids_and_null_fk_with_caller_rollback() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE models (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            provider_id TEXT NOT NULL,
            model_id TEXT NOT NULL
        );
        CREATE TABLE combo_targets (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            provider_id TEXT NOT NULL,
            model_row_id INTEGER,
            upstream_model_id TEXT NOT NULL
        );
        INSERT INTO models (id, provider_id, model_id) VALUES
            (10, 'provA', 'm1'),
            (20, 'provA', 'm2'),
            (30, 'provB', 'm3');
        INSERT INTO combo_targets (id, provider_id, model_row_id, upstream_model_id) VALUES
            (1, 'provA', NULL, 'm1'),
            (2, 'provA', 999,  'm2'),
            (3, 'provB', NULL, 'm1'),
            (4, 'provA', NULL, 'm4'),
            (5, 'provA', NULL, 'm2');",
    )
    .unwrap();

    let tx = conn.transaction().unwrap();
    let provider_a = ProviderId::new("provA");

    let count =
        reconnect_inserted_combo_targets(&tx, &provider_a, &["m1", "m3", "m4"], map_db_error)
            .unwrap();
    assert_eq!(count, 1);

    let m1_fk: Option<i64> = tx
        .query_row(
            "SELECT model_row_id FROM combo_targets WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(m1_fk, Some(10));

    let unchanged: Vec<(i64, Option<i64>)> = {
        let mut stmt = tx
            .prepare("SELECT id, model_row_id FROM combo_targets WHERE id != 1 ORDER BY id")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    assert_eq!(unchanged, [(2, Some(999)), (3, None), (4, None), (5, None)]);

    let count = reconnect_inserted_combo_targets(&tx, &provider_a, &["m2"], map_db_error).unwrap();
    assert_eq!(count, 1, "only the orphan m2 target should change");
    let linked_m2: i64 = tx
        .query_row(
            "SELECT model_row_id FROM combo_targets WHERE id = 2",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(linked_m2, 999, "already linked targets must remain intact");

    // Caller rolls back transaction: uncommitted changes are discarded
    tx.rollback().unwrap();

    let m1_fk_post_rollback: Option<i64> = conn
        .query_row(
            "SELECT model_row_id FROM combo_targets WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(m1_fk_post_rollback, None);
}

#[test]
fn test_lookup_error_missing_models_maps_raw_and_contextual() {
    let conn = Connection::open_in_memory().unwrap();
    let tx = conn.unchecked_transaction().unwrap();
    let provider = ProviderId::new("provA");

    let raw: fn(rusqlite::Error) -> CoreError = map_db_error;
    let contextual: fn(rusqlite::Error) -> CoreError =
        |e| map_db_error_ctx("query inserted models")(e);
    let cases = [
        ("raw", raw, ""),
        ("context", contextual, "query inserted models: "),
    ];

    for (name, mapper, prefix) in cases {
        let err = reconnect_inserted_combo_targets(&tx, &provider, &["m1"], mapper).unwrap_err();
        match err {
            CoreError::Database { message, source } => {
                assert!(
                    message.contains("no such table: models"),
                    "[{name}] expected sqlite table error, got: {message}"
                );
                let downcasted = source
                    .as_ref()
                    .and_then(|s| s.downcast_ref::<rusqlite::Error>())
                    .expect("source should retain the original rusqlite error");
                assert_eq!(message, format!("{prefix}{downcasted}"), "[{name}]");
            }
            other => panic!("[{name}] expected CoreError::Database, got: {other:?}"),
        }
    }
}

#[test]
fn test_batch_trigger_abort_preserves_batch_context_without_lookup_prefix() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE models (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            provider_id TEXT NOT NULL,
            model_id TEXT NOT NULL
        );
        CREATE TABLE combo_targets (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            provider_id TEXT NOT NULL,
            model_row_id INTEGER,
            upstream_model_id TEXT NOT NULL
        );
        INSERT INTO models (id, provider_id, model_id) VALUES (1, 'prov_trigger', 'm1');
        INSERT INTO combo_targets (provider_id, model_row_id, upstream_model_id)
            VALUES ('prov_trigger', NULL, 'm1');
        CREATE TRIGGER abort_target_update
            BEFORE UPDATE ON combo_targets
            BEGIN
                SELECT RAISE(ABORT, 'forced target update failure');
            END;",
    )
    .unwrap();

    let tx = conn.unchecked_transaction().unwrap();
    let provider = ProviderId::new("prov_trigger");

    let ctx_mapper = |e| map_db_error_ctx("query inserted models")(e);
    let err = reconnect_inserted_combo_targets(&tx, &provider, &["m1"], ctx_mapper).unwrap_err();

    match err {
        CoreError::Database { message, source } => {
            assert!(
                !message.contains("query inserted models"),
                "lookup context must not leak into batch error: {message}"
            );
            assert!(
                message.contains("reconnect orphan targets batch for provider prov_trigger"),
                "batch error context missing: {message}"
            );
            assert!(
                message.contains("forced target update failure"),
                "trigger message missing: {message}"
            );
            assert!(
                source
                    .as_ref()
                    .and_then(|s| s.downcast_ref::<rusqlite::Error>())
                    .is_some()
            );
        }
        other => panic!("expected CoreError::Database, got: {other:?}"),
    }
}
