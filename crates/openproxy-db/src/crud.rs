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

/// Helper function for checking existence via SQL query.
pub fn exists<P: Params>(
    conn: &Connection,
    sql: &str,
    params: P,
    ctx: impl AsRef<str>,
) -> Result<bool> {
    let exists = conn
        .query_row(sql, params, |r| r.get::<_, i64>(0))
        .map(|v| v != 0)
        .map_err(map_db_error_ctx(ctx.as_ref()))?;
    Ok(exists)
}

/// Declarative macros for macro-based CRUD operations.
#[macro_export]
macro_rules! db_execute {
    ($conn:expr, $sql:expr, $params:expr, $ctx:expr $(,)?) => {
        $crate::crud::execute($conn, $sql, $params, $ctx)
    };
}

#[macro_export]
macro_rules! db_exists {
    ($conn:expr, $table:literal, WHERE $col:ident = $val:expr, $ctx:expr $(,)?) => {
        $crate::crud::exists(
            $conn,
            concat!(
                "SELECT EXISTS(SELECT 1 FROM ",
                $table,
                " WHERE ",
                stringify!($col),
                " = ?1)"
            ),
            ::rusqlite::params![$val],
            $ctx,
        )
    };
    ($conn:expr, $table:literal, WHERE id = $val:expr, $ctx:expr $(,)?) => {
        $crate::crud::exists(
            $conn,
            concat!("SELECT EXISTS(SELECT 1 FROM ", $table, " WHERE id = ?1)"),
            ::rusqlite::params![$val],
            $ctx,
        )
    };
    ($conn:expr, $sql:expr, $params:expr, $ctx:expr $(,)?) => {
        $crate::crud::exists($conn, $sql, $params, $ctx)
    };
}

#[macro_export]
macro_rules! db_query_one {
    ($conn:expr, $sql:expr, $params:expr, $ctx:expr $(,)?) => {
        $crate::crud::query_one($conn, $sql, $params, $ctx)
    };
    ($conn:expr, $sql:expr, $params:expr, $mapper:expr, $ctx:expr $(,)?) => {
        $crate::crud::query_one_with($conn, $sql, $params, $mapper, $ctx)
    };
}

#[macro_export]
macro_rules! db_query_all {
    ($conn:expr, $sql:expr, $params:expr, $ctx:expr $(,)?) => {
        $crate::crud::query_all($conn, $sql, $params, $ctx)
    };
    ($conn:expr, $sql:expr, $params:expr, $mapper:expr, $ctx:expr $(,)?) => {
        $crate::crud::query_all_with($conn, $sql, $params, $mapper, $ctx)
    };
}

