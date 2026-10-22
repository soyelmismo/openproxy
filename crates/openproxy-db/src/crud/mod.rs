//! Generic CRUD operations and helpers for `rusqlite` database access.

use openproxy_types::Result;
use rusqlite::{Connection, OptionalExtension, Params, Row};

use crate::error::map_db_error_ctx;

pub trait FromRow: Sized {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self>;
}

pub fn query_one<T: FromRow, P: Params>(
    conn: &Connection,
    sql: &str,
    params: P,
    ctx: impl AsRef<str>,
) -> Result<Option<T>> {
    query_one_with(conn, sql, params, T::from_row, ctx)
}

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

pub fn query_all<T: FromRow, P: Params>(
    conn: &Connection,
    sql: &str,
    params: P,
    ctx: impl AsRef<str>,
) -> Result<Vec<T>> {
    query_all_with(conn, sql, params, T::from_row, ctx)
}

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

pub fn execute<P: Params>(
    conn: &Connection,
    sql: &str,
    params: P,
    ctx: impl AsRef<str>,
) -> Result<usize> {
    conn.execute(sql, params)
        .map_err(map_db_error_ctx(ctx.as_ref()))
}

pub fn execute_check_affected<P: Params, E>(
    conn: &Connection,
    sql: &str,
    params: P,
    ctx: impl AsRef<str>,
    not_found: impl FnOnce() -> E,
) -> Result<()>
where
    E: Into<openproxy_types::CoreError>,
{
    let affected = execute(conn, sql, params, ctx)?;
    check_affected(affected, not_found)
}

pub fn check_affected<E>(affected: usize, not_found: impl FnOnce() -> E) -> Result<()>
where
    E: Into<openproxy_types::CoreError>,
{
    if affected == 0 {
        Err(not_found().into())
    } else {
        Ok(())
    }
}

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

#[macro_export]
macro_rules! db_execute {
    ($conn:expr, $sql:expr, $params:expr, $ctx:expr $(,)?) => {
        $crate::crud::execute($conn, $sql, $params, $ctx)
    };
}

#[macro_export]
macro_rules! db_delete_by_id {
    ($conn:expr, $table:literal, $id:expr, $ctx:expr $(,)?) => {
        $crate::crud::execute(
            $conn,
            concat!("DELETE FROM ", $table, " WHERE id = ?1"),
            ::rusqlite::params![$id],
            $ctx,
        )
    };
    ($conn:expr, $table:literal, WHERE $col:ident = $val:expr, $ctx:expr $(,)?) => {
        $crate::crud::execute(
            $conn,
            concat!("DELETE FROM ", $table, " WHERE ", stringify!($col), " = ?1"),
            ::rusqlite::params![$val],
            $ctx,
        )
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

/// Typed `rusqlite::Row` extraction. Qualifiers:
/// - `@bool(idx)`, `@opt_bool(idx)`, `@u8(idx)`, `@u16(idx)`, `@opt_u16(idx)`,
///   `@u32(idx)`, `@opt_u32(idx)`, `@u64(idx)`, `@opt_u64(idx)`
/// - `@box_str(idx)`, `@opt_box_str(idx)`: `String` as `Box<str>`
/// - `@id(idx, Type)`, `@opt_id(idx, Type)`: numeric id as `Type`
/// - `@id_str(idx, Type)`: string id as `Type::new(String)`
/// - `@enum_parse(idx, Type)`, `@opt_enum_parse(idx, Type)`: `FromStr` parse,
///   also spelled `@enum(idx, Type)` / `@opt_enum(idx, Type)`
/// - `@from_db(idx, Type)`: `Type::from_db(Option<&str>)`
/// - `@json(idx)`: JSON deserializable `T`
/// - `@opt_default(idx, [Type,] default)`: `Option<T>` or `default`
/// - `(idx, Type)`, `idx`
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

/// Maps a `rusqlite::Row` into a typed tuple. Fields are
/// `@qualifier(...)` (`@id(0, AccountId)`, `@enum_parse(1, Status)`),
/// `(idx, Type)` or a bare `idx`.
///
/// ```ignore
/// map_row_tuple!(row => (0, 1))
/// map_row_tuple!(row, ((0, i64), (1, String)))
/// map_row_tuple!(row => (@id(0, AccountId), 1, @bool(2)))
/// ```
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
mod tests;
