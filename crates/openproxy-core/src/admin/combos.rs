//! Combo administration service layer.

use crate::error::{CoreError, Result};
use crate::ids::{AccountId, ComboId, ComboTargetId, ModelRowId, ProviderId};
use openproxy_db::combos;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

/// Inputs for [`create_combo`].
///
/// The `priority_mode` / `cooldown_mode` / per-combo cooldown
/// overrides / `lkgp_exploration_rate` / `selection_window_secs`
/// fields are all optional (migration 000035). `None` means "use
/// the legacy default" — `Strict` priority mode, `Flat` cooldown
/// mode, and the global `[cooldown]` config for the cooldown
/// numbers. A non-`None` `priority_mode` / `cooldown_mode` is
/// parsed and validated by [`combos::PriorityMode::parse`] /
/// [`combos::CooldownMode::parse`]; an unknown value surfaces as
/// [`CoreError::Validation`] (HTTP 400).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateComboInput {
    pub name: String,
    pub strategy: String,
    pub race_size: Option<u8>,
    /// Priority mode for `Strategy::Priority`. `None` = `strict`
    /// (the legacy walk). Ignored for `RoundRobin` / `Shuffle`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority_mode: Option<String>,
    /// Cooldown growth mode. `None` = `flat` (the legacy behavior).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_mode: Option<String>,
    /// Per-combo cooldown base (seconds). `None` = use the global
    /// `[cooldown] cooldown_secs` / `[cooldown] base_secs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_base_secs: Option<u64>,
    /// Per-combo cooldown cap (seconds). `None` = use the global
    /// `[cooldown] max_secs` (default 3600).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_max_secs: Option<u64>,
    /// Per-combo exponential growth factor. `None` = use the global
    /// `[cooldown] factor` (default 2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_factor: Option<u32>,
    /// LKGP exploration rate (0.0–1.0). `None` = default 0.1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lkgp_exploration_rate: Option<f64>,
    /// Selection window (seconds) for `least_used` / `p2c` modes.
    /// `None` = default 3600.
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

/// Add a target to a combo. The wire shape is the historical one for
/// flat (model) targets, plus a new `sub_combo_id` field for combo-in-
/// combo targets. Exactly one of `model_row_id` / `sub_combo_id` must
/// be `Some`; the XOR is enforced by [`combos::add_target`] because
/// SQLite cannot add a CHECK constraint to a populated table.
///
/// For sub-combo targets the `provider_id` field is accepted for
/// backward-compatibility (the wire shape is uniform) but is
/// effectively ignored: the virtual `"combo"` provider is what the
/// stored row references, and the routing happens through the
/// sub-combo's children, not through the chosen provider. Pass any
/// value (e.g. `"combo"`) and the validator will be happy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddTargetInput {
    pub provider_id: String,
    pub account_id: Option<AccountId>,
    pub model_row_id: Option<ModelRowId>,
    pub sub_combo_id: Option<ComboId>,
    pub priority_order: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
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
    // Default of 1 is the "serial / one target at a time" race
    // window. NOTE: for `Strategy::Priority` the pipeline ignores
    // `race_size` entirely (the operator wants walk-the-row
    // behavior), so this default only meaningfully applies to
    // `Strategy::RoundRobin` and `Strategy::Shuffle`. Don't
    // change it without revisiting `pipeline.rs` step 5.
    let race_size = input.race_size.unwrap_or(1);
    let combo_id = combos::create_combo(conn, &input.name, strategy, race_size)?;

    // Apply the migration-000035 per-combo overrides. Each helper
    // validates its inputs (e.g. `priority_mode` must be a known
    // enum value, `lkgp_exploration_rate` must be in `[0.0, 1.0]`)
    // and writes a single UPDATE. A validation failure here leaves
    // the combo created (with `auto_populate` already run) and
    // surfaces the error to the caller — the operator can fix the
    // bad input and re-POST, or PATCH the combo after the fact.
    apply_combo_overrides(conn, combo_id, input)?;
    Ok(combo_id)
}

/// List all combos.
pub fn list_combos(conn: &Connection) -> Result<Vec<openproxy_types::combos::Combo>> {
    combos::list_combos(conn)
}

/// Lightweight projection of a combo for the "add sub-combo target"
/// picker. We don't need the full [`openproxy_types::combos::Combo`] (race_size,
/// created_at, …) — only the id and the name are surfaced in the UI.
#[derive(Debug, Clone, Serialize)]
pub struct ComboSummary {
    pub id: i64,
    pub name: String,
}

