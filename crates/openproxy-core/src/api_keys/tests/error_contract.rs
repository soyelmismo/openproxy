use super::{fresh_pool, make_input};
use crate::api_keys::{self, UpdateParams};
use crate::error::{CoreError, Result};
use crate::ids::ApiKeyId;
use rusqlite::Connection;

fn assert_sql_error(error: CoreError, context: &str) {
    let CoreError::Database { message, source } = error else {
        panic!("expected database error, got {error:?}");
    };
    let source = source.expect("SQL errors retain their source");
    let sqlite = source
        .downcast_ref::<rusqlite::Error>()
        .expect("original rusqlite error");
    assert_eq!(message, format!("{context}: {sqlite}"));
}

#[test]
fn missing_tables_preserve_all_operation_contexts() {
    type Operation = fn(&Connection, ApiKeyId) -> Result<()>;
    let cases: [(&str, Operation); 8] = [
        ("get", |conn, id| api_keys::get_by_id(conn, id).map(|_| ())),
        ("revoke", api_keys::revoke),
        ("delete", api_keys::hard_delete),
        ("regenerate", |conn, id| {
            api_keys::regenerate(conn, id).map(|_| ())
        }),
        ("touch_last_used", api_keys::touch_last_used),
        ("count", |conn, id| {
            api_keys::update(conn, id, UpdateParams::default())
        }),
        ("update", |conn, id| {
            api_keys::update(
                conn,
                id,
                UpdateParams {
                    label: Some("renamed"),
                    ..Default::default()
                },
            )
        }),
        ("usage_summary for", |conn, id| {
            api_keys::usage_summary(conn, id).map(|_| ())
        }),
    ];
    for (operation, invoke) in cases {
        let conn = Connection::open_in_memory().unwrap();
        let error = invoke(&conn, ApiKeyId(37)).unwrap_err();
        assert_sql_error(error, &format!("{operation} api_key 37"));
    }

    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE usage (request_id TEXT, status_code INTEGER, cost_usd REAL, api_key_id INTEGER);",
    ).unwrap();
    assert_sql_error(
        api_keys::usage_summary(&conn, ApiKeyId(37)).unwrap_err(),
        "select last_used_at for api_key 37",
    );
}

#[test]
fn row_conversion_preserves_typed_source_and_get_context() {
    let (conn, _) = fresh_pool();
    let (key, _) = api_keys::create(&conn, make_input("conversion"), "test").unwrap();
    conn.execute(
        "UPDATE api_keys SET key_hash = X'FF' WHERE id = ?1",
        [key.id.0],
    )
    .unwrap();
    let error = api_keys::get_by_id(&conn, key.id).unwrap_err();
    if let CoreError::Database {
        source: Some(source),
        ..
    } = &error
    {
        assert!(matches!(
            source.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::InvalidColumnType(
                1,
                _,
                rusqlite::types::Type::Blob
            ))
        ));
    } else {
        panic!("expected SQL conversion error, got {error:?}");
    }
    assert_sql_error(error, &format!("get api_key {}", key.id.0));
}

#[test]
fn trigger_abort_preserves_revoke_context_and_row() {
    let (conn, _) = fresh_pool();
    let (key, _) = api_keys::create(&conn, make_input("trigger"), "test").unwrap();
    conn.execute_batch(
        "CREATE TRIGGER abort_revoke BEFORE UPDATE ON api_keys
         BEGIN SELECT RAISE(ABORT, 'forced revoke failure'); END;",
    )
    .unwrap();
    assert_sql_error(
        api_keys::revoke(&conn, key.id).unwrap_err(),
        &format!("revoke api_key {}", key.id.0),
    );
    let row = api_keys::get_by_id(&conn, key.id).unwrap().unwrap();
    assert!(row.is_active);
    assert!(row.revoked_at.is_none());
}

#[test]
fn invalid_expiration_retains_absent_source() {
    let error = api_keys::is_expired(Some("not-a-date"), chrono::Utc::now()).unwrap_err();
    let CoreError::Database { message, source } = error else {
        panic!("expected database error, got {error:?}");
    };
    assert!(source.is_none());
    let parse_error = openproxy_types::timestamp::parse_timestamp("not-a-date").unwrap_err();
    assert_eq!(
        message,
        format!("invalid expires_at \"not-a-date\": {parse_error}")
    );
}
