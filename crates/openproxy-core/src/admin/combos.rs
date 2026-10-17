//! Combo administration service layer.

use crate::error::{CoreError, Result};
use crate::ids::{AccountId, ComboId, ComboTargetId, ModelRowId, ProviderId};
use openproxy_db::combos;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

/// Inputs for [`create_combo`].
///
/// The priority/cooldown/LKGP/selection-window fields are optional (migration
/// 000035). `None` means `Strict` priority, `Flat` cooldown and the global
/// `[cooldown]` numbers. A supplied `priority_mode` / `cooldown_mode` goes
/// through `combos::PriorityMode::parse` / `combos::CooldownMode::parse`, and an
/// unknown value surfaces as [`CoreError::Validation`] (HTTP 400).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateComboInput {
    pub name: String,
    pub strategy: String,
    pub race_size: Option<u8>,
    /// Priority mode for `Strategy::Priority`. `None` = `strict`. Ignored for
    /// `RoundRobin` / `Shuffle`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority_mode: Option<String>,
    /// Cooldown growth mode. `None` = `flat`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_mode: Option<String>,
    /// Per-combo cooldown base (seconds). `None` = global `[cooldown]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_base_secs: Option<u64>,
    /// Per-combo cooldown cap (seconds). `None` = global `[cooldown] max_secs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_max_secs: Option<u64>,
    /// Per-combo exponential growth factor. `None` = global `[cooldown] factor`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_factor: Option<u32>,
    /// LKGP exploration rate (0.0–1.0). `None` = default 0.1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lkgp_exploration_rate: Option<f64>,
    /// Selection window (seconds) for `least_used` / `p2c`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_window_secs: Option<u64>,
    /// Decision routing model for `priority_mode = "decision"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_model: Option<String>,
    /// Decision routing timeout in milliseconds. Default: 100ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_timeout_ms: Option<u64>,
}

impl CreateComboInput {
    fn has_cooldown_settings(&self) -> bool {
        self.cooldown_mode.is_some()
            || self.cooldown_base_secs.is_some()
            || self.cooldown_max_secs.is_some()
            || self.cooldown_factor.is_some()
    }
}

/// Add a target to a combo: the historical flat-target wire shape plus
/// `sub_combo_id` for combo-in-combo targets. Exactly one of `model_row_id` /
/// `sub_combo_id` must be `Some`; [`combos::add_target`] enforces the XOR because
/// SQLite cannot add a CHECK constraint to a populated table.
///
/// `provider_id` is ignored for sub-combo targets: the stored row references the
/// virtual `"combo"` provider and routing goes through the sub-combo's children.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddTargetInput {
    pub provider_id: String,
    pub account_id: Option<AccountId>,
    pub model_row_id: Option<ModelRowId>,
    pub sub_combo_id: Option<ComboId>,
    pub priority_order: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_accounts: Option<Vec<AccountId>>,
}

fn apply_combo_overrides(
    conn: &Connection,
    combo_id: ComboId,
    input: &CreateComboInput,
) -> Result<()> {
    if input.priority_mode.is_some() {
        combos::update_priority_mode(conn, combo_id, input.priority_mode.as_deref())?;
    }
    if input.has_cooldown_settings() {
        combos::update_cooldown_settings(
            conn,
            combo_id,
            input.cooldown_mode.as_deref(),
            input.cooldown_base_secs,
            input.cooldown_max_secs,
            input.cooldown_factor,
        )?;
    }
    if input.lkgp_exploration_rate.is_some() {
        combos::update_lkgp_settings(conn, combo_id, input.lkgp_exploration_rate)?;
    }
    if input.selection_window_secs.is_some() {
        combos::update_selection_window(conn, combo_id, input.selection_window_secs)?;
    }
    if input.decision_model.is_some() {
        combos::update_decision_model(conn, combo_id, input.decision_model.as_deref())?;
    }
    if input.decision_timeout_ms.is_some() {
        combos::update_decision_timeout(conn, combo_id, input.decision_timeout_ms)?;
    }
    Ok(())
}

pub fn create_combo(conn: &Connection, input: &CreateComboInput) -> Result<ComboId> {
    let strategy =
        openproxy_types::combos::Strategy::parse(&input.strategy).map_err(CoreError::Validation)?;
    // 1 is the serial race window. `Strategy::Priority` ignores `race_size`
    // entirely, so this only matters for RoundRobin / Shuffle.
    let race_size = input.race_size.unwrap_or(1);
    let combo_id = combos::create_combo(conn, &input.name, strategy, race_size)?;

    // Each override helper validates its own inputs and writes one UPDATE. A
    // validation failure leaves the combo created and surfaces the error, so the
    // operator can fix the input and re-POST or PATCH.
    apply_combo_overrides(conn, combo_id, input)?;
    Ok(combo_id)
}

/// Every combo.
pub fn list_combos(conn: &Connection) -> Result<Vec<openproxy_types::combos::Combo>> {
    combos::list_combos(conn)
}

/// Id + name only, for the "add sub-combo target" picker.
#[derive(Debug, Clone, Serialize)]
pub struct ComboSummary {
    pub id: i64,
    pub name: String,
}

