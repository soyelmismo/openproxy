//! Row mapping and query projections for combos.

use openproxy_types::ProviderId;
use openproxy_types::combos::{Combo, ComboTarget, ComboTargetWithModel, PriorityMode, Strategy};
use openproxy_types::config::CooldownMode;
use openproxy_types::ids::{AccountId, ComboId, ComboTargetId, ModelRowId};
use rusqlite::Row;

crate::def_table_select!(
    combo_select,
    "combos",
    "id, name, strategy, race_size, created_at, context_window, \
     priority_mode, cooldown_mode, cooldown_base_secs, cooldown_max_secs, \
     cooldown_factor, lkgp_exploration_rate, selection_window_secs, \
     COALESCE(preventive_rate_limit, 0)"
);
pub(crate) use combo_select;

crate::def_table_select!(
    combo_target_select,
    "combo_targets ct INNER JOIN providers p ON p.id = ct.provider_id",
    "ct.id, ct.combo_id, ct.provider_id, ct.account_id, ct.model_row_id, \
     ct.sub_combo_id, ct.priority_order, ct.weight, p.rate_limit_scope, ct.active, \
     ct.cooldown_mode, ct.cooldown_base_secs, ct.cooldown_max_secs, ct.cooldown_factor, \
     ct.thinking_effort"
);
pub(crate) use combo_target_select;

crate::def_table_select!(
    combo_target_with_model_select,
    "combo_targets ct \
     LEFT JOIN providers p ON p.id = ct.provider_id \
     LEFT JOIN models m ON m.id = ct.model_row_id \
     LEFT JOIN combos sc ON sc.id = ct.sub_combo_id \
     LEFT JOIN target_cooldowns tc ON tc.combo_target_id = ct.id \
     LEFT JOIN ( \
         SELECT ct2.model_row_id, MAX(tc2.cooldown_until) as model_cooldown_until, MAX(tc2.reason) as model_cooldown_reason \
         FROM target_cooldowns tc2 \
         INNER JOIN combo_targets ct2 ON ct2.id = tc2.combo_target_id \
         WHERE ct2.model_row_id IS NOT NULL \
           AND datetime(tc2.cooldown_until) > datetime('now') \
         GROUP BY ct2.model_row_id \
     ) mc ON mc.model_row_id = ct.model_row_id",
    "ct.id, ct.combo_id, ct.provider_id, ct.account_id, ct.model_row_id, \
     ct.sub_combo_id, sc.name as sub_combo_name, \
     COALESCE(m.model_id, ''), m.display_name, ct.priority_order, \
     COALESCE(tc.cooldown_until, mc.model_cooldown_until), \
     CASE WHEN (tc.cooldown_until IS NOT NULL AND datetime(tc.cooldown_until) > datetime('now')) \
               OR mc.model_cooldown_until IS NOT NULL \
          THEN 1 ELSE 0 END as in_cooldown, \
     COALESCE(tc.reason, mc.model_cooldown_reason), \
     m.context_length, \
     m.max_output_tokens, \
     ct.weight, \
     COALESCE(p.active, 0) as provider_active, \
     ct.active, \
     ct.cooldown_mode, \
     ct.cooldown_base_secs, \
     ct.cooldown_max_secs, \
     ct.cooldown_factor, \
     ct.thinking_effort"
);
pub(crate) use combo_target_with_model_select;

crate::def_table_select!(model_provider_id_select, "models", "provider_id");
pub(crate) use model_provider_id_select;

crate::def_table_select!(model_upstream_id_select, "models", "model_id");
pub(crate) use model_upstream_id_select;

crate::def_table_select!(model_context_length_select, "models", "context_length");
pub(crate) use model_context_length_select;

crate::def_table_select!(combo_context_window_select, "combos", "context_window");
pub(crate) use combo_context_window_select;

crate::def_table_select!(combo_target_ids_select, "combo_targets", "id");
pub(crate) use combo_target_ids_select;