/// Declarative macro for typed `rusqlite::Row` extraction.
///
/// Supports qualifiers:
/// - `@bool(idx)`: extracts a boolean
/// - `@opt_bool(idx)`: extracts an `Option<bool>`
/// - `@u8(idx)`: extracts a `u8`
/// - `@box_str(idx)`: extracts a `String` and converts to `Box<str>`
/// - `@opt_box_str(idx)`: extracts an `Option<String>` and converts to `Option<Box<str>>`
/// - `@u16(idx)`: extracts a `u16`
/// - `@opt_u16(idx)`: extracts an `Option<u16>`
/// - `@u32(idx)`: extracts a `u32`
/// - `@opt_u32(idx)`: extracts an `Option<u32>`
/// - `@u64(idx)`: extracts a `u64`
/// - `@opt_u64(idx)`: extracts an `Option<u64>`
/// - `@id(idx, Type)`: extracts numeric id as `Type(i64)`
/// - `@opt_id(idx, Type)`: extracts optional numeric id as `Option<Type>`
/// - `@id_str(idx, Type)`: extracts string id as `Type::new(String)`
/// - `@enum_parse(idx, Type)`: extracts `&str` and parses into `Type` via `FromStr`
/// - `@opt_enum_parse(idx, Type)`: extracts `Option<&str>` and parses into `Option<Type>`
/// - `@enum(idx, Type)`: alias for `@enum_parse`
/// - `@opt_enum(idx, Type)`: alias for `@opt_enum_parse`
/// - `@from_db(idx, Type)`: extracts `Option<&str>` and calls `Type::from_db(Option<&str>)`
/// - `@json(idx)`: extracts `Option<String>` and parses as JSON deserializable `T`
/// - `@opt_default(idx, [Type,] default)`: extracts `Option<T>` or falls back to `default`
/// - `(idx, Type)`: extracts typed value `row.get::<_, Type>(idx)?`
/// - `idx`: extracts value `row.get(idx)?`
#[macro_export]
macro_rules! map_row_fields {
    (@get $row:expr, @box_str($idx:expr)) => {
        $row.get::<_, String>($idx)?.into_boxed_str()
    };
    (@get $row:expr, @box_str_default($idx:expr, $default:expr)) => {
        $row.get::<_, Option<String>>($idx)?
            .map(String::into_boxed_str)
            .unwrap_or_else(|| $default.into())
    };
    (@get $row:expr, @opt_box_str($idx:expr)) => {
        $row.get::<_, Option<String>>($idx)?.map(String::into_boxed_str)
    };
    (@get $row:expr, @bool($idx:expr)) => {
        $row.get::<_, bool>($idx)?
    };
    (@get $row:expr, @opt_bool($idx:expr)) => {
        $row.get::<_, Option<bool>>($idx)?
    };
    (@get $row:expr, @u8($idx:expr)) => {
        $row.get::<_, u8>($idx)?
    };
    (@get $row:expr, @u16($idx:expr)) => {
        $row.get::<_, u16>($idx)?
    };
    (@get $row:expr, @opt_u16($idx:expr)) => {
        $row.get::<_, Option<u16>>($idx)?
    };
    (@get $row:expr, @u32($idx:expr)) => {
        $row.get::<_, u32>($idx)?
    };
    (@get $row:expr, @opt_u32($idx:expr)) => {
        $row.get::<_, Option<u32>>($idx)?
    };
    (@get $row:expr, @u64($idx:expr)) => {
        $row.get::<_, i64>($idx)? as u64
    };
    (@get $row:expr, @opt_u64($idx:expr)) => {
        $row.get::<_, Option<i64>>($idx)?.map(|v| v as u64)
    };
    (@get $row:expr, @id($idx:expr, $id_ty:ident)) => {
        $id_ty($row.get::<_, i64>($idx)?)
    };
    (@get $row:expr, @opt_id($idx:expr, $id_ty:ident)) => {
        $row.get::<_, Option<i64>>($idx)?.map($id_ty)
    };
    (@get $row:expr, @id_str($idx:expr, $id_ty:ident)) => {
        $id_ty($row.get::<_, String>($idx)?)
    };
    (@get $row:expr, @enum_parse($idx:expr, $enum_ty:path)) => {
        $row.get_ref($idx)?
            .as_str()?
            .parse::<$enum_ty>()
            .map_err(|e| ::rusqlite::Error::FromSqlConversionFailure($idx, ::rusqlite::types::Type::Text, Box::from(format!("{e}"))))?
    };
    (@get $row:expr, @enum_or_default($idx:expr, $enum_ty:path)) => {
        match $row.get_ref($idx)? {
            ::rusqlite::types::ValueRef::Null => <$enum_ty>::default(),
            val => val
                .as_str()
                .ok()
                .and_then(|s| s.parse::<$enum_ty>().ok())
                .unwrap_or_default(),
        }
    };
    (@get $row:expr, @opt_enum_parse($idx:expr, $enum_ty:path)) => {
        match $row.get_ref($idx)? {
            ::rusqlite::types::ValueRef::Null => None,
            val => val
                .as_str()
                .ok()
                .and_then(|s| s.parse::<$enum_ty>().ok()),
        }
    };
    (@get $row:expr, @enum($idx:expr, $enum_ty:path)) => {
        $crate::map_row_fields!(@get $row, @enum_parse($idx, $enum_ty))
    };
    (@get $row:expr, @opt_enum($idx:expr, $enum_ty:path)) => {
        $crate::map_row_fields!(@get $row, @opt_enum_parse($idx, $enum_ty))
    };
    (@get $row:expr, @from_db($idx:expr, $ty:path)) => {
        <$ty>::from_db(match $row.get_ref($idx)? {
            ::rusqlite::types::ValueRef::Null => None,
            val => val.as_str().ok(),
        })
    };
    (@get $row:expr, @json($idx:expr)) => {
        $row.get::<_, Option<String>>($idx)?
            .and_then(|s| ::serde_json::from_str(&s).ok())
    };
    (@get $row:expr, @opt_json($idx:expr, $ty:path)) => {
        $row.get::<_, Option<String>>($idx)?
            .and_then(|s| ::serde_json::from_str::<$ty>(&s).ok())
    };
    (@get $row:expr, @json_or_default($idx:expr, $ty:path)) => {
        $row.get::<_, Option<String>>($idx)?
            .and_then(|s| ::serde_json::from_str::<$ty>(&s).ok())
            .unwrap_or_default()
    };
    (@get $row:expr, @json_or_default($idx:expr)) => {
        $row.get::<_, Option<String>>($idx)?
            .and_then(|s| ::serde_json::from_str(&s).ok())
            .unwrap_or_default()
    };
    (@get $row:expr, @expr($val:expr)) => {
        $val
    };
    (@get $row:expr, @opt_default($idx:expr, $ty:ty, $default:expr)) => {
        $row.get::<_, Option<$ty>>($idx)?.unwrap_or($default)
    };
    (@get $row:expr, @opt_default($idx:expr, $default:expr)) => {
        $row.get::<_, Option<_>>($idx)?.unwrap_or_else(|| $default).into()
    };
    (@get $row:expr, ($idx:expr, $ty:ty)) => {
        $row.get::<_, $ty>($idx)?
    };
    (@get $row:expr, $idx:expr) => {
        $row.get($idx)?
    };

    ($row:expr, $($tt:tt)+) => {
        $crate::map_row_fields!(@get $row, $($tt)+)
    };
}

