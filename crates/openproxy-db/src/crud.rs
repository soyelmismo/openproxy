//! Generic CRUD operations and helpers for `rusqlite` database access.

use openproxy_types::Result;
use rusqlite::{Connection, OptionalExtension, Params, Row};

use crate::error::map_db_error_ctx;

/// Trait for converting a database row into a domain struct/type.
pub trait FromRow: Sized {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self>;
}

/// Executes a query expecting 0 or 1 row mapped via `FromRow`.
pub fn query_one<T: FromRow, P: Params>(
    conn: &Connection,
    sql: &str,
    params: P,
    ctx: impl AsRef<str>,
) -> Result<Option<T>> {
    query_one_with(conn, sql, params, T::from_row, ctx)
}

/// Executes a query expecting 0 or 1 row mapped via custom closure `f`.
pub fn query_one_with<T, P: Params>(
    conn: &Connection,
    sql: &str,
    params: P,
    f: impl FnOnce(&Row<'_>) -> rusqlite::Result<T>,
    ctx: impl AsRef<str>,
) -> Result<Option<T>> {
    conn.query_row(sql, params, f)
        .optional()
        .map_err(map_db_error_ctx(ctx.as_ref()))
}

/// Executes a query returning a list of `T` mapped via `FromRow`.
pub fn query_all<T: FromRow, P: Params>(
    conn: &Connection,
    sql: &str,
    params: P,
    ctx: impl AsRef<str>,
) -> Result<Vec<T>> {
    query_all_with(conn, sql, params, T::from_row, ctx)
}

/// Executes a query returning a list of `T` mapped via custom closure `f`.
pub fn query_all_with<T, P: Params>(
    conn: &Connection,
    sql: &str,
    params: P,
    f: impl FnMut(&Row<'_>) -> rusqlite::Result<T>,
    ctx: impl AsRef<str>,
) -> Result<Vec<T>> {
    let mut stmt = conn.prepare(sql).map_err(map_db_error_ctx(ctx.as_ref()))?;
    let rows = stmt
        .query_map(params, f)
        .map_err(map_db_error_ctx(ctx.as_ref()))?;
    let mut results = Vec::new();
    for r in rows {
        results.push(r.map_err(map_db_error_ctx(ctx.as_ref()))?);
    }
    Ok(results)
}

/// Executes an SQL modification statement (`INSERT`, `UPDATE`, `DELETE`) with mapped error context.
pub fn execute<P: Params>(
    conn: &Connection,
    sql: &str,
    params: P,
    ctx: impl AsRef<str>,
) -> Result<usize> {
    conn.execute(sql, params)
        .map_err(map_db_error_ctx(ctx.as_ref()))
}

/// Executes an `INSERT` statement and returns the `last_insert_rowid()`.
pub fn insert_get_id<P: Params>(
    conn: &Connection,
    sql: &str,
    params: P,
    ctx: impl AsRef<str>,
) -> Result<i64> {
    execute(conn, sql, params, ctx)?;
    Ok(conn.last_insert_rowid())
}

/// Declarative macros for macro-based CRUD operations.
#[macro_export]
macro_rules! db_execute {
    ($conn:expr, $sql:expr, $params:expr, $ctx:expr) => {
        $crate::crud::execute($conn, $sql, $params, $ctx)
    };
}

#[macro_export]
macro_rules! db_query_one {
    ($conn:expr, $sql:expr, $params:expr, $ctx:expr) => {
        $crate::crud::query_one($conn, $sql, $params, $ctx)
    };
    ($conn:expr, $sql:expr, $params:expr, $mapper:expr, $ctx:expr) => {
        $crate::crud::query_one_with($conn, $sql, $params, $mapper, $ctx)
    };
}

#[macro_export]
macro_rules! db_query_all {
    ($conn:expr, $sql:expr, $params:expr, $ctx:expr) => {
        $crate::crud::query_all($conn, $sql, $params, $ctx)
    };
    ($conn:expr, $sql:expr, $params:expr, $mapper:expr, $ctx:expr) => {
        $crate::crud::query_all_with($conn, $sql, $params, $mapper, $ctx)
    };
}
