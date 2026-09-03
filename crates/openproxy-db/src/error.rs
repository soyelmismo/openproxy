use openproxy_types::error::CoreError;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DbErrorKind {
    #[error("unique constraint violation")]
    UniqueViolation,
    #[error("foreign key constraint violation")]
    ForeignKeyViolation,
    #[error("check constraint violation")]
    CheckViolation,
    #[error("database busy or locked")]
    BusyOrLocked,
    #[error("database is corrupt")]
    Corrupt,
    #[error("other database error")]
    Other,
}

pub fn classify_sqlite_error(err: &rusqlite::Error) -> DbErrorKind {
    let rusqlite::Error::SqliteFailure(ffi_err, msg) = err else {
        return DbErrorKind::Other;
    };

    classify_extended_code(ffi_err.extended_code)
        .unwrap_or_else(|| classify_primary_code(ffi_err.code, msg.as_deref()))
}

fn classify_extended_code(code: std::ffi::c_int) -> Option<DbErrorKind> {
    match code {
        rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE | rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY => {
            Some(DbErrorKind::UniqueViolation)
        }
        rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY => Some(DbErrorKind::ForeignKeyViolation),
        rusqlite::ffi::SQLITE_CONSTRAINT_CHECK => Some(DbErrorKind::CheckViolation),
        rusqlite::ffi::SQLITE_BUSY
        | rusqlite::ffi::SQLITE_LOCKED
        | rusqlite::ffi::SQLITE_BUSY_RECOVERY
        | rusqlite::ffi::SQLITE_BUSY_SNAPSHOT => Some(DbErrorKind::BusyOrLocked),
        rusqlite::ffi::SQLITE_CORRUPT
        | rusqlite::ffi::SQLITE_NOTADB
        | rusqlite::ffi::SQLITE_CORRUPT_VTAB
        | rusqlite::ffi::SQLITE_CORRUPT_SEQUENCE
        | rusqlite::ffi::SQLITE_CORRUPT_INDEX => Some(DbErrorKind::Corrupt),
        _ => None,
    }
}

fn classify_primary_code(code: rusqlite::ErrorCode, msg: Option<&str>) -> DbErrorKind {
    match code {
        rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked => {
            DbErrorKind::BusyOrLocked
        }
        rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase => {
            DbErrorKind::Corrupt
        }
        rusqlite::ErrorCode::ConstraintViolation => {
            msg.map_or(DbErrorKind::Other, classify_constraint_message)
        }
        _ => DbErrorKind::Other,
    }
}

fn classify_constraint_message(msg: &str) -> DbErrorKind {
    let upper = msg.to_ascii_uppercase();
    if upper.contains("FOREIGN KEY") {
        DbErrorKind::ForeignKeyViolation
    } else if upper.contains("UNIQUE") || upper.contains("PRIMARY KEY") {
        DbErrorKind::UniqueViolation
    } else if upper.contains("CHECK") {
        DbErrorKind::CheckViolation
    } else {
        DbErrorKind::Other
    }
}

pub fn map_constraint_error(err: rusqlite::Error, fk_msg: &str, unique_msg: &str) -> CoreError {
    match classify_sqlite_error(&err) {
        DbErrorKind::ForeignKeyViolation => CoreError::Validation(fk_msg.to_string()),
        DbErrorKind::UniqueViolation => CoreError::Validation(unique_msg.to_string()),
        DbErrorKind::CheckViolation => {
            CoreError::Validation(format!("check constraint violation: {err}"))
        }
        _ => map_db_error(err),
    }
}

pub fn map_db_error<E: std::error::Error + Send + Sync + 'static>(e: E) -> CoreError {
    CoreError::Database {
        message: e.to_string(),
        source: Some(Arc::new(e)),
    }
}

pub fn map_db_error_ctx<E: std::error::Error + Send + Sync + 'static>(
    ctx: impl Into<String>,
) -> impl FnOnce(E) -> CoreError {
    let c = ctx.into();
    move |e| CoreError::Database {
        message: format!("{c}: {e}"),
        source: Some(Arc::new(e)),
    }
}