/// List combos that are valid sub-combo targets of `combo_id`.
///
/// A combo is *not* a valid sub-combo target if:
///
/// - it is the same combo (no self-loop), or
/// - adding it as a sub-combo would close a cycle in the sub-combo
///   graph — the probe uses [`combos::combo_in_chain`] with the
///   same depth cap as the row-level check in
///   [`combos::add_target`], so the picker never offers a choice
///   that the API would later reject.
///
/// The function returns the combos in id-ascending order so the UI
/// renders a stable list. Combos with the same id as `combo_id` are
/// silently filtered (no error — the picker is allowed to ask about
/// a combo's own valid sub-combos at any time).
pub fn list_valid_sub_combos(conn: &Connection, combo_id: ComboId) -> Result<Vec<ComboSummary>> {
    let all = combos::list_combos(conn)?;
    let mut out = Vec::with_capacity(all.len());
    for c in all {
        if c.id == combo_id {
            continue;
        }
        // Would adding `c` as a sub-combo of `combo_id` create a
        // cycle? Yes iff `combo_id` is already reachable from `c`
        // in the sub-combo graph (i.e. `c` already contains
        // `combo_id` somewhere downstream). The probe walks down
        // from `c`; see [`combos::combo_in_chain`].
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

/// Add a target to an existing combo. Returns the new target id.
///
/// Validates that the combo, the model (for flat targets) or the
/// sub-combo (for combo-in-combo targets), and (if provided) the
/// account all exist; missing entities surface as
/// [`CoreError::ComboNotFound`], [`CoreError::AccountNotFound`], or
/// [`CoreError::Validation`] respectively (delegated to
/// [`combos::add_target`]). For sub-combo targets, the function also
/// rejects self-loops and would-be cycles via [`combos::combo_in_chain`].
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
        },
    )
}

/// List targets for a combo, ordered by `(priority_order ASC, id ASC)`.
pub fn list_combo_targets(
    conn: &Connection,
    combo_id: ComboId,
) -> Result<Vec<openproxy_types::combos::ComboTarget>> {
    combos::list_targets(conn, combo_id)
}

/// List targets enriched with the model's display name. Used by the admin
/// API so the dashboard can render the human-readable model id without
/// doing a per-row roundtrip to `GET /admin/models`. See
/// [`combos::list_targets_with_model`] for the SQL details.
pub fn list_combo_targets_with_model(
    conn: &Connection,
    combo_id: ComboId,
) -> Result<Vec<openproxy_types::combos::ComboTargetWithModel>> {
    combos::list_targets_with_model(conn, combo_id)
}

/// Delete a combo by id. Idempotent. FK cascade removes its targets.
pub fn delete_combo(conn: &Connection, id: ComboId) -> Result<()> {
    combos::delete_combo(conn, id)
}

/// Delete a combo target by id. The combo_id is not strictly required
/// (the target id is unique on its own), but we validate that the
/// target belongs to the requested combo as a defense-in-depth check
/// against a malformed URL like
/// `DELETE /admin/combos/9999/targets/1` where the target exists
/// but in a different combo.
///
/// Missing rows surface as [`CoreError::Validation`] because there is
/// no dedicated "target not in combo" variant in [`CoreError`]; the
/// server maps that to HTTP 400, which is the right code for a
/// URL-shape mismatch the caller should fix.
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
    // Validate the target belongs to the combo (defense in depth).
    ensure_target_in_combo(conn, combo_id, target_id)?;
    combos::delete_target(conn, target_id)
}

/// Atomically reassign `priority_order` for every target of `combo_id`
/// so the order matches `ordered_ids` (index 0 becomes priority 1,
/// index 1 becomes priority 2, etc.). The call is the swap-style
/// alternative to the partial `update_target_priority` helper: it
/// renumbers the whole combo in a single transaction so two targets
/// can never briefly hold the same `priority_order`.
///
/// The reorder is rejected with [`CoreError::Validation`] when
/// `ordered_ids` is not a permutation of the combo's current target
/// ids (extra id, missing id, duplicate id, or cross-combo id).
///
/// Takes `&mut Connection` because the underlying
/// [`combos::reorder_targets`] opens an `IMMEDIATE` transaction; the
/// HTTP handler hands in the writer guard's `&mut` reborrow.
pub fn reorder_combo_targets(
    conn: &mut Connection,
    combo_id: ComboId,
    ordered_ids: &[ComboTargetId],
) -> Result<()> {
    combos::reorder_targets(conn, combo_id, ordered_ids)
}

/// Force-clear the cooldown for a single target. Used by the
/// dashboard's "Reset cooldown" button: an operator who has
/// diagnosed the upstream issue can clear a parked target without
/// waiting for `cooldown_secs` to elapse.
///
/// Validates that the target belongs to `combo_id` (defense in
/// depth, mirroring [`delete_combo_target`]). Cross-combo
/// combinations surface as [`CoreError::Validation`].
pub fn clear_combo_target_cooldown(
    conn: &Connection,
    combo_id: ComboId,
    target_id: ComboTargetId,
) -> Result<()> {
    // The check is the same shape as `delete_combo_target`: the
    // target row must reference the requested combo. The
    // cascade-on-delete FK on `target_cooldowns.combo_target_id`
    // means a delete of the target will *also* clear its
    // cooldown, but the operator's intent here is "clear the
    // cooldown, not delete the target", so we use the explicit
    // DELETE rather than a no-op target delete.
    ensure_target_in_combo(conn, combo_id, target_id)?;
    openproxy_db::cooldowns::clear_cooldown(conn, target_id)
}
