//! Combo CRUD operations.

use openproxy_types::combos::{Combo, Strategy};
use openproxy_types::error::{CoreError, Result};
use openproxy_types::ids::{ComboId, ComboTargetId};
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
    crate::db_delete_by_id!(conn, "combos", id.0, format!("delete combo {}", id.0))?;
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
        crate::crud::execute_check_affected(
            conn,
            "UPDATE combos SET race_size = ?1 WHERE id = ?2",
            params![i64::from(rs), id.0],
            format!("update race_size for combo {}", id.0),
            || CoreError::ComboNotFound(id.0),
        )?;
    }
    Ok(())
}

/// Update the routing `strategy` of a combo. The string is parsed via
/// [`Strategy::parse`] — the same validation the create path applies —
/// so an unknown value surfaces as [`CoreError::Validation`].
pub fn update_name(conn: &Connection, id: ComboId, name: &str) -> Result<()> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(CoreError::Validation("combo name cannot be empty".into()));
    }
    let res = conn.execute(
        "UPDATE combos SET name = ?1 WHERE id = ?2",
        params![trimmed, id.0],
    );
    match res {
        Ok(0) => Err(CoreError::ComboNotFound(id.0)),
        Ok(_) => Ok(()),
        Err(e) => {
            if crate::error::classify_sqlite_error(&e) == crate::error::DbErrorKind::UniqueViolation
            {
                Err(CoreError::Validation(format!(
                    "combo name already exists: {trimmed}"
                )))
            } else {
                Err(crate::error::map_db_error_ctx(format!(
                    "update name for combo {}",
                    id.0
                ))(e))
            }
        }
    }
}

pub fn update_strategy(conn: &Connection, id: ComboId, strategy: &str) -> Result<()> {
    let parsed = Strategy::parse(strategy).map_err(CoreError::Validation)?;
    crate::crud::execute_check_affected(
        conn,
        "UPDATE combos SET strategy = ?1 WHERE id = ?2",
        params![parsed.as_str(), id.0],
        format!("update strategy for combo {}", id.0),
        || CoreError::ComboNotFound(id.0),
    )
}

pub fn clear_targets(conn: &Connection, combo_id: ComboId) -> Result<()> {
    conn.execute(
        "DELETE FROM combo_targets WHERE combo_id = ?1",
        params![combo_id.0],
    )
    .map_err(|e| {
        crate::error::map_db_error_ctx(format!("clear combo_targets for combo {}", combo_id.0))(e)
    })?;
    Ok(())
}

/// Clone an existing combo with all its settings, targets and system prefixes.
/// Appends `-copy` to the combo name (or uses the provided new name).
pub fn clone_combo(
    conn: &mut Connection,
    source_combo_id: ComboId,
    override_name: Option<&str>,
) -> Result<ComboId> {
    let source_combo = get_combo(conn, source_combo_id)?
        .ok_or_else(|| CoreError::ComboNotFound(source_combo_id.0))?;

    let new_name = match override_name {
        Some(n) if !n.trim().is_empty() => n.trim().to_string(),
        _ => format!("{}-copy", source_combo.name),
    };

    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(crate::error::map_db_error)?;

    let result = tx.execute(
        "INSERT INTO combos(
            name, strategy, race_size, context_window, max_output_tokens,
            priority_mode, cooldown_mode, cooldown_base_secs, cooldown_max_secs,
            cooldown_factor, lkgp_exploration_rate, selection_window_secs,
            preventive_rate_limit, decision_model, decision_timeout_ms,
            modality_gate, sanitize_response, real_model_id
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
        params![
            new_name,
            source_combo.strategy.as_str(),
            i64::from(source_combo.race_size),
            source_combo.context_window,
            source_combo.max_output_tokens,
            source_combo.priority_mode.as_str(),
            source_combo.cooldown_mode.as_str(),
            source_combo.cooldown_base_secs.map(|v| v as i64),
            source_combo.cooldown_max_secs.map(|v| v as i64),
            source_combo.cooldown_factor.map(i64::from),
            source_combo.lkgp_exploration_rate,
            source_combo.selection_window_secs.map(|v| v as i64),
            i64::from(source_combo.preventive_rate_limit),
            source_combo.decision_model,
            source_combo.decision_timeout_ms.map(|v| v as i64),
            source_combo.modality_gate.map(i64::from),
            source_combo.sanitize_response.map(i64::from),
            source_combo.real_model_id,
        ],
    );

    match result {
        Ok(_) => {}
        Err(e) => {
            if crate::error::classify_sqlite_error(&e) == crate::error::DbErrorKind::UniqueViolation
            {
                return Err(CoreError::Validation(format!(
                    "combo name already exists: {new_name}"
                )));
            }
            return Err(crate::error::map_db_error_ctx(format!(
                "insert cloned combo {new_name}"
            ))(e));
        }
    }

    let new_combo_id = ComboId(tx.last_insert_rowid());

    // Copy targets
    let targets = super::targets::list_targets(&tx, source_combo_id)?;
    for t in targets {
        let old_target_id = t.id;
        let selected_accounts_json = t
            .selected_accounts
            .as_ref()
            .and_then(|a| serde_json::to_string(a).ok());

        tx.execute(
            "INSERT INTO combo_targets(
                combo_id, provider_id, account_id, model_row_id, sub_combo_id,
                priority_order, weight, active, cooldown_mode, cooldown_base_secs,
                cooldown_max_secs, cooldown_factor, thinking_effort, description, selected_accounts
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                new_combo_id.0,
                t.provider_id.as_str(),
                t.account_id.map(|a| a.0),
                t.model_row_id.map(|m| m.0),
                t.sub_combo_id.map(|c| c.0),
                t.priority_order,
                t.weight,
                i32::from(t.active),
                t.cooldown_mode.map(|c| c.as_str().to_string()),
                t.cooldown_base_secs.map(|v| v as i64),
                t.cooldown_max_secs.map(|v| v as i64),
                t.cooldown_factor.map(i64::from),
                t.thinking_effort,
                t.description,
                selected_accounts_json,
            ],
        )
        .map_err(crate::error::map_db_error_ctx("clone combo_target"))?;

        let new_target_id = ComboTargetId(tx.last_insert_rowid());

        // Copy prefixes for this target
        let prefixes = super::targets::list_target_system_prefixes(&tx, old_target_id)?;
        for p in prefixes {
            tx.execute(
                "INSERT INTO combo_target_system_prefixes (
                    target_id, name, criteria, system_text, active, priority_order
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    new_target_id.0,
                    p.name,
                    p.criteria,
                    p.system_text,
                    i32::from(p.active),
                    p.priority_order,
                ],
            )
            .map_err(crate::error::map_db_error_ctx(
                "clone combo_target_system_prefix",
            ))?;
        }
    }

    tx.commit().map_err(crate::error::map_db_error)?;
    Ok(new_combo_id)
}
