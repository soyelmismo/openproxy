use openproxy_types::error::CoreError;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbErrorKind {
    UniqueViolation,
    ForeignKeyViolation,
    CheckViolation,
    BusyOrLocked,
    Corrupt,
    Other,
}

pub fn classify_sqlite_error(err: &rusqlite::Error) -> DbErrorKind {
    match err {
        rusqlite::Error::SqliteFailure(ffi_err, msg) => {
            // Check extended error code first for maximum precision
            match ffi_err.extended_code {
                rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
                | rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY => {
                    return DbErrorKind::UniqueViolation;
                }
                rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY => {
                    return DbErrorKind::ForeignKeyViolation;
                }
                rusqlite::ffi::SQLITE_CONSTRAINT_CHECK => {
                    return DbErrorKind::CheckViolation;
                }
                rusqlite::ffi::SQLITE_BUSY
                | rusqlite::ffi::SQLITE_LOCKED
                | rusqlite::ffi::SQLITE_BUSY_RECOVERY
                | rusqlite::ffi::SQLITE_BUSY_SNAPSHOT => {
                    return DbErrorKind::BusyOrLocked;
                }
                rusqlite::ffi::SQLITE_CORRUPT
                | rusqlite::ffi::SQLITE_NOTADB
                | rusqlite::ffi::SQLITE_CORRUPT_VTAB
                | rusqlite::ffi::SQLITE_CORRUPT_SEQUENCE
                | rusqlite::ffi::SQLITE_CORRUPT_INDEX => {
                    return DbErrorKind::Corrupt;
                }
                _ => {}
            }

            // Fallback to primary error code / message
            match ffi_err.code {
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked => {
                    DbErrorKind::BusyOrLocked
                }
                rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase => {
                    DbErrorKind::Corrupt
                }
                rusqlite::ErrorCode::ConstraintViolation => {
                    if let Some(m) = msg {
                        let upper = m.to_ascii_uppercase();
                        if upper.contains("FOREIGN KEY") {
                            DbErrorKind::ForeignKeyViolation
                        } else if upper.contains("UNIQUE") || upper.contains("PRIMARY KEY") {
                            DbErrorKind::UniqueViolation
                        } else if upper.contains("CHECK") {
                            DbErrorKind::CheckViolation
                        } else {
                            DbErrorKind::Other
                        }
                    } else {
                        DbErrorKind::Other
                    }
                }
                _ => DbErrorKind::Other,
            }
        }
        _ => DbErrorKind::Other,
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

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn test_classify_unique_violation() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val TEXT UNIQUE)", [])
            .unwrap();
        conn.execute("INSERT INTO t VALUES (1, 'a')", []).unwrap();
        let err = conn.execute("INSERT INTO t VALUES (2, 'a')", []).unwrap_err();

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

        let err = conn.execute("INSERT INTO child VALUES (1, 999)", []).unwrap_err();

        assert_eq!(classify_sqlite_error(&err), DbErrorKind::ForeignKeyViolation);
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
        let err = conn.execute("SELECT * FROM nonexistent_table", []).unwrap_err();

        assert_eq!(classify_sqlite_error(&err), DbErrorKind::Other);
        let mapped = map_constraint_error(err, "fk error", "unique error");
        match mapped {
            CoreError::Database { .. } => {}
            other => panic!("expected Database, got {other:?}"),
        }
    }
}