/// Returns true if the error is a transient SQLite BUSY/LOCKED condition.
///
/// Inspects `CoreError::Database.source` and re-classifies the inner
/// `rusqlite::Error` via `classify_sqlite_error`. Returns false for
/// any other error variant or for `Database` errors whose source is
/// not a `rusqlite::Error` (e.g. a higher-level wrapper).
///
/// Use this from retry wrappers to decide whether a failed query is
/// safe to retry without changing the observable behavior of
/// non-transient errors (constraint violations, schema mismatches,
/// IO failures, etc.).
pub fn is_sqlite_busy(err: &openproxy_types::error::CoreError) -> bool {
    let CoreError::Database {
        source: Some(src), ..
    } = err
    else {
        return false;
    };
    src.downcast_ref::<rusqlite::Error>()
        .is_some_and(|r| classify_sqlite_error(r) == DbErrorKind::BusyOrLocked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn test_classify_unique_violation() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, val TEXT UNIQUE)",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO t VALUES (1, 'a')", []).unwrap();
        let err = conn
            .execute("INSERT INTO t VALUES (2, 'a')", [])
            .unwrap_err();

        assert_eq!(classify_sqlite_error(&err), DbErrorKind::UniqueViolation);
        let mapped = map_constraint_error(err, "fk error", "unique error");
        match mapped {
            CoreError::Validation(msg) => assert_eq!(msg, "unique error"),
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[test]
    fn test_classify_foreign_key_violation() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute("PRAGMA foreign_keys = ON", []).unwrap();
        conn.execute("CREATE TABLE parent (id INTEGER PRIMARY KEY)", [])
            .unwrap();
        conn.execute(
            "CREATE TABLE child (id INTEGER PRIMARY KEY, p_id INTEGER REFERENCES parent(id))",
            [],
        )
        .unwrap();

        let err = conn
            .execute("INSERT INTO child VALUES (1, 999)", [])
            .unwrap_err();

        assert_eq!(
            classify_sqlite_error(&err),
            DbErrorKind::ForeignKeyViolation
        );
        let mapped = map_constraint_error(err, "fk error", "unique error");
        match mapped {
            CoreError::Validation(msg) => assert_eq!(msg, "fk error"),
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[test]
    fn test_classify_check_violation() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute("CREATE TABLE t (val INTEGER CHECK (val > 0))", [])
            .unwrap();
        let err = conn.execute("INSERT INTO t VALUES (-1)", []).unwrap_err();

        assert_eq!(classify_sqlite_error(&err), DbErrorKind::CheckViolation);
        let mapped = map_constraint_error(err, "fk error", "unique error");
        match mapped {
            CoreError::Validation(msg) => assert!(msg.contains("check constraint violation")),
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[test]
    fn test_classify_other_error() {
        let conn = Connection::open_in_memory().unwrap();
        let err = conn
            .execute("SELECT * FROM nonexistent_table", [])
            .unwrap_err();

        assert_eq!(classify_sqlite_error(&err), DbErrorKind::Other);
        let mapped = map_constraint_error(err, "fk error", "unique error");
        match mapped {
            CoreError::Database { .. } => {}
            other => panic!("expected Database, got {other:?}"),
        }
    }

    #[test]
    fn is_sqlite_busy_detects_busy_in_map_db_error() {
        // Two connections on the same in-memory DB is impossible
        // (`open_in_memory` returns a private DB per connection),
        // so we use a temp file. We force `journal_mode = WAL`
        // (so reads don't block on the writer's tx) and use
        // `BEGIN IMMEDIATE` on the blocker so its tx holds a
        // RESERVED write lock that prevents other writers.
        let dir = std::env::temp_dir().join(format!(
            "openproxy-busy-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("busy.db");
        let a = Connection::open(&path).unwrap();
        a.execute_batch("PRAGMA journal_mode = WAL; CREATE TABLE t (id INTEGER PRIMARY KEY);")
            .unwrap();
        a.pragma_update(None, "busy_timeout", 0i64).unwrap();
        let b = Connection::open(&path).unwrap();
        b.execute_batch("BEGIN IMMEDIATE").unwrap();
        let err = a.execute("INSERT INTO t VALUES (1)", []).unwrap_err();
        let mapped = map_db_error(err);
        b.execute_batch("ROLLBACK").unwrap();
        assert!(is_sqlite_busy(&mapped), "expected busy, got {mapped:?}");
    }

    #[test]
    fn is_sqlite_busy_detects_busy_under_ctx_wrapper() {
        let dir = std::env::temp_dir().join(format!(
            "openproxy-busy-test-ctx-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("busy.db");
        let a = Connection::open(&path).unwrap();
        a.execute_batch("PRAGMA journal_mode = WAL; CREATE TABLE t (id INTEGER PRIMARY KEY);")
            .unwrap();
        a.pragma_update(None, "busy_timeout", 0i64).unwrap();
        let b = Connection::open(&path).unwrap();
        b.execute_batch("BEGIN IMMEDIATE").unwrap();
        let err = a.execute("INSERT INTO t VALUES (1)", []).unwrap_err();
        let mapped = map_db_error_ctx("ctx test")(err);
        b.execute_batch("ROLLBACK").unwrap();
        assert!(
            is_sqlite_busy(&mapped),
            "expected busy under ctx wrapper, got {mapped:?}",
        );
    }

    #[test]
    fn is_sqlite_busy_returns_false_for_non_busy() {
        let conn = Connection::open_in_memory().unwrap();
        let err = conn
            .execute("SELECT * FROM nonexistent_table", [])
            .unwrap_err();
        let mapped = map_db_error(err);
        assert!(
            !is_sqlite_busy(&mapped),
            "non-busy error must not classify as busy, got {mapped:?}",
        );
    }

    #[test]
    fn is_sqlite_busy_returns_false_for_non_database_variants() {
        let err = CoreError::Validation("not a busy".to_string());
        assert!(!is_sqlite_busy(&err));
        let err = CoreError::ProviderNotFound("acme".to_string());
        assert!(!is_sqlite_busy(&err));
    }

    #[test]
    fn is_sqlite_busy_returns_false_for_database_with_no_source() {
        let err = CoreError::Database {
            message: "no source".to_string(),
            source: None,
        };
        assert!(!is_sqlite_busy(&err));
    }
}
