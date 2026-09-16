//! Combo CRUD operations.

use openproxy_types::combos::{Combo, Strategy};
use openproxy_types::error::{CoreError, Result};
use openproxy_types::ids::ComboId;
use rusqlite::{Connection, params};

use super::mapping::{combo_select, row_to_combo};

pub fn create_combo(
    conn: &Connection,
    name: &str,
    strategy: Strategy,
    race_size: u8,
) -> Result<ComboId> {
    // Validate race_size against the schema CHECK constraint (1..=8).
    if !(1..=8).contains(&race_size) {
        return Err(CoreError::Validation(format!(
            "race_size must be in 1..=8, got {race_size}"
        )));
    }

    let result = conn.execute(
        "INSERT INTO combos(name, strategy, race_size) VALUES (?1, ?2, ?3)",
        params![name, strategy.as_str(), i64::from(race_size)],
    );

    match result {
        Ok(_) => {}
        Err(e) => {
            if crate::error::classify_sqlite_error(&e) == crate::error::DbErrorKind::UniqueViolation
            {
                return Err(CoreError::Validation(format!(
                    "combo name already exists: {name}"
                )));
            }
            return Err(crate::error::map_db_error_ctx(format!(
                "insert combo {name}"
            ))(e));
        }
    }

    Ok(ComboId(conn.last_insert_rowid()))
}

pub fn get_combo(conn: &Connection, id: ComboId) -> Result<Option<Combo>> {
    crate::db_query_one!(
        conn,
        combo_select!("WHERE id = ?1"),
        params![id.0],
        row_to_combo,
        format!("get combo {}", id.0)
    )
}

pub fn list_combos(conn: &Connection) -> Result<Vec<Combo>> {
    crate::db_query_all!(
        conn,
        combo_select!("ORDER BY id"),
        [],
        row_to_combo,
        "list combos"
    )
}

/// Look up a combo by its exact (case-sensitive) name. Returns `Ok(None)`
/// when no row matches.
pub fn get_combo_by_name(conn: &Connection, name: &str) -> Result<Option<Combo>> {
    crate::db_query_one!(
        conn,
        combo_select!("WHERE name = ?1"),
        params![name],
        row_to_combo,
        "get combo by name"
    )
}

pub fn delete_combo(conn: &Connection, id: ComboId) -> Result<()> {
    crate::db_execute!(
        conn,
        "DELETE FROM combos WHERE id = ?1",
        params![id.0],
        format!("delete combo {}", id.0)
    )?;
    Ok(())
}

/// Update mutable fields of a combo. Currently only `race_size` is
/// supported; passing `None` leaves the existing value untouched. The
/// `1..=8` CHECK constraint from migration 000004 is enforced by SQLite.
pub fn update_combo(conn: &Connection, id: ComboId, race_size: Option<u8>) -> Result<()> {
    if let Some(rs) = race_size {
        if !(1..=8).contains(&rs) {
            return Err(CoreError::Validation(format!(
                "race_size must be in 1..=8, got {rs}"
            )));
        }
        let affected = conn
            .execute(
                "UPDATE combos SET race_size = ?1 WHERE id = ?2",
                params![i64::from(rs), id.0],
            )
            .map_err(crate::error::map_db_error_ctx(format!(
                "update race_size for combo {}",
                id.0
            )))?;
        if affected == 0 {
            return Err(CoreError::ComboNotFound(id.0));
        }
    }
    Ok(())
}

/// Update the routing `strategy` of a combo. The string is parsed via
/// [`Strategy::parse`] — the same validation the create path applies —
/// so an unknown value surfaces as [`CoreError::Validation`].
pub fn update_strategy(conn: &Connection, id: ComboId, strategy: &str) -> Result<()> {
    let parsed = Strategy::parse(strategy).map_err(CoreError::Validation)?;
    let affected = conn
        .execute(
            "UPDATE combos SET strategy = ?1 WHERE id = ?2",
            params![parsed.as_str(), id.0],
        )
        .map_err(crate::error::map_db_error_ctx(format!(
            "update strategy for combo {}",
            id.0
        )))?;
    if affected == 0 {
        return Err(CoreError::ComboNotFound(id.0));
    }
    Ok(())
}

pub fn clear_targets(conn: &Connection, combo_id: ComboId) -> Result<()> {
    conn.execute(
        "DELETE FROM combo_targets WHERE combo_id = ?1",
        params![combo_id.0],
    )
    .map_err(crate::error::map_db_error_ctx(format!(
        "clear combo_targets for combo {}",
        combo_id.0
    )))?;
    Ok(())
}
