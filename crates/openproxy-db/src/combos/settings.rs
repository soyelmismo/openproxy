//! Settings and column update operations for combos and targets.

use openproxy_types::combos::PriorityMode;
use openproxy_types::config::CooldownMode;
use openproxy_types::error::{CoreError, Result};
use openproxy_types::ids::{ComboId, ComboTargetId};
use rusqlite::{Connection, params};

define_column_updaters! {
    table: "combo_targets",
    id_type: ComboTargetId,
    id_field: |id| id.0,
    pub fn update_target_priority(new_order: i32 => new_order as i64) => "priority_order";
    pub(crate) fn raw_update_target_weight(weight: i64) => "weight";
    pub fn update_target_active(active: bool => i64::from(active)) => "active";
    pub(crate) fn raw_update_target_cooldown_mode(mode: Option<&str>) => "cooldown_mode";
    pub(crate) fn raw_update_target_thinking_effort(effort: Option<&str>) => "thinking_effort";
    pub fn update_target_cooldown_base(base: Option<u64> => base.map(|v| v as i64)) => "cooldown_base_secs";
    pub fn update_target_cooldown_max(max: Option<u64> => max.map(|v| v as i64)) => "cooldown_max_secs";
    pub fn update_target_cooldown_factor(factor: Option<u32> => factor.map(i64::from)) => "cooldown_factor";
}

define_column_updaters! {
    table: "combos",
    id_type: ComboId,
    id_field: |id| id.0,
    not_found: |id| CoreError::ComboNotFound(id.0),
    pub fn update_context_window(context_window: Option<i64>) => "context_window";
    pub(crate) fn raw_update_combo_priority_mode(mode: Option<&str>) => "priority_mode";
    pub(crate) fn raw_update_combo_cooldown_mode(mode: Option<&str>) => "cooldown_mode";
    pub fn update_cooldown_base(base: Option<u64> => base.map(|v| v as i64)) => "cooldown_base_secs";
    pub fn update_cooldown_max(max: Option<u64> => max.map(|v| v as i64)) => "cooldown_max_secs";
    pub fn update_cooldown_factor(factor: Option<u32> => factor.map(i64::from)) => "cooldown_factor";
    pub fn update_preventive_rate_limit(enabled: bool => i64::from(enabled)) => "preventive_rate_limit";
    pub(crate) fn raw_update_combo_lkgp_rate(rate: Option<f64>) => "lkgp_exploration_rate";
    pub(crate) fn raw_update_combo_selection_window(window: Option<i64>) => "selection_window_secs";
}

pub fn update_target_weight(
    conn: &Connection,
    target_id: ComboTargetId,
    weight: i32,
) -> Result<()> {
    if weight <= 0 {
        return Err(CoreError::Validation(format!(
            "combo_target weight must be positive, got {weight}"
        )));
    }
    raw_update_target_weight(conn, target_id, weight as i64)
}

pub fn update_target_cooldown_mode(
    conn: &Connection,
    target_id: ComboTargetId,
    mode: Option<&str>,
) -> Result<()> {
    let mode_str = match mode {
        None => None,
        Some(s) => Some(
            CooldownMode::parse(s)
                .map_err(CoreError::Validation)?
                .as_str(),
        ),
    };
    raw_update_target_cooldown_mode(conn, target_id, mode_str)
}

pub fn update_target_thinking_effort(
    conn: &Connection,
    target_id: ComboTargetId,
    effort: Option<&str>,
) -> Result<()> {
    let effort_str = match effort {
        Some(s) if !s.trim().is_empty() && s != "passthrough" => Some(s.trim()),
        _ => None,
    };
    raw_update_target_thinking_effort(conn, target_id, effort_str)
}

pub fn update_priority_mode(conn: &Connection, id: ComboId, mode: Option<&str>) -> Result<()> {
    let value: Option<&str> = match mode {
        None => None,
        Some(s) => {
            let parsed = PriorityMode::parse(s).map_err(CoreError::Validation)?;
            Some(parsed.as_str())
        }
    };
    raw_update_combo_priority_mode(conn, id, value)
}

pub fn update_cooldown_settings(
    conn: &Connection,
    id: ComboId,
    mode: Option<&str>,
    base: Option<u64>,
    max: Option<u64>,
    factor: Option<u32>,
) -> Result<()> {
    let mode_value: Option<&str> = match mode {
        None => None,
        Some(s) => {
            let parsed = CooldownMode::parse(s).map_err(CoreError::Validation)?;
            Some(parsed.as_str())
        }
    };
    let affected = conn
        .execute(
            "UPDATE combos SET cooldown_mode = ?1, \
                                cooldown_base_secs = ?2, \
                                cooldown_max_secs = ?3, \
                                cooldown_factor = ?4 \
             WHERE id = ?5",
            params![
                mode_value,
                base.map(|v| v as i64),
                max.map(|v| v as i64),
                factor.map(i64::from),
                id.0
            ],
        )
        .map_err(crate::error::map_db_error_ctx(format!(
            "update cooldown settings for combo {}",
            id.0
        )))?;
    if affected == 0 {
        return Err(CoreError::ComboNotFound(id.0));
    }
    Ok(())
}

pub fn update_cooldown_mode(conn: &Connection, id: ComboId, mode: Option<&str>) -> Result<()> {
    let mode_value: Option<&str> = match mode {
        None => None,
        Some(s) => {
            let parsed = CooldownMode::parse(s).map_err(CoreError::Validation)?;
            Some(parsed.as_str())
        }
    };
    raw_update_combo_cooldown_mode(conn, id, mode_value)
}

pub fn update_lkgp_settings(
    conn: &Connection,
    id: ComboId,
    exploration_rate: Option<f64>,
) -> Result<()> {
    if let Some(rate) = exploration_rate
        && !(0.0..=1.0).contains(&rate)
    {
        return Err(CoreError::Validation(format!(
            "lkgp_exploration_rate must be in [0.0, 1.0], got {rate}"
        )));
    }
    raw_update_combo_lkgp_rate(conn, id, exploration_rate)
}

pub fn update_selection_window(
    conn: &Connection,
    id: ComboId,
    window_secs: Option<u64>,
) -> Result<()> {
    raw_update_combo_selection_window(conn, id, window_secs.map(|v| v as i64))
}
