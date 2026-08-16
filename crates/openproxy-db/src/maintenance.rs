//! Database maintenance, integrity checking, and diagnostics.

use openproxy_types::Result;
use rusqlite::Connection;

/// Run a PRAGMA integrity check on the connection.
pub fn integrity_check(conn: &Connection) -> String {
    conn.query_row("PRAGMA integrity_check;", [], |r| r.get::<_, String>(0))
        .unwrap_or_else(|e| format!("error: {e}"))
}

/// Checkpoint the WAL log.
pub fn checkpoint_wal(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")
        .map_err(crate::error::map_db_error)
}

/// Execute incremental vacuum.
pub fn incremental_vacuum(conn: &Connection, pages: i64) -> Result<()> {
    let _ = conn.pragma_update(None, "auto_vacuum", "INCREMENTAL");
    conn.execute_batch(&format!("PRAGMA incremental_vacuum({pages});"))
        .map_err(crate::error::map_db_error)
}

/// Execute a full VACUUM.
pub fn vacuum(conn: &Connection) -> Result<()> {
    let _ = conn.pragma_update(None, "temp_store", "FILE");
    let res = conn.execute_batch("VACUUM;");
    let _ = conn.pragma_update(None, "temp_store", "MEMORY");
    res.map_err(crate::error::map_db_error)
}

/// List all non-internal SQLite user table names.
pub fn list_user_tables(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .map_err(crate::error::map_db_error)?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(crate::error::map_db_error)?;
    rows.map(|r| r.map_err(crate::error::map_db_error)).collect()
}

/// Count rows in a specific table.
pub fn count_table_rows(conn: &Connection, table: &str) -> rusqlite::Result<i64> {
    if !table.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(rusqlite::Error::InvalidQuery);
    }
    conn.query_row(&format!("SELECT COUNT(*) FROM \"{table}\""), [], |r| {
        r.get(0)
    })
}