crate::def_table_select!(
    combo_target_model_sub_select,
    "combo_targets ct",
    "ct.model_row_id, ct.sub_combo_id"
);
pub(crate) use combo_target_model_sub_select;

crate::def_table_select!(account_healthy_ids_select, "accounts", "id");
pub(crate) use account_healthy_ids_select;

pub(crate) fn row_to_combo(row: &Row<'_>) -> rusqlite::Result<Combo> {
    let race_size: u8 = crate::map_row_fields!(row, @u8(3));
    if !(1..=8).contains(&race_size) {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            3,
            rusqlite::types::Type::Integer,
            Box::from(format!("race_size out of range: {race_size}")),
        ));
    }

    crate::map_row_struct!(row, Combo {
        id: @id(0, ComboId),
        name: 1,
        strategy: @enum_parse(2, Strategy),
        race_size: @expr(race_size),
        created_at: 4,
        context_window: 5,
        priority_mode: @enum_or_default(6, PriorityMode),
        cooldown_mode: @enum_or_default(7, CooldownMode),
        cooldown_base_secs: @opt_u64(8),
        cooldown_max_secs: @opt_u64(9),
        cooldown_factor: @opt_u32(10),
        lkgp_exploration_rate: 11,
        selection_window_secs: @opt_u64(12),
        preventive_rate_limit: @bool(13),
    })
}

pub(crate) fn row_to_target(row: &Row<'_>) -> rusqlite::Result<ComboTarget> {
    crate::map_row_struct!(row, ComboTarget {
        id: @id(0, ComboTargetId),
        combo_id: @id(1, ComboId),
        provider_id: @id_str(2, ProviderId),
        account_id: @opt_id(3, AccountId),
        model_row_id: @opt_id(4, ModelRowId),
        sub_combo_id: @opt_id(5, ComboId),
        priority_order: 6,
        weight: @opt_default(7, i32, 1),
        rate_limit_scope: @enum_parse(8, openproxy_types::RateLimitScope),
        active: @opt_default(9, bool, true),
        cooldown_mode: @opt_enum_parse(10, CooldownMode),
        cooldown_base_secs: @opt_u64(11),
        cooldown_max_secs: @opt_u64(12),
        cooldown_factor: @opt_u32(13),
        thinking_effort: 14,
    })
}

pub(crate) fn row_to_target_with_model(row: &Row<'_>) -> rusqlite::Result<ComboTargetWithModel> {
    crate::map_row_struct!(row, ComboTargetWithModel {
        id: @id(0, ComboTargetId),
        combo_id: @id(1, ComboId),
        provider_id: @id_str(2, ProviderId),
        account_id: @opt_id(3, AccountId),
        model_row_id: @opt_id(4, ModelRowId),
        sub_combo_id: @opt_id(5, ComboId),
        sub_combo_name: @opt_box_str(6),
        model_id: @box_str(7),
        model_display_name: @opt_box_str(8),
        priority_order: 9,
        weight: @opt_default(15, i32, 1),
        in_cooldown: @bool(11),
        cooldown_until: @opt_box_str(10),
        cooldown_reason: @opt_box_str(12),
        context_length: 13,
        max_output_tokens: 14,
        active: @opt_default(17, bool, true),
        provider_active: @bool(16),
        cooldown_mode: @opt_enum_parse(18, CooldownMode),
        cooldown_base_secs: @opt_u64(19),
        cooldown_max_secs: @opt_u64(20),
        cooldown_factor: @opt_u32(21),
        thinking_effort: @opt_box_str(22),
    })
}

impl crate::crud::FromRow for Combo {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        row_to_combo(row)
    }
}

impl crate::crud::FromRow for ComboTarget {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        row_to_target(row)
    }
}

impl crate::crud::FromRow for ComboTargetWithModel {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        row_to_target_with_model(row)
    }
}