/// Declarative macro to define standard `SELECT` projections for a table.
#[macro_export]
macro_rules! def_table_select {
    ($macro_name:ident, $table:literal, $cols:literal $(,)?) => {
        #[allow(unused_macros)]
        macro_rules! $macro_name {
            ($tail:expr) => {
                concat!("SELECT ", $cols, " FROM ", $table, " ", $tail)
            };
            () => {
                concat!("SELECT ", $cols, " FROM ", $table)
            };
        }
    };
}

/// Declarative macro for updating a single field in a table.
#[macro_export]
macro_rules! db_update_field {
    ($conn:expr, $table:literal, $col:ident = $val:expr, WHERE $id_col:ident = $id_val:expr, $ctx:expr $(,)?) => {
        $crate::crud::execute(
            $conn,
            concat!(
                "UPDATE ",
                $table,
                " SET ",
                stringify!($col),
                " = ?1 WHERE ",
                stringify!($id_col),
                " = ?2"
            ),
            ::rusqlite::params![$val, $id_val],
            $ctx,
        )
    };
    ($conn:expr, $table:literal, $col:ident = $val:expr, WHERE id = $id_val:expr, $ctx:expr $(,)?) => {
        $crate::crud::execute(
            $conn,
            concat!(
                "UPDATE ",
                $table,
                " SET ",
                stringify!($col),
                " = ?1 WHERE id = ?2"
            ),
            ::rusqlite::params![$val, $id_val],
            $ctx,
        )
    };
    ($conn:expr, $table:literal, $col:literal = $val:expr, WHERE $id_col:literal = $id_val:expr, $ctx:expr $(,)?) => {
        $crate::crud::execute(
            $conn,
            concat!(
                "UPDATE ",
                $table,
                " SET ",
                $col,
                " = ?1 WHERE ",
                $id_col,
                " = ?2"
            ),
            ::rusqlite::params![$val, $id_val],
            $ctx,
        )
    };
    ($conn:expr, $table:literal, $col:literal = $val:expr, WHERE id = $id_val:expr, $ctx:expr $(,)?) => {
        $crate::crud::execute(
            $conn,
            concat!("UPDATE ", $table, " SET ", $col, " = ?1 WHERE id = ?2"),
            ::rusqlite::params![$val, $id_val],
            $ctx,
        )
    };
    ($conn:expr, $table:literal, $col:literal, $val:expr, $id_val:expr, $ctx:expr $(,)?) => {
        $crate::crud::execute(
            $conn,
            concat!("UPDATE ", $table, " SET ", $col, " = ?1 WHERE id = ?2"),
            ::rusqlite::params![$val, $id_val],
            $ctx,
        )
    };
    ($conn:expr, $table:literal, $col:literal, $val:expr, $id_col:literal, $id_val:expr, $ctx:expr $(,)?) => {
        $crate::crud::execute(
            $conn,
            concat!(
                "UPDATE ",
                $table,
                " SET ",
                $col,
                " = ?1 WHERE ",
                $id_col,
                " = ?2"
            ),
            ::rusqlite::params![$val, $id_val],
            $ctx,
        )
    };
}

