use super::catalog::{export_models, export_providers};
use super::parse_backup_enum;
use super::routing::export_combos;
use openproxy_types::{CoreError, Result};
use rusqlite::Connection;

fn setup_test_db() -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    openproxy_db::migrations::run(&mut conn).unwrap();
    conn.execute_batch(
        "PRAGMA foreign_keys = OFF; \
         INSERT INTO providers (id, name, base_url, auth_type, format, rate_limit_scope) \
         VALUES ('p', 'P', 'https://p.com', 'bearer', 'openai', 'account'); \
         INSERT INTO models (id, provider_id, model_id, display_name, target_format, active) \
         VALUES (1, 'p', 'm', 'Model', 'openai', 1); \
         INSERT INTO combos (id, name, strategy) \
         VALUES (1, 'c', 'priority');",
    )
    .unwrap();
    conn
}

fn run_providers(c: &Connection) -> Result<()> {
    export_providers(c).map(|_| ())
}

fn run_models(c: &Connection) -> Result<()> {
    export_models(c).map(|_| ())
}

fn run_combos(c: &Connection) -> Result<()> {
    export_combos(c).map(|_| ())
}

#[test]
fn test_parse_backup_enum_helper_direct() {
    let err_str =
        parse_backup_enum("bad", 5, |_| Err::<(), _>("custom string err".to_string())).unwrap_err();
    let rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, inner) =
        err_str
    else {
        panic!("expected FromSqlConversionFailure, got {err_str:?}");
    };
    assert_eq!(idx, 5);
    let io_err = inner.downcast_ref::<std::io::Error>().unwrap();
    assert_eq!(io_err.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(io_err.get_ref().unwrap().to_string(), "custom string err");

    let err_core = parse_backup_enum("bad", 9, |_| {
        Err::<(), _>(CoreError::Validation("custom core err".into()))
    })
    .unwrap_err();
    let rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, inner) =
        err_core
    else {
        panic!("expected FromSqlConversionFailure, got {err_core:?}");
    };
    assert_eq!(idx, 9);
    let io_err = inner.downcast_ref::<std::io::Error>().unwrap();
    assert_eq!(io_err.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(
        io_err.get_ref().unwrap().to_string(),
        "validation: custom core err"
    );
}

#[test]
fn test_integrated_export_enum_failures() {
    type Exporter = fn(&Connection) -> Result<()>;
    let cases: [(&str, Exporter, usize, &str); 5] = [
        (
            "UPDATE providers SET auth_type = 'invalid' WHERE id = 'p'",
            run_providers,
            3,
            "invalid auth_type: invalid",
        ),
        (
            "UPDATE providers SET format = 'invalid' WHERE id = 'p'",
            run_providers,
            4,
            "invalid provider format: invalid",
        ),
        (
            "UPDATE providers SET rate_limit_scope = 'invalid' WHERE id = 'p'",
            run_providers,
            11,
            "invalid rate_limit_scope: invalid",
        ),
        (
            "UPDATE models SET target_format = 'invalid' WHERE id = 1",
            run_models,
            4,
            "validation: invalid target_format: invalid",
        ),
        (
            "UPDATE combos SET strategy = 'invalid' WHERE id = 1",
            run_combos,
            2,
            "invalid strategy: invalid",
        ),
    ];

    for (sql, run_export, expected_col, expected_err) in cases {
        let conn = setup_test_db();
        conn.execute_batch(&format!("PRAGMA ignore_check_constraints = ON; {sql}"))
            .unwrap();
        let err = run_export(&conn).unwrap_err();
        let CoreError::Database { message, source } = err else {
            panic!("expected CoreError::Database, got {err:?}");
        };
        let sqlite = source
            .as_deref()
            .and_then(|s| s.downcast_ref::<rusqlite::Error>())
            .expect("typed rusqlite error");
        assert_eq!(message, sqlite.to_string());
        let rusqlite::Error::FromSqlConversionFailure(col, rusqlite::types::Type::Text, inner) =
            sqlite
        else {
            panic!("expected FromSqlConversionFailure, got {sqlite:?}");
        };
        assert_eq!(*col, expected_col);
        let io_err = inner.downcast_ref::<std::io::Error>().expect("io::Error");
        assert_eq!(io_err.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(
            io_err.get_ref().expect("nested parse error").to_string(),
            expected_err
        );
    }
}
