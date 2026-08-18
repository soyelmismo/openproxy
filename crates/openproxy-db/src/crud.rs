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
    rows.map(|r| r.map_err(map_db_error_ctx(ctx.as_ref())))
        .collect()
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

/// Declarative macro for typed `rusqlite::Row` extraction.
///
/// Supports qualifiers:
/// - `@bool(idx)`: extracts a boolean
/// - `@opt_bool(idx)`: extracts an `Option<bool>`
/// - `@u16(idx)`: extracts a `u16`
/// - `@opt_u16(idx)`: extracts an `Option<u16>`
/// - `@json(idx)`: extracts `Option<String>` and parses as JSON deserializable `T`
/// - `@enum(idx, Type)`: extracts `String` and parses into `Type` via `FromStr`
/// - `@opt_enum(idx, Type)`: extracts `Option<String>` and parses into `Option<Type>`
/// - `(idx, Type)`: extracts typed value `row.get::<_, Type>(idx)?`
/// - `idx`: extracts value `row.get(idx)?`
#[macro_export]
macro_rules! map_row_fields {
    (@get $row:expr, @bool($idx:expr)) => {
        $row.get::<_, bool>($idx)?
    };
    (@get $row:expr, @opt_bool($idx:expr)) => {
        $row.get::<_, Option<bool>>($idx)?
    };
    (@get $row:expr, @u16($idx:expr)) => {
        $row.get::<_, u16>($idx)?
    };
    (@get $row:expr, @opt_u16($idx:expr)) => {
        $row.get::<_, Option<u16>>($idx)?
    };
    (@get $row:expr, @json($idx:expr)) => {
        $row.get::<_, Option<String>>($idx)?
            .and_then(|s| serde_json::from_str(&s).ok())
    };
    (@get $row:expr, @enum($idx:expr, $enum_ty:ty)) => {
        $row.get::<_, String>($idx)?
            .parse::<$enum_ty>()
            .map_err(|e| rusqlite::Error::FromSqlConversionFailure($idx, rusqlite::types::Type::Text, Box::from(format!("{e}"))))?
    };
    (@get $row:expr, @opt_enum($idx:expr, $enum_ty:ty)) => {
        $row.get::<_, Option<String>>($idx)?
            .and_then(|s| s.parse::<$enum_ty>().ok())
    };
    (@get $row:expr, ($idx:expr, $ty:ty)) => {
        $row.get::<_, $ty>($idx)?
    };
    (@get $row:expr, $idx:expr) => {
        $row.get($idx)?
    };

    ($row:expr, @bool($idx:expr)) => {
        $crate::map_row_fields!(@get $row, @bool($idx))
    };
    ($row:expr, @opt_bool($idx:expr)) => {
        $crate::map_row_fields!(@get $row, @opt_bool($idx))
    };
    ($row:expr, @u16($idx:expr)) => {
        $crate::map_row_fields!(@get $row, @u16($idx))
    };
    ($row:expr, @opt_u16($idx:expr)) => {
        $crate::map_row_fields!(@get $row, @opt_u16($idx))
    };
    ($row:expr, @json($idx:expr)) => {
        $crate::map_row_fields!(@get $row, @json($idx))
    };
    ($row:expr, @enum($idx:expr, $enum_ty:ty)) => {
        $crate::map_row_fields!(@get $row, @enum($idx, $enum_ty))
    };
    ($row:expr, @opt_enum($idx:expr, $enum_ty:ty)) => {
        $crate::map_row_fields!(@get $row, @opt_enum($idx, $enum_ty))
    };
    ($row:expr, ($idx:expr, $ty:ty)) => {
        $crate::map_row_fields!(@get $row, ($idx, $ty))
    };
    ($row:expr, $idx:expr) => {
        $crate::map_row_fields!(@get $row, $idx)
    };
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    #[derive(Debug, PartialEq, Eq)]
    enum TestRole {
        Admin,
        User,
    }

    impl std::str::FromStr for TestRole {
        type Err = String;
        fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
            match s {
                "admin" => Ok(Self::Admin),
                "user" => Ok(Self::User),
                other => Err(format!("unknown role: {other}")),
            }
        }
    }

    #[test]
    fn test_map_row_fields_qualifiers() {
        let conn = Connection::open_in_memory().expect("open memory db");
        conn.execute(
            "CREATE TABLE items (
                id INTEGER PRIMARY KEY,
                is_active INTEGER NOT NULL,
                port INTEGER NOT NULL,
                metadata TEXT,
                role TEXT NOT NULL
            )",
            [],
        )
        .expect("create table");

        conn.execute(
            "INSERT INTO items (id, is_active, port, metadata, role) VALUES (1, 1, 8080, '{\"k\":\"v\"}', 'admin')",
            [],
        )
        .expect("insert item");

        conn.query_row(
            "SELECT id, is_active, port, metadata, role FROM items WHERE id = 1",
            [],
            |row| {
                let id: i64 = map_row_fields!(row, 0);
                let is_active: bool = map_row_fields!(row, @bool(1));
                let port: u16 = map_row_fields!(row, @u16(2));
                let meta: Option<serde_json::Value> = map_row_fields!(row, @json(3));
                let role: TestRole = map_row_fields!(row, @enum(4, TestRole));

                assert_eq!(id, 1);
                assert!(is_active);
                assert_eq!(port, 8080);
                assert_eq!(meta.as_ref().unwrap()["k"], "v");
                assert_eq!(role, TestRole::Admin);

                Ok(())
            },
        )
        .expect("query row");
    }
}