/// Declarative macro for mapping a `rusqlite::Row` into a typed domain struct.
#[macro_export]
macro_rules! map_row_struct {
    ($row:ident, $struct_ty:ident { $($fields:tt)* }) => {
        $crate::map_row_struct!(@build $row, $struct_ty, {}, $($fields)*)
    };
    ($row:ident => $struct_ty:ident { $($fields:tt)* }) => {
        $crate::map_row_struct!(@build $row, $struct_ty, {}, $($fields)*)
    };
    ($row:expr => $struct_ty:ident { $($fields:tt)* }) => {
        $crate::map_row_struct!(@build $row, $struct_ty, {}, $($fields)*)
    };

    (@build $row:ident, $struct_ty:ident, { $($built:tt)* }, $(,)?) => {
        Ok($struct_ty {
            $($built)*
        })
    };

    (@build $row:ident, $struct_ty:ident, { $($built:tt)* }, $field:ident : @ $q:ident ( $($args:tt)* ) , $($rest:tt)*) => {
        $crate::map_row_struct!(
            @build $row,
            $struct_ty,
            {
                $($built)*
                $field: $crate::map_row_fields!(@get $row, @ $q ( $($args)* )),
            },
            $($rest)*
        )
    };

    (@build $row:ident, $struct_ty:ident, { $($built:tt)* }, $field:ident : @ $q:ident ( $($args:tt)* )) => {
        $crate::map_row_struct!(
            @build $row,
            $struct_ty,
            {
                $($built)*
                $field: $crate::map_row_fields!(@get $row, @ $q ( $($args)* )),
            },
        )
    };

    (@build $row:ident, $struct_ty:ident, { $($built:tt)* }, $field:ident : ( $idx:expr, $ty:ty ) , $($rest:tt)*) => {
        $crate::map_row_struct!(
            @build $row,
            $struct_ty,
            {
                $($built)*
                $field: $crate::map_row_fields!(@get $row, ($idx, $ty)),
            },
            $($rest)*
        )
    };

    (@build $row:ident, $struct_ty:ident, { $($built:tt)* }, $field:ident : ( $idx:expr, $ty:ty )) => {
        $crate::map_row_struct!(
            @build $row,
            $struct_ty,
            {
                $($built)*
                $field: $crate::map_row_fields!(@get $row, ($idx, $ty)),
            },
        )
    };

    (@build $row:ident, $struct_ty:ident, { $($built:tt)* }, $field:ident : $idx:expr , $($rest:tt)*) => {
        $crate::map_row_struct!(
            @build $row,
            $struct_ty,
            {
                $($built)*
                $field: $crate::map_row_fields!(@get $row, $idx),
            },
            $($rest)*
        )
    };

    (@build $row:ident, $struct_ty:ident, { $($built:tt)* }, $field:ident : $idx:expr) => {
        $crate::map_row_struct!(
            @build $row,
            $struct_ty,
            {
                $($built)*
                $field: $crate::map_row_fields!(@get $row, $idx),
            },
        )
    };
}