/// Combos that are valid sub-combo targets of `combo_id`.
///
/// A combo is excluded when it is `combo_id` itself (no self-loop) or would close
/// a cycle. The cycle probe is [`combos::combo_in_chain`] with the same depth cap
/// as [`combos::add_target`], so the picker never offers a choice the API would
/// later reject. Results are id-ascending for a stable list.
pub fn list_valid_sub_combos(conn: &Connection, combo_id: ComboId) -> Result<Vec<ComboSummary>> {
    let all = combos::list_combos(conn)?;
    let mut out = Vec::with_capacity(all.len());
    for c in all {
        if c.id == combo_id {
            continue;
        }
        // adding `c` closes a cycle iff `combo_id` is already reachable from `c`
        if combos::combo_in_chain(
            conn,
            combo_id,
            c.id,
            openproxy_types::combos::MAX_SUB_COMBO_DEPTH,
        )? {
            continue;
        }
        out.push(ComboSummary {
            id: c.id.0,
            name: c.name,
        });
    }
    Ok(out)
}

/// Add a target to an existing combo, returning the new target id.
///
/// Existence checks are delegated to [`combos::add_target`], which also rejects
/// self-loops and would-be cycles via [`combos::combo_in_chain`].
pub fn add_target_to_combo(
    conn: &Connection,
    combo_id: ComboId,
    input: AddTargetInput,
) -> Result<ComboTargetId> {
    let provider = ProviderId::new(input.provider_id);
    combos::add_target(
        conn,
        combos::AddTargetInput {
            combo_id,
            provider_id: provider,
            account_id: input.account_id,
            model_row_id: input.model_row_id,
            sub_combo_id: input.sub_combo_id,
            priority_order: input.priority_order,
            description: input.description,
            selected_accounts: input.selected_accounts,
        },
    )
}

/// Targets of a combo, ordered by `(priority_order ASC, id ASC)`.
pub fn list_combo_targets(
    conn: &Connection,
    combo_id: ComboId,
) -> Result<Vec<openproxy_types::combos::ComboTarget>> {
    combos::list_targets(conn, combo_id)
}

/// Targets enriched with the model display name, so the dashboard needs no
/// per-row `GET /admin/models` roundtrip. See [`combos::list_targets_with_model`].
pub fn list_combo_targets_with_model(
    conn: &Connection,
    combo_id: ComboId,
) -> Result<Vec<openproxy_types::combos::ComboTargetWithModel>> {
    combos::list_targets_with_model(conn, combo_id)
}

/// Delete a combo by id. Idempotent, and the FK cascade removes its targets.
pub fn delete_combo(conn: &Connection, id: ComboId) -> Result<()> {
    combos::delete_combo(conn, id)
}

/// Reject a `DELETE /admin/combos/9999/targets/1` whose target exists in another
/// combo. The target id alone is unique, so this is a URL-shape check.
///
/// A mismatch surfaces as [`CoreError::Validation`] because [`CoreError`] has no
/// dedicated variant, and the server maps that to HTTP 400.
fn ensure_target_in_combo(
    conn: &Connection,
    combo_id: ComboId,
    target_id: ComboTargetId,
) -> Result<()> {
    let belongs = combos::target_belongs_to_combo(conn, combo_id, target_id)?;
    if !belongs {
        return Err(CoreError::Validation(format!(
            "target {} not in combo {}",
            target_id.0, combo_id.0
        )));
    }
    Ok(())
}

pub fn delete_combo_target(
    conn: &Connection,
    combo_id: ComboId,
    target_id: ComboTargetId,
) -> Result<()> {
    ensure_target_in_combo(conn, combo_id, target_id)?;
    combos::delete_target(conn, target_id)
}

/// Renumber every `priority_order` of `combo_id` in one transaction to match
/// `ordered_ids`, so two targets can never briefly share a `priority_order`.
///
/// Rejected with [`CoreError::Validation`] when `ordered_ids` is not a
/// permutation of the combo's target ids.
///
/// Takes `&mut Connection` because [`combos::reorder_targets`] opens an
/// `IMMEDIATE` transaction; the handler passes the writer guard's `&mut`.
pub fn reorder_combo_targets(
    conn: &mut Connection,
    combo_id: ComboId,
    ordered_ids: &[ComboTargetId],
) -> Result<()> {
    combos::reorder_targets(conn, combo_id, ordered_ids)
}

/// Force-clear a target's cooldown, backing the dashboard's "Reset cooldown"
/// button: an operator who diagnosed the upstream issue clears a parked target
/// without waiting out `cooldown_secs`.
///
/// Cross-combo combinations surface as [`CoreError::Validation`].
pub fn clear_combo_target_cooldown(
    conn: &Connection,
    combo_id: ComboId,
    target_id: ComboTargetId,
) -> Result<()> {
    // deleting the target would also clear its cooldown through the cascade FK
    // on `target_cooldowns.combo_target_id`, but the intent here is to keep the
    // target, so clear the cooldown explicitly
    ensure_target_in_combo(conn, combo_id, target_id)?;
    openproxy_db::cooldowns::clear_cooldown(conn, target_id)
}
