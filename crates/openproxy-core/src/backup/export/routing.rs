//! Combo and target routing backup export helpers.

use super::parse_backup_enum;
use openproxy_db::error::map_db_error;
use openproxy_types::backup::{BackupCombo, BackupComboTarget};
use openproxy_types::combos::{CooldownMode, PriorityMode, Strategy};
use openproxy_types::{ProviderId, RateLimitScope, Result};
use rusqlite::Connection;

pub(super) fn export_combos(conn: &Connection) -> Result<Vec<BackupCombo>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, name, strategy, race_size, preventive_rate_limit, \
             context_window, priority_mode, cooldown_mode, cooldown_base_secs, \
             cooldown_max_secs, cooldown_factor, lkgp_exploration_rate, \
             selection_window_secs, decision_model, decision_timeout_ms \
             FROM combos ORDER BY id",
        )
        .map_err(map_db_error)?;

    let combos = stmt
        .query_map([], |row| {
            let strat_str: String = row.get(2)?;
            let strategy = parse_backup_enum(&strat_str, 2, Strategy::parse)?;

            let prio_mode_str: Option<String> = row.get(6)?;
            let priority_mode = prio_mode_str
                .as_deref()
                .and_then(|s| PriorityMode::parse(s).ok())
                .unwrap_or_default();

            let cd_mode_str: Option<String> = row.get(7)?;
            let cooldown_mode = cd_mode_str
                .as_deref()
                .and_then(|s| CooldownMode::parse(s).ok())
                .unwrap_or_default();

            Ok(BackupCombo {
                id: row.get(0)?,
                name: row.get(1)?,
                strategy,
                race_size: row.get(3)?,
                preventive_rate_limit: row.get::<_, i64>(4)? != 0,
                context_window: row.get(5)?,
                priority_mode,
                cooldown_mode,
                cooldown_base_secs: row.get::<_, Option<i64>>(8)?.map(|v| v as u64),
                cooldown_max_secs: row.get::<_, Option<i64>>(9)?.map(|v| v as u64),
                cooldown_factor: row.get(10)?,
                lkgp_exploration_rate: row.get(11)?,
                selection_window_secs: row.get::<_, Option<i64>>(12)?.map(|v| v as u64),
                decision_model: row.get(13)?,
                decision_timeout_ms: row.get::<_, Option<i64>>(14)?.map(|v| v as u64),
            })
        })
        .map_err(map_db_error)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(map_db_error)?;

    Ok(combos)
}

pub(super) fn export_combo_targets(conn: &Connection) -> Result<Vec<BackupComboTarget>> {
    let mut stmt = conn
        .prepare(
            "SELECT ct.id, ct.combo_id, ct.provider_id, ct.account_id, ct.model_row_id, \
             ct.sub_combo_id, ct.upstream_model_id, ct.priority_order, ct.weight, \
             ct.active, p.rate_limit_scope, ct.cooldown_mode, ct.cooldown_base_secs, \
             ct.cooldown_max_secs, ct.cooldown_factor, ct.thinking_effort, ct.description \
             FROM combo_targets ct \
             LEFT JOIN providers p ON p.id = ct.provider_id \
             ORDER BY ct.combo_id, ct.priority_order",
        )
        .map_err(map_db_error)?;

    let combo_targets = stmt
        .query_map([], |row| {
            let scope_str: Option<String> = row.get(10)?;
            let rate_limit_scope = scope_str
                .as_deref()
                .and_then(|s| RateLimitScope::parse(s).ok())
                .unwrap_or(RateLimitScope::Account);

            let cd_str: Option<String> = row.get(11)?;
            let cooldown_mode = cd_str.as_deref().and_then(|s| CooldownMode::parse(s).ok());

            Ok(BackupComboTarget {
                id: row.get(0)?,
                combo_id: row.get(1)?,
                provider_id: ProviderId::new(row.get::<_, String>(2)?),
                account_id: row.get(3)?,
                model_row_id: row.get(4)?,
                sub_combo_id: row.get(5)?,
                upstream_model_id: row.get(6)?,
                priority_order: row.get(7)?,
                weight: row.get(8)?,
                active: row.get::<_, i64>(9)? != 0,
                rate_limit_scope,
                cooldown_mode,
                cooldown_base_secs: row.get::<_, Option<i64>>(12)?.map(|v| v as u64),
                cooldown_max_secs: row.get::<_, Option<i64>>(13)?.map(|v| v as u64),
                cooldown_factor: row.get(14)?,
                thinking_effort: row.get(15)?,
                description: row.get(16)?,
            })
        })
        .map_err(map_db_error)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(map_db_error)?;

    Ok(combo_targets)
}