/// Declarative macro for mapping a `rusqlite::Row` into a typed tuple $O(1)$.
///
/// Supports qualifiers:
/// - `@qualifier(...)` (e.g. `@id(0, AccountId)`, `@enum_parse(1, Status)`)
/// - `(idx, Type)` (e.g. `(0, i64)`, `(1, String)`)
/// - `idx` (e.g. `0`, `1`)
///
/// Syntax examples:
/// - `map_row_tuple!(row => (0, 1))`
/// - `map_row_tuple!(row, ((0, i64), (1, String)))`
/// - `map_row_tuple!(row => (@id(0, AccountId), 1, @bool(2)))`
#[macro_export]
macro_rules! map_row_tuple {
    ($row:ident, ( $($elem:tt)+ )) => {
        $crate::map_row_tuple!($row => ( $($elem)+ ))
    };
    ($row:ident => ( $($elem:tt)+ )) => {
        (|| -> std::result::Result<_, rusqlite::Error> {
            $crate::map_row_tuple!(@tuple $row, (), $($elem)+)
        })()
    };
    ($row:expr, ( $($elem:tt)+ )) => {
        $crate::map_row_tuple!($row => ( $($elem)+ ))
    };
    ($row:expr => ( $($elem:tt)+ )) => {
        {
            let row = $row;
            (|| -> std::result::Result<_, rusqlite::Error> {
                $crate::map_row_tuple!(@tuple row, (), $($elem)+)
            })()
        }
    };

    (@tuple $row:ident, ( $($built:expr,)* ), @ $q:ident ( $($args:tt)* ) , $($rest:tt)+) => {
        $crate::map_row_tuple!(
            @tuple $row,
            ( $($built,)* $crate::map_row_fields!(@get $row, @ $q ( $($args)* )), ),
            $($rest)+
        )
    };
    (@tuple $row:ident, ( $($built:expr,)* ), @ $q:ident ( $($args:tt)* ) $(,)?) => {
        Ok(( $($built,)* $crate::map_row_fields!(@get $row, @ $q ( $($args)* )), ))
    };

    (@tuple $row:ident, ( $($built:expr,)* ), ( $idx:expr, $ty:ty ) , $($rest:tt)+) => {
        $crate::map_row_tuple!(
            @tuple $row,
            ( $($built,)* $crate::map_row_fields!(@get $row, ($idx, $ty)), ),
            $($rest)+
        )
    };
    (@tuple $row:ident, ( $($built:expr,)* ), ( $idx:expr, $ty:ty ) $(,)?) => {
        Ok(( $($built,)* $crate::map_row_fields!(@get $row, ($idx, $ty)), ))
    };

    (@tuple $row:ident, ( $($built:expr,)* ), $idx:expr , $($rest:tt)+) => {
        $crate::map_row_tuple!(
            @tuple $row,
            ( $($built,)* $crate::map_row_fields!(@get $row, $idx), ),
            $($rest)+
        )
    };
    (@tuple $row:ident, ( $($built:expr,)* ), $idx:expr $(,)?) => {
        Ok(( $($built,)* $crate::map_row_fields!(@get $row, $idx), ))
    };
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[derive(Debug, PartialEq, Eq)]
    enum TestMode {
        Fast,
        Safe,
    }

    impl TestMode {
        fn from_db(s: Option<&str>) -> Self {
            match s {
                Some("fast") => Self::Fast,
                _ => Self::Safe,
            }
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    struct CustomRowId(i64);

    #[derive(Debug, PartialEq, Eq)]
    struct CustomStrId(String);

    #[derive(Debug, PartialEq, Eq)]
    struct TestItem {
        id: CustomRowId,
        parent_id: Option<CustomRowId>,
        name_id: CustomStrId,
        is_active: bool,
        opt_bool: Option<bool>,
        u8_val: u8,
        port: u16,
        opt_u16_val: Option<u16>,
        u32_val: u32,
        opt_u32_val: Option<u32>,
        u64_val: u64,
        opt_u64_val: Option<u64>,
        role: TestRole,
        opt_role: Option<TestRole>,
        mode: TestMode,
        metadata: Option<serde_json::Value>,
        tag: String,
        description: String,
        custom_computed: String,
        default_payload: serde_json::Value,
    }

    #[test]
    fn test_map_row_struct_and_qualifiers() {
        let conn = Connection::open_in_memory().expect("open memory db");
        conn.execute(
            "CREATE TABLE items (
                id INTEGER PRIMARY KEY,
                parent_id INTEGER,
                name_id TEXT NOT NULL,
                is_active INTEGER NOT NULL,
                opt_bool INTEGER,
                u8_val INTEGER NOT NULL,
                port INTEGER NOT NULL,
                opt_u16_val INTEGER,
                u32_val INTEGER NOT NULL,
                opt_u32_val INTEGER,
                u64_val INTEGER NOT NULL,
                opt_u64_val INTEGER,
                role TEXT NOT NULL,
                opt_role TEXT,
                mode TEXT,
                metadata TEXT,
                tag TEXT,
                description TEXT NOT NULL,
                default_payload TEXT
            )",
            [],
        )
        .expect("create table");

        conn.execute(
            "INSERT INTO items (
                id, parent_id, name_id, is_active, opt_bool, u8_val, port, opt_u16_val,
                u32_val, opt_u32_val, u64_val, opt_u64_val, role, opt_role, mode, metadata, tag, description, default_payload
            ) VALUES (
                1, 42, 'str-id-1', 1, 0, 8, 8080, 9090,
                3000, 4000, 50000, 60000, 'admin', 'user', 'fast', '{\"k\":\"v\"}', NULL, 'a test item', NULL
            )",
            [],
        )
        .expect("insert item");

        let computed_val = "computed-value".to_string();

        let item: TestItem = conn
            .query_row("SELECT * FROM items WHERE id = 1", [], |row| {
                map_row_struct!(row, TestItem {
                    id: @id(0, CustomRowId),
                    parent_id: @opt_id(1, CustomRowId),
                    name_id: @id_str(2, CustomStrId),
                    is_active: @bool(3),
                    opt_bool: @opt_bool(4),
                    u8_val: @u8(5),
                    port: @u16(6),
                    opt_u16_val: @opt_u16(7),
                    u32_val: @u32(8),
                    opt_u32_val: @opt_u32(9),
                    u64_val: @u64(10),
                    opt_u64_val: @opt_u64(11),
                    role: @enum_parse(12, TestRole),
                    opt_role: @opt_enum_parse(13, TestRole),
                    mode: @from_db(14, TestMode),
                    metadata: @json(15),
                    tag: @opt_default(16, "default_tag".to_string()),
                    description: 17,
                    custom_computed: @expr(computed_val.clone()),
                    default_payload: @json_or_default(18),
                })
            })
            .expect("query row");

        assert_eq!(item.id, CustomRowId(1));
        assert_eq!(item.parent_id, Some(CustomRowId(42)));
        assert_eq!(item.name_id, CustomStrId("str-id-1".into()));
        assert!(item.is_active);
        assert_eq!(item.opt_bool, Some(false));
        assert_eq!(item.u8_val, 8);
        assert_eq!(item.port, 8080);
        assert_eq!(item.opt_u16_val, Some(9090));
        assert_eq!(item.u32_val, 3000);
        assert_eq!(item.opt_u32_val, Some(4000));
        assert_eq!(item.u64_val, 50000);
        assert_eq!(item.opt_u64_val, Some(60000));
        assert_eq!(item.role, TestRole::Admin);
        assert_eq!(item.opt_role, Some(TestRole::User));
        assert_eq!(item.mode, TestMode::Fast);
        assert_eq!(item.metadata.as_ref().unwrap()["k"], "v");
        assert_eq!(item.tag, "default_tag");
        assert_eq!(item.description, "a test item");
        assert_eq!(item.custom_computed, "computed-value");
        assert_eq!(item.default_payload, serde_json::Value::Null);
    }

    #[derive(Debug, PartialEq, Eq)]
    struct SimpleUser {
        id: i64,
        name: String,
    }

    impl FromRow for SimpleUser {
        fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
            Ok(Self {
                id: row.get(0)?,
                name: row.get(1)?,
            })
        }
    }

    def_table_select!(user_select, "users", "id, name");

    #[test]
    fn test_crud_macros() {
        let conn = Connection::open_in_memory().expect("open memory db");
        db_execute!(
            &conn,
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL)",
            [],
            "create users table"
        )
        .expect("create table");

        let inserted = db_execute!(
            &conn,
            "INSERT INTO users (id, name) VALUES (?, ?)",
            rusqlite::params![1, "Alice"],
            "insert alice"
        )
        .expect("insert");
        assert_eq!(inserted, 1);

        db_execute!(
            &conn,
            "INSERT INTO users (id, name) VALUES (?, ?)",
            rusqlite::params![2, "Bob"],
            "insert bob"
        )
        .expect("insert");

        // db_query_one with FromRow & def_table_select!
        let alice: Option<SimpleUser> = db_query_one!(
            &conn,
            user_select!("WHERE id = ?1"),
            rusqlite::params![1],
            "query alice"
        )
        .expect("query one");
        assert_eq!(
            alice,
            Some(SimpleUser {
                id: 1,
                name: "Alice".into()
            })
        );

        let non_existent: Option<SimpleUser> = db_query_one!(
            &conn,
            user_select!("WHERE id = ?1"),
            rusqlite::params![999],
            "query non existent"
        )
        .expect("query one non existent");
        assert_eq!(non_existent, None);

        // db_query_one with custom mapper
        let bob_name: Option<String> = db_query_one!(
            &conn,
            "SELECT name FROM users WHERE id = ?",
            rusqlite::params![2],
            |row| row.get(0),
            "query bob name"
        )
        .expect("query one mapper");
        assert_eq!(bob_name, Some("Bob".into()));

        // db_query_all with FromRow & def_table_select!
        let all_users: Vec<SimpleUser> = db_query_all!(
            &conn,
            user_select!("ORDER BY id ASC"),
            [],
            "query all users"
        )
        .expect("query all");
        assert_eq!(
            all_users,
            vec![
                SimpleUser {
                    id: 1,
                    name: "Alice".into()
                },
                SimpleUser {
                    id: 2,
                    name: "Bob".into()
                },
            ]
        );

        // db_query_all with custom mapper
        let names: Vec<String> = db_query_all!(
            &conn,
            "SELECT name FROM users ORDER BY id DESC",
            [],
            |row| row.get(0),
            "query all names"
        )
        .expect("query all mapper");
        assert_eq!(names, vec!["Bob", "Alice"]);

        // db_update_field!
        let affected = db_update_field!(
            &conn,
            "users",
            name = "Alice Updated",
            WHERE id = 1,
            "update user name"
        )
        .expect("update field");
        assert_eq!(affected, 1);

        let updated_alice: Option<SimpleUser> = db_query_one!(
            &conn,
            user_select!("WHERE id = ?1"),
            rusqlite::params![1],
            "query updated alice"
        )
        .expect("query updated alice");
        assert_eq!(
            updated_alice,
            Some(SimpleUser {
                id: 1,
                name: "Alice Updated".into()
            })
        );

        // db_exists!
        let exists_1 = db_exists!(&conn, "users", WHERE id = 1, "check user 1 exists").unwrap();
        assert!(exists_1);
        let exists_999 =
            db_exists!(&conn, "users", WHERE id = 999, "check user 999 exists").unwrap();
        assert!(!exists_999);
        let exists_named =
            db_exists!(&conn, "users", WHERE name = "Bob", "check Bob exists").unwrap();
        assert!(exists_named);

        // map_row_tuple!
        let tuple_res: (i64, String) = conn
            .query_row(
                "SELECT id, name FROM users WHERE id = 2",
                [],
                |row| map_row_tuple!(row => (0, 1)),
            )
            .unwrap();
        assert_eq!(tuple_res, (2, "Bob".to_string()));

        let typed_tuple: (i64, String) = conn
            .query_row(
                "SELECT id, name FROM users WHERE id = 2",
                [],
                |row| map_row_tuple!(row => ((0, i64), (1, String))),
            )
            .unwrap();
        assert_eq!(typed_tuple, (2, "Bob".to_string()));
    }
}
