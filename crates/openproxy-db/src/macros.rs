//! Declarative macros for database repository operations.

/// Generates single-column update functions for a database table.
///
/// Supports optional value transformation and optional `not_found` validation
/// when `affected == 0`.
#[macro_export]
macro_rules! define_column_updaters {
    // Pattern with not_found check (returns error if affected rows == 0)
    (
        table: $table:expr,
        id_type: $id_type:ty,
        id_field: |$id:ident| $id_val:expr,
        not_found: |$nf_id:ident| $nf_err:expr,
        $(
            $(#[$meta:meta])*
            $vis:vis fn $fn_name:ident ( $val_ident:ident : $val_type:ty $(=> $transform:expr)? ) => $col:expr;
        )*
    ) => {
        $(
            $(#[$meta])*
            $vis fn $fn_name(conn: &rusqlite::Connection, id: $id_type, $val_ident: $val_type) -> openproxy_types::Result<()> {
                let val = {
                    $( let $val_ident = $transform; )?
                    $val_ident
                };
                let id_raw = { let $id = id; $id_val };
                let sql = concat!("UPDATE ", $table, " SET ", $col, " = ?1 WHERE id = ?2");
                let affected = conn.execute(sql, rusqlite::params![val, id_raw])
                    .map_err($crate::error::map_db_error_ctx(format!(
                        "update {} for {} {}",
                        $col, $table, id_raw
                    )))?;
                if affected == 0 {
                    return Err({ let $nf_id = id; $nf_err });
                }
                Ok(())
            }
        )*
    };
    // Pattern without not_found check (e.g. combo_targets legacy semantics)
    (
        table: $table:expr,
        id_type: $id_type:ty,
        id_field: |$id:ident| $id_val:expr,
        $(
            $(#[$meta:meta])*
            $vis:vis fn $fn_name:ident ( $val_ident:ident : $val_type:ty $(=> $transform:expr)? ) => $col:expr;
        )*
    ) => {
        $(
            $(#[$meta])*
            $vis fn $fn_name(conn: &rusqlite::Connection, id: $id_type, $val_ident: $val_type) -> openproxy_types::Result<()> {
                let val = {
                    $( let $val_ident = $transform; )?
                    $val_ident
                };
                let id_raw = { let $id = id; $id_val };
                let sql = concat!("UPDATE ", $table, " SET ", $col, " = ?1 WHERE id = ?2");
                conn.execute(sql, rusqlite::params![val, id_raw])
                    .map_err($crate::error::map_db_error_ctx(format!(
                        "update {} for {} {}",
                        $col, $table, id_raw
                    )))?;
                Ok(())
            }
        )*
    };
}

#[cfg(test)]
mod tests {
    use crate::DbPool;
    use openproxy_types::CoreError;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct DummyId(pub i64);

    define_column_updaters! {
        table: "combos",
        id_type: DummyId,
        id_field: |id| id.0,
        not_found: |id| CoreError::ComboNotFound(id.0),
        pub fn test_update_cooldown_base(base: Option<u64> => base.map(|v| v as i64)) => "cooldown_base_secs";
    }

    define_column_updaters! {
        table: "combo_targets",
        id_type: DummyId,
        id_field: |id| id.0,
        pub fn test_update_target_priority(priority: u32 => priority as i64) => "priority_order";
    }

    #[test]
    fn test_macro_updater_not_found() {
        let pool = DbPool::test_pool().expect("pool");
        let conn = pool.writer();
        let err = test_update_cooldown_base(&conn, DummyId(999999), Some(10)).unwrap_err();
        match err {
            CoreError::ComboNotFound(id) => assert_eq!(id, 999999),
            other => panic!("expected ComboNotFound, got {other:?}"),
        }
        let res = test_update_target_priority(&conn, DummyId(999999), 5);
        assert!(res.is_ok());
    }
}
