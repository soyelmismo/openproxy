//! Combos: ordered list of targets with a strategy. Priority or round-robin.
//! Each target references a (provider, model, optional account). Accounts can be rotated within a provider.

use rand::prelude::SliceRandom;
use crate::error::{CoreError, Result};
use crate::ids::{AccountId, ComboId, ComboTargetId, ModelRowId, ProviderId};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    Priority,
    RoundRobin,
    Shuffle,
}

impl Strategy {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Priority => "priority",
            Self::RoundRobin => "round_robin",
            Self::Shuffle => "shuffle",
        }
    }
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "priority" => Ok(Self::Priority),
            "round_robin" => Ok(Self::RoundRobin),
            "shuffle" => Ok(Self::Shuffle),
            other => Err(CoreError::Validation(format!("invalid strategy: {}", other))),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Combo {
    pub id: ComboId,
    pub name: String,
    pub strategy: Strategy,
    pub race_size: u8,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComboTarget {
    pub id: ComboTargetId,
    pub combo_id: ComboId,
    pub provider_id: ProviderId,
    pub account_id: Option<AccountId>,  // None = rotate among healthy accounts of this provider
    /// `Some(_)` for a flat (model) target; `None` when this target
    /// is a sub-combo (i.e. `sub_combo_id` is set). The XOR between
    /// `model_row_id` and `sub_combo_id` is enforced in
    /// [`add_target`].
    pub model_row_id: Option<ModelRowId>,
    /// `Some(_)` for a sub-combo target; `None` for a flat (model)
    /// target. Sub-combo targets are flattened by
    /// [`resolve_combo_to_targets`] before the pipeline iterates the
    /// resolved list — they never reach `execute_single` directly.
    pub sub_combo_id: Option<ComboId>,
    pub priority_order: i32,
}

/// Combo target enriched with the model's display metadata. Used by the
/// admin API so the dashboard can render a human-readable model id
/// (`model_id` = upstream id like `anthropic/claude-3.5-sonnet` and
/// `model_display_name` = the row's `display_name`) without doing a
/// per-row roundtrip to `GET /v1/admin/models`.
///
/// The pipeline's hot path still uses [`ComboTarget`] — `expand_account_rotation`
/// and `resolve_target_order` work on the slim shape — so the enriched
/// variant is intentionally a separate type.
///
/// The three trailing `in_cooldown` / `cooldown_until` / `cooldown_reason`
/// fields are populated by a `LEFT JOIN` against `target_cooldowns` and
/// let the dashboard render the "⏸ cooldown" badge inline with each
/// row. The fields are `Option`/`bool` so adding the cooldown feature
/// to existing rows in flight is a no-op on the JSON shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComboTargetWithModel {
    pub id: ComboTargetId,
    pub combo_id: ComboId,
    pub provider_id: ProviderId,
    pub account_id: Option<AccountId>,
    /// `Some(_)` for a flat (model) target; `None` for a sub-combo
    /// target. See [`ComboTarget::model_row_id`].
    pub model_row_id: Option<ModelRowId>,
    /// `Some(_)` for a sub-combo target; `None` for a flat target.
    /// See [`ComboTarget::sub_combo_id`].
    pub sub_combo_id: Option<ComboId>,
    /// Upstream sub-combo name (the row's `combos.name`) for sub-combo
    /// targets. `None` for flat targets.
    pub sub_combo_name: Option<String>,
    /// Upstream model id from `models.model_id` (e.g. `"anthropic/claude-3.5-sonnet"`).
    /// Empty string for sub-combo targets or if the model row was
    /// somehow deleted (FK cascade should prevent the latter, but we
    /// fall back to `""` to keep the JSON shape stable).
    pub model_id: String,
    /// Optional human-readable name from `models.display_name`. `None`
    /// for sub-combo targets, for rows created before display names
    /// were supported, or for upstream providers that don't expose
    /// one.
    pub model_display_name: Option<String>,
    pub priority_order: i32,
    /// `true` when this target has an active row in `target_cooldowns`
    /// (`cooldown_until > now`). Always `false` for sub-combo targets
    /// — cooldowns attach to the *child* targets after flattening, not
    /// to the indirection row itself.
    #[serde(default)]
    pub in_cooldown: bool,
    /// ISO 8601 UTC of the cooldown expiry. `None` when not in
    /// cooldown. Surfaced so the dashboard can show a relative-time
    /// "back in 30s" hint without re-deriving the timestamp client-
    /// side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_until: Option<String>,
    /// Last error string that fired the cooldown. `None` when not in
    /// cooldown. Useful as a tooltip so the operator doesn't have to
    /// open the usage errors view to find out *why* a target is
    /// parked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_reason: Option<String>,
}

pub fn create_combo(
    conn: &Connection,
    name: &str,
    strategy: Strategy,
    race_size: u8,
) -> Result<ComboId> {
    // Validate race_size against the schema CHECK constraint (1..=8).
    if race_size < 1 || race_size > 8 {
        return Err(CoreError::Validation(format!(
            "race_size must be in 1..=8, got {}",
            race_size
        )));
    }

    let result = conn.execute(
        "INSERT INTO combos(name, strategy, race_size) VALUES (?1, ?2, ?3)",
        params![name, strategy.as_str(), race_size as i64],
    );

    match result {
        Ok(_) => {}
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("UNIQUE") || msg.contains("PRIMARY KEY") {
                return Err(CoreError::Validation(format!(
                    "combo name already exists: {}",
                    name
                )));
            }
            return Err(CoreError::Database {
                message: format!("insert combo {}: {}", name, e),
                source: Some(Box::new(e)),
            });
        }
    }

    let id: i64 = conn
        .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
        .map_err(|e| CoreError::Database {
            message: format!("last_insert_rowid after insert combo: {}", e),
            source: Some(Box::new(e)),
        })?;
    Ok(ComboId(id))
}

pub fn get_combo(conn: &Connection, id: ComboId) -> Result<Option<Combo>> {
    let row = conn
        .query_row(
            "SELECT id, name, strategy, race_size, created_at \
             FROM combos WHERE id = ?1",
            params![id.0],
            row_to_combo,
        )
        .optional()
        .map_err(|e| CoreError::Database {
            message: format!("get combo {}: {}", id.0, e),
            source: Some(Box::new(e)),
        })?;
    Ok(row)
}

pub fn list_combos(conn: &Connection) -> Result<Vec<Combo>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, name, strategy, race_size, created_at \
             FROM combos ORDER BY id",
        )
        .map_err(|e| CoreError::Database {
            message: format!("prepare list combos: {}", e),
            source: Some(Box::new(e)),
        })?;
    let rows = stmt
        .query_map([], row_to_combo)
        .map_err(|e| CoreError::Database {
            message: format!("query list combos: {}", e),
            source: Some(Box::new(e)),
        })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| CoreError::Database {
            message: format!("read combo row: {}", e),
            source: Some(Box::new(e)),
        })?);
    }
    Ok(out)
}

/// Look up a combo by its exact (case-sensitive) name. Returns `Ok(None)`
/// when no row matches.
///
/// Used by the routing layer: a chat request whose `model` field matches
/// `combo:<name>` is dispatched to the combo with `name = <name>`. The
/// match is case-sensitive to match how the names are stored and surfaced
/// in the admin / `/v1/models` endpoints.
pub fn get_combo_by_name(conn: &Connection, name: &str) -> Result<Option<Combo>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, name, strategy, race_size, created_at \
             FROM combos WHERE name = ?1",
        )
        .map_err(|e| CoreError::Database {
            message: format!("prepare get_combo_by_name: {}", e),
            source: Some(Box::new(e)),
        })?;
    let mut rows = stmt
        .query_map(params![name], row_to_combo)
        .map_err(|e| CoreError::Database {
            message: format!("query get_combo_by_name: {}", e),
            source: Some(Box::new(e)),
        })?;
    match rows.next() {
        Some(row) => Ok(Some(row.map_err(|e| CoreError::Database {
            message: format!("read combo_by_name row: {}", e),
            source: Some(Box::new(e)),
        })?)),
        None => Ok(None),
    }
}

pub fn delete_combo(conn: &Connection, id: ComboId) -> Result<()> {
    conn.execute("DELETE FROM combos WHERE id = ?1", params![id.0])
        .map_err(|e| CoreError::Database {
            message: format!("delete combo {}: {}", id.0, e),
            source: Some(Box::new(e)),
        })?;
    Ok(())
}

/// Inputs for [`add_target`]. Carries either a `model_row_id` (a flat
/// target) or a `sub_combo_id` (a combo-in-combo target). Exactly one
/// of the two must be `Some`; the XOR is enforced inside [`add_target`]
/// because SQLite cannot add a CHECK constraint to a populated table.
#[derive(Debug, Clone)]
pub struct AddTargetInput {
    pub combo_id: ComboId,
    pub provider_id: ProviderId,
    pub account_id: Option<AccountId>,
    pub model_row_id: Option<ModelRowId>,
    pub sub_combo_id: Option<ComboId>,
    pub priority_order: i32,
}

pub fn add_target(conn: &Connection, input: AddTargetInput) -> Result<ComboTargetId> {
    let AddTargetInput {
        combo_id,
        provider_id,
        account_id,
        model_row_id,
        sub_combo_id,
        priority_order,
    } = input;

    // XOR: exactly one of model_row_id / sub_combo_id must be set.
    // (SQLite cannot add a CHECK constraint to a populated table, so
    // the rule is enforced here at the boundary that creates rows.)
    if model_row_id.is_some() == sub_combo_id.is_some() {
        return Err(CoreError::Validation(
            "must provide exactly one of model_row_id or sub_combo_id".into(),
        ));
    }

    // Validate the combo exists.
    let combo_exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM combos WHERE id = ?1)",
            params![combo_id.0],
            |r| r.get::<_, i64>(0),
        )
        .map(|v| v != 0)
        .map_err(|e| CoreError::Database {
            message: format!("check combo {} exists: {}", combo_id.0, e),
            source: Some(Box::new(e)),
        })?;
    if !combo_exists {
        return Err(CoreError::ComboNotFound(combo_id.0));
    }

    // Flat-target validations: model row exists and is owned by
    // the requested provider.
    if let Some(model_row_id) = model_row_id {
        let model_exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM models WHERE id = ?1)",
                params![model_row_id.0],
                |r| r.get::<_, i64>(0),
            )
            .map(|v| v != 0)
            .map_err(|e| CoreError::Database {
                message: format!("check model {} exists: {}", model_row_id.0, e),
                source: Some(Box::new(e)),
            })?;
        if !model_exists {
            return Err(CoreError::Validation(format!(
                "model_row_id does not exist: {}",
                model_row_id.0
            )));
        }

        // The model row must actually belong to the requested
        // provider. A model that's owned by `p1` cannot be added to
        // a target whose `provider_id` is `p2` — the routing layer
        // would otherwise try to dispatch a chat call to `p2` while
        // asking it for `p1`'s model, which is meaningless. Defense
        // in depth on top of the FK on `combo_targets.provider_id`.
        let model_provider: String = conn
            .query_row(
                "SELECT provider_id FROM models WHERE id = ?1",
                params![model_row_id.0],
                |r| r.get::<_, String>(0),
            )
            .map_err(|e| CoreError::Database {
                message: format!("read model {} provider_id: {}", model_row_id.0, e),
                source: Some(Box::new(e)),
            })?;
        if model_provider != provider_id.as_str() {
            return Err(CoreError::Validation(format!(
                "model {} belongs to provider '{}', not '{}'",
                model_row_id.0, model_provider, provider_id
            )));
        }
    }

    // Sub-combo validations: target combo is not the parent (no
    // self-loop), the sub-combo exists, and adding it does not
    // introduce a cycle in the sub-combo graph.
    if let Some(sub_id) = sub_combo_id {
        if sub_id == combo_id {
            return Err(CoreError::Validation(
                "combo cannot contain itself".into(),
            ));
        }
        let sub_exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM combos WHERE id = ?1)",
                params![sub_id.0],
                |r| r.get::<_, i64>(0),
            )
            .map(|v| v != 0)
            .map_err(|e| CoreError::Database {
                message: format!("check sub-combo {} exists: {}", sub_id.0, e),
                source: Some(Box::new(e)),
            })?;
        if !sub_exists {
            return Err(CoreError::Validation(format!(
                "sub_combo_id does not exist: {}",
                sub_id.0
            )));
        }
        // Cycle detection: walking down from `sub_id` (the new
        // sub-combo), is `combo_id` (the parent) already reachable?
        // If yes, the new edge would close a cycle. The probe
        // descends the sub-combo graph starting at `sub_id` and
        // uses the same `MAX_SUB_COMBO_DEPTH` cap as the runtime
        // resolver.
        if combo_in_chain(conn, combo_id, sub_id, MAX_SUB_COMBO_DEPTH)? {
            return Err(CoreError::Validation(format!(
                "adding sub-combo {} to combo {} would create a cycle",
                sub_id.0, combo_id.0
            )));
        }
    }

    // If account_id is provided, validate the account exists. (Only
    // meaningful for flat targets — sub-combo targets never carry a
    // pinned account; they expand at runtime by flattening the
    // sub-combo's children.)
    if let Some(aid) = account_id {
        if model_row_id.is_none() {
            return Err(CoreError::Validation(
                "account_id is only valid on flat (model) targets".into(),
            ));
        }
        let account_exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM accounts WHERE id = ?1)",
                params![aid.0],
                |r| r.get::<_, i64>(0),
            )
            .map(|v| v != 0)
            .map_err(|e| CoreError::Database {
                message: format!("check account {} exists: {}", aid.0, e),
                source: Some(Box::new(e)),
            })?;
        if !account_exists {
            return Err(CoreError::AccountNotFound(aid.0));
        }
    }

    let result = conn.execute(
        "INSERT INTO combo_targets(combo_id, provider_id, account_id, model_row_id, sub_combo_id, priority_order) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            combo_id.0,
            provider_id.as_str(),
            account_id.map(|a| a.0),
            model_row_id.map(|m| m.0),
            sub_combo_id.map(|c| c.0),
            priority_order,
        ],
    );

    match result {
        Ok(_) => {}
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("FOREIGN KEY") {
                return Err(CoreError::Validation(format!(
                    "provider_id or sub_combo_id does not exist: {}",
                    provider_id
                )));
            }
            if msg.contains("UNIQUE") {
                return Err(CoreError::Validation(format!(
                    "duplicate target for combo {} (provider={}, account={:?}, model={:?}, sub_combo={:?})",
                    combo_id.0, provider_id, account_id, model_row_id, sub_combo_id
                )));
            }
            return Err(CoreError::Database {
                message: format!("insert combo_target: {}", e),
                source: Some(Box::new(e)),
            });
        }
    }

    let id: i64 = conn
        .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
        .map_err(|e| CoreError::Database {
            message: format!("last_insert_rowid after insert combo_target: {}", e),
            source: Some(Box::new(e)),
        })?;
    Ok(ComboTargetId(id))
}

/// Maximum depth of sub-combo nesting (root combo → sub → sub → …).
/// Enforced both at insert time ([`add_target`], via
/// [`combo_in_chain`]) and at runtime resolution time
/// ([`resolve_combo_to_targets`]). The cap is the same constant in
/// both places so an attacker who hand-writes a row past the insert
/// check still gets a clean runtime error.
pub const MAX_SUB_COMBO_DEPTH: u32 = 8;

/// Walk down the sub-combo chain from `start_combo_id` and return
/// `true` if `target_combo_id` is reachable within `max_depth`
/// steps. Used by [`add_target`] to reject inserts that would close
/// a cycle.
///
/// This is a *best-effort* cycle probe: it descends only into the
/// first sub-combo target it finds at each level, so a malformed
/// chain can be missed in pathological cases. The runtime resolver
/// ([`resolve_combo_to_targets`]) is the authoritative cycle
/// detector — it visits every node — and will catch anything this
/// probe misses.
pub fn combo_in_chain(
    conn: &Connection,
    target_combo_id: ComboId,
    start_combo_id: ComboId,
    max_depth: u32,
) -> Result<bool> {
    if start_combo_id == target_combo_id {
        return Ok(true);
    }
    let mut current = start_combo_id;
    for _ in 0..max_depth {
        let mut stmt = conn
            .prepare(
                "SELECT sub_combo_id FROM combo_targets \
                 WHERE combo_id = ?1 AND sub_combo_id IS NOT NULL",
            )
            .map_err(|e| CoreError::Database {
                message: format!("prepare combo_in_chain stmt: {}", e),
                source: Some(Box::new(e)),
            })?;
        let sub_ids: Vec<i64> = stmt
            .query_map(params![current.0], |r| r.get::<_, Option<i64>>(0))
            .map_err(|e| CoreError::Database {
                message: format!("query combo_in_chain: {}", e),
                source: Some(Box::new(e)),
            })?
            .filter_map(|x| x.ok().flatten())
            .collect();
        if sub_ids.is_empty() {
            return Ok(false);
        }
        for sid in &sub_ids {
            if *sid == target_combo_id.0 {
                return Ok(true);
            }
        }
        // Advance to the first sub-combo found at this level; a
        // deeper probe is only relevant if that branch itself
        // eventually leads back to `target_combo_id`.
        current = ComboId(sub_ids[0]);
    }
    Ok(false)
}

pub fn list_targets(conn: &Connection, combo_id: ComboId) -> Result<Vec<ComboTarget>> {
    // Targets whose provider has been deactivated (active = 0) are
    // excluded from the result. The row is still in `combo_targets` —
    // we don't mutate the table here — so a later reactivation of the
    // provider brings the target back into the routable set without
    // any extra steps. If every target of a combo is in inactive
    // providers, the function returns an empty Vec and the pipeline
    // surfaces `NoHealthyTargets` upstream.
    //
    // Sub-combo targets (where `model_row_id` is NULL) use the
    // virtual `"combo"` provider id; the `p.active = 1` filter still
    // applies to them so a deactivated "combo" provider would hide
    // every sub-combo target. In practice the seed code creates the
    // row with `active = 1` and there is no admin endpoint to
    // deactivate it, but the filter is the same uniform rule for
    // every target type.
    //
    // Orphan targets — rows where the upstream model has been
    // deleted by the scheduler, leaving
    // `(model_row_id IS NULL, sub_combo_id IS NULL)` — are excluded;
    // they remain in the table for audit and re-activation when the
    // model reappears. Without this filter the row would be passed
    // to `RoutingPlan::Combo` and then to `execute_single`, which
    // surfaces a confusing `5xx Internal: ... sub-combo target`
    // (Gate E3).
    let mut stmt = conn
        .prepare(
            "SELECT ct.id, ct.combo_id, ct.provider_id, ct.account_id, ct.model_row_id, \
                    ct.sub_combo_id, ct.priority_order \
             FROM combo_targets ct \
             INNER JOIN providers p ON p.id = ct.provider_id \
             WHERE ct.combo_id = ?1 AND p.active = 1 \
                 AND NOT (ct.model_row_id IS NULL AND ct.sub_combo_id IS NULL) \
             ORDER BY ct.priority_order ASC, ct.id ASC",
        )
        .map_err(|e| CoreError::Database {
            message: format!("prepare list_targets: {}", e),
            source: Some(Box::new(e)),
        })?;
    let rows = stmt
        .query_map(params![combo_id.0], row_to_target)
        .map_err(|e| CoreError::Database {
            message: format!("query list_targets: {}", e),
            source: Some(Box::new(e)),
        })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| CoreError::Database {
            message: format!("read combo_target row: {}", e),
            source: Some(Box::new(e)),
        })?);
    }
    Ok(out)
}

/// Like [`list_targets`], but joins against the `models` table so the
/// caller gets the upstream model id and the optional human-readable
/// display name alongside the target's own columns. The order, the
/// "inactive providers are hidden" filter, and the `priority_order`
/// semantics are identical to [`list_targets`].
///
/// Used by the admin `GET /v1/admin/combos/:id/targets` endpoint; the
/// pipeline's hot path stays on the slim [`ComboTarget`] shape.
pub fn list_targets_with_model(
    conn: &Connection,
    combo_id: ComboId,
) -> Result<Vec<ComboTargetWithModel>> {
    // Same filter + ordering as `list_targets`. The COALESCE on
    // `m.model_id` defends against a stale row whose underlying model
    // got deleted out from under it (FK cascade should make that
    // impossible today, but the dashboard should never crash on a
    // NULL string column).
    //
    // The join against `models` is a `LEFT JOIN` (instead of INNER)
    // because sub-combo targets have `model_row_id = NULL`. The
    // sub-combo's own name is fetched via a second `LEFT JOIN` to
    // `combos`; the columns are aliased to avoid the `id` / `name`
    // collisions between the two joins.
    //
    // The third `LEFT JOIN` is against `target_cooldowns` so the
    // dashboard can render the "⏸ cooldown" badge inline with the
    // target row. We project three columns out of it:
    //
    // - `cooldown_until`: passed through (ISO 8601 string).
    // - `cooldown_until > datetime('now')`: collapsed to a 0/1
    //   `in_cooldown` boolean via the CASE expression below; we don't
    //   want the dashboard doing the timestamp comparison itself.
    // - `reason`: the last error string that parked this target.
    let mut stmt = conn
        .prepare(
            "SELECT ct.id, ct.combo_id, ct.provider_id, ct.account_id, ct.model_row_id, \
                    ct.sub_combo_id, sc.name as sub_combo_name, \
                    COALESCE(m.model_id, ''), m.display_name, ct.priority_order, \
                    tc.cooldown_until, \
                    CASE WHEN tc.cooldown_until IS NOT NULL \
                              AND datetime(tc.cooldown_until) > datetime('now') \
                         THEN 1 ELSE 0 END as in_cooldown, \
                    tc.reason \
             FROM combo_targets ct \
             INNER JOIN providers p ON p.id = ct.provider_id \
             LEFT JOIN models m ON m.id = ct.model_row_id \
             LEFT JOIN combos sc ON sc.id = ct.sub_combo_id \
             LEFT JOIN target_cooldowns tc ON tc.combo_target_id = ct.id \
             WHERE ct.combo_id = ?1 AND p.active = 1 \
             ORDER BY ct.priority_order ASC, ct.id ASC",
        )
        .map_err(|e| CoreError::Database {
            message: format!("prepare list_targets_with_model: {}", e),
            source: Some(Box::new(e)),
        })?;
    let rows = stmt
        .query_map(params![combo_id.0], row_to_target_with_model)
        .map_err(|e| CoreError::Database {
            message: format!("query list_targets_with_model: {}", e),
            source: Some(Box::new(e)),
        })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| CoreError::Database {
            message: format!("read combo_target_with_model row: {}", e),
            source: Some(Box::new(e)),
        })?);
    }
    Ok(out)
}

pub fn get_target(conn: &Connection, id: ComboTargetId) -> Result<Option<ComboTarget>> {
    let row = conn
        .query_row(
            "SELECT id, combo_id, provider_id, account_id, model_row_id, \
                    sub_combo_id, priority_order \
             FROM combo_targets WHERE id = ?1",
            params![id.0],
            row_to_target,
        )
        .optional()
        .map_err(|e| CoreError::Database {
            message: format!("get combo_target {}: {}", id.0, e),
            source: Some(Box::new(e)),
        })?;
    Ok(row)
}

pub fn delete_target(conn: &Connection, id: ComboTargetId) -> Result<()> {
    conn.execute(
        "DELETE FROM combo_targets WHERE id = ?1",
        params![id.0],
    )
    .map_err(|e| CoreError::Database {
        message: format!("delete combo_target {}: {}", id.0, e),
        source: Some(Box::new(e)),
    })?;
    Ok(())
}

/// Reorder a target within (or across) its combo. `new_order` is the new
/// `priority_order`; the caller is responsible for picking a sane value
/// relative to siblings — we don't renumber the rest of the rowset here.
///
/// Returns `Ok(())` whether or not a row was affected; toggling a
/// non-existent id is a silent no-op (the mutation has nothing to do),
/// matching the idempotent style of [`delete_target`].
pub fn update_target_priority(
    conn: &Connection,
    target_id: ComboTargetId,
    new_order: i32,
) -> Result<()> {
    conn.execute(
        "UPDATE combo_targets SET priority_order = ?1 WHERE id = ?2",
        params![new_order, target_id.0],
    )
    .map_err(|e| CoreError::Database {
        message: format!("update priority_order for combo_target {}: {}", target_id.0, e),
        source: Some(Box::new(e)),
    })?;
    Ok(())
}

/// Atomically reassign `priority_order` for every target of `combo_id`
/// so the order matches `ordered_ids` (index 0 → priority 1, index 1
/// → priority 2, etc.). The whole call runs inside a single
/// `IMMEDIATE` transaction so two targets can never share a
/// `priority_order` mid-reorder — the dashboard's ↑/↓ buttons are
/// safe to spam-click without leaving a half-swapped combo on disk.
///
/// The reorder is rejected with [`CoreError::Validation`] when
/// `ordered_ids` is not a permutation of the combo's current target
/// ids (extra id, missing id, duplicate id, or id belonging to a
/// different combo). Doing the validation *before* any UPDATE means
/// a bad call leaves the combo's `priority_order` values untouched.
///
/// Takes `&mut Connection` because rusqlite's transaction API
/// requires it; the caller (typically a handler) gets the
/// `&mut` via the `WriterGuard` deref on `db_pool().writer()`.
pub fn reorder_targets(
    conn: &mut Connection,
    combo_id: ComboId,
    ordered_ids: &[ComboTargetId],
) -> Result<()> {
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| CoreError::Database {
            message: format!("begin reorder_targets tx: {}", e),
            source: Some(Box::new(e)),
        })?;

    // Pull the current target ids for this combo, scoped by `combo_id`
    // so a stray id from another combo can never sneak into the
    // validation set.
    let mut stmt = tx
        .prepare("SELECT id FROM combo_targets WHERE combo_id = ?1")
        .map_err(|e| CoreError::Database {
            message: format!("prepare select combo_targets for reorder: {}", e),
            source: Some(Box::new(e)),
        })?;
    let current: Vec<i64> = stmt
        .query_map(params![combo_id.0], |r| r.get::<_, i64>(0))
        .map_err(|e| CoreError::Database {
            message: format!("query combo_targets for reorder: {}", e),
            source: Some(Box::new(e)),
        })?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| CoreError::Database {
            message: format!("read combo_targets for reorder: {}", e),
            source: Some(Box::new(e)),
        })?;
    drop(stmt);

    // Multiset equality via sorted Vec<i64>: identical multisets
    // produce identical sorted lists, so this check catches missing
    // ids, duplicate ids, and extra ids all at once. The
    // "not belonging to this combo" case falls out for free because
    // the SELECT above is scoped by `combo_id`.
    let mut current_sorted = current.clone();
    current_sorted.sort();
    let mut incoming: Vec<i64> = ordered_ids.iter().map(|i| i.0 as i64).collect();
    incoming.sort();
    if current_sorted != incoming {
        return Err(CoreError::Validation(
            "target_ids must be a permutation of the combo's current targets".into(),
        ));
    }

    // Assign priority_order = 1, 2, 3, ... in the order received. The
    // `combo_id = ?3` guard is what closes the cross-combo rename
    // hole even if the validation above is ever loosened.
    for (idx, tid) in ordered_ids.iter().enumerate() {
        tx.execute(
            "UPDATE combo_targets SET priority_order = ?1 WHERE id = ?2 AND combo_id = ?3",
            params![(idx as i32) + 1, tid.0, combo_id.0],
        )
        .map_err(|e| CoreError::Database {
            message: format!(
                "reorder step {} (target={}, combo={}): {}",
                idx + 1,
                tid.0,
                combo_id.0,
                e
            ),
            source: Some(Box::new(e)),
        })?;
    }
    tx.commit().map_err(|e| CoreError::Database {
        message: format!("commit reorder_targets tx: {}", e),
        source: Some(Box::new(e)),
    })?;
    Ok(())
}

/// Update mutable fields of a combo. Currently only `race_size` is
/// supported; passing `None` leaves the existing value untouched. The
/// `1..=8` CHECK constraint from migration 000004 is enforced by SQLite.
pub fn update_combo(
    conn: &Connection,
    id: ComboId,
    race_size: Option<u8>,
) -> Result<()> {
    if let Some(rs) = race_size {
        if rs < 1 || rs > 8 {
            return Err(CoreError::Validation(format!(
                "race_size must be in 1..=8, got {}",
                rs
            )));
        }
        let affected = conn
            .execute(
                "UPDATE combos SET race_size = ?1 WHERE id = ?2",
                params![rs as i64, id.0],
            )
            .map_err(|e| CoreError::Database {
                message: format!("update race_size for combo {}: {}", id.0, e),
                source: Some(Box::new(e)),
            })?;
        if affected == 0 {
            return Err(CoreError::ComboNotFound(id.0));
        }
    }
    Ok(())
}

pub fn clear_targets(conn: &Connection, combo_id: ComboId) -> Result<()> {
    conn.execute(
        "DELETE FROM combo_targets WHERE combo_id = ?1",
        params![combo_id.0],
    )
    .map_err(|e| CoreError::Database {
        message: format!("clear combo_targets for combo {}: {}", combo_id.0, e),
        source: Some(Box::new(e)),
    })?;
    Ok(())
}

/// Resolve the targets to actually use for a request, in execution order.
/// - priority: ordered by priority_order ASC.
/// - round_robin: rotates target order using a per-combo counter (in-memory, persisted across calls within the same process).
///
/// The counter is held in a global Mutex<HashMap<ComboId, u64>>.
/// On round_robin, the order is shifted by `counter[combo_id] % N` and the counter is incremented.
/// This is per-process; multi-instance deployments are out of scope (single-instance MVP).
pub fn resolve_target_order(
    conn: &Connection,
    combo_id: ComboId,
    strategy: Strategy,
    rr_counters: &Arc<parking_lot::Mutex<std::collections::HashMap<ComboId, u64>>>,
) -> Result<Vec<ComboTarget>> {
    let targets = list_targets(conn, combo_id)?;

    match strategy {
        Strategy::Priority => Ok(targets),
        Strategy::Shuffle => {
            let mut shuffled = targets;
            shuffled.shuffle(&mut rand::thread_rng());
            Ok(shuffled)
        }
        Strategy::RoundRobin => {
            let n = targets.len();
            if n == 0 {
                return Ok(targets);
            }
            let shift = {
                let mut counters = rr_counters.lock();
                let counter = counters.entry(combo_id).or_insert(0);
                let s = (*counter % n as u64) as usize;
                *counter = counter.wrapping_add(1);
                s
            };
            // Rotate: new order = targets[shift..] ++ targets[..shift].
            let mut rotated = Vec::with_capacity(n);
            rotated.extend_from_slice(&targets[shift..]);
            rotated.extend_from_slice(&targets[..shift]);
            Ok(rotated)
        }
    }
}

/// Recursively resolve a combo into a flat list of executable
/// targets. Sub-combo targets are flattened: their children are
/// appended in priority order, then the resolver descends into each
/// child's sub-combo targets. The result is a `Vec<ComboTarget>` in
/// which every entry has `sub_combo_id = None` (i.e. every entry is
/// directly executable by [`crate::pipeline::Pipeline::run`]) and the
/// `priority_order` from the *innermost* target — it is not
/// recomputed, so the relative order between a sub-combo's first and
/// second child is preserved across the flatten.
///
/// Cycle detection is enforced at two levels:
///
/// 1. `visited` tracks the *combo ids* already descended into; a
///    repeat visit is a cycle and the function returns
///    `CoreError::Validation`.
/// 2. `depth` caps the recursion at [`MAX_SUB_COMBO_DEPTH`].
///
/// The runtime safety net in case the row-level check in
/// [`add_target`] ever lets a cycle through.
pub fn resolve_combo_to_targets(
    conn: &Connection,
    combo_id: ComboId,
    visited: &mut Vec<ComboId>,
    depth: u32,
) -> Result<Vec<ComboTarget>> {
    if depth > MAX_SUB_COMBO_DEPTH {
        return Err(CoreError::Validation(format!(
            "combo nesting exceeded max depth {} (combo {})",
            MAX_SUB_COMBO_DEPTH, combo_id.0
        )));
    }
    if visited.iter().any(|c| *c == combo_id) {
        return Err(CoreError::Validation(format!(
            "cycle detected: combo {} appears twice in resolution chain {:?}",
            combo_id.0, visited
        )));
    }
    visited.push(combo_id);

    let targets = list_targets(conn, combo_id)?;
    let mut flat = Vec::with_capacity(targets.len());
    for t in targets {
        if let Some(sub_id) = t.sub_combo_id {
            // Recurse: append the sub-combo's resolved children.
            let sub_targets = resolve_combo_to_targets(conn, sub_id, visited, depth + 1)?;
            flat.extend(sub_targets);
        } else {
            // Flat (model) target: pass through. account_id stays
            // as None so `expand_account_rotation` can fan it out
            // by healthy account at request time.
            flat.push(t);
        }
    }
    visited.pop();
    Ok(flat)
}
/// Auto-populate an empty combo with one target per active model of the first
/// provider that has at least one healthy account and at least one active model.
///
/// Returns `Ok(n)` with the number of targets inserted, or `Ok(0)` when no
/// suitable (provider, models) pair exists. Used by the pipeline's
/// "no healthy targets" fallback and by `admin::create_combo` so a freshly
/// created combo is routable without manual configuration.
///
/// The selection rule is intentionally simple: the first provider (alphabetical
/// by `provider_id`) that has `active = 1` AND at least one account with
/// `health_status = 'healthy'` AND at least one row in `models` with
/// `active = 1`. Model visibility is enforced at write time by
/// [`crate::models::upsert_many`] (rows the upstream dropped are removed
/// on refresh), so the `active = 1` filter alone is the source of truth.
/// Every active model of that provider gets one `combo_target` row with
/// `account_id = NULL` (which the engine expands to one row per healthy
/// account at request time).
pub fn auto_populate_empty_combo(conn: &Connection, combo_id: ComboId) -> Result<usize> {
    // Find the first candidate provider. The query is written defensively
    // so a missing FK can't poison the result: a missing `accounts` row
    // simply means the EXISTS subquery evaluates to 0 and the provider
    // is skipped. The `models` half of the predicate uses the same
    // `active = 1` filter the routing layer uses, so what we auto-populate
    // is exactly what the engine would route to.
    let provider: Option<String> = conn
        .query_row(
            "SELECT p.id \
             FROM providers p \
             WHERE p.active = 1 \
               AND p.id != ?1 \
               AND EXISTS ( \
                   SELECT 1 FROM accounts a \
                   WHERE a.provider_id = p.id \
                     AND a.health_status = 'healthy' \
               ) \
               AND EXISTS ( \
                   SELECT 1 FROM models m \
                   WHERE m.provider_id = p.id \
                     AND m.active = 1 \
               ) \
             ORDER BY p.id ASC \
             LIMIT 1",
            params![crate::seed::VIRTUAL_COMBO_PROVIDER_ID],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .map_err(|e| CoreError::Database {
            message: format!("query auto_populate provider: {}", e),
            source: Some(Box::new(e)),
        })?;

    let Some(provider_id) = provider else {
        return Ok(0);
    };

    // Now insert one combo_target per active model of that provider.
    // `priority_order` follows the model's `id` so the order is
    // deterministic; account_id stays NULL so account rotation kicks in
    // at request time.
    let provider_typed = ProviderId::new(provider_id);
    let added = combos_insert_targets_for_active_models(conn, combo_id, &provider_typed)?;
    Ok(added)
}

/// Insert one combo_target row per active model of `provider_id` for the
/// given `combo_id`. `priority_order` is the model's row id (so the order
/// matches the discovery order). Returns the number of rows inserted.
///
/// Exposed at module scope so both `auto_populate_empty_combo` and the
/// admin `create_combo` path can call it without duplicating the SQL.
///
/// Model visibility is enforced at write time by
/// [`crate::models::upsert_many`] (rows the upstream dropped are removed
/// on refresh), so the `active = 1` filter is the source of truth — we
/// deliberately do NOT add an `expires_at > now()` clause here, for the
/// same reason the routing layer doesn't: a row in the table with
/// `active = 1` reflects a model the upstream currently lists.
fn combos_insert_targets_for_active_models(
    conn: &Connection,
    combo_id: ComboId,
    provider_id: &ProviderId,
) -> Result<usize> {
    // Pull the active model rows. We capture `(row_id, model_id)` so the
    // test can verify the priority_order choice and the log can echo the
    // model that ended up routable.
    let mut stmt = conn
        .prepare(
            "SELECT id, model_id FROM models \
             WHERE provider_id = ?1 \
               AND active = 1 \
             ORDER BY id ASC",
        )
        .map_err(|e| CoreError::Database {
            message: format!("prepare active models: {}", e),
            source: Some(Box::new(e)),
        })?;
    let rows: Vec<(i64, String)> = stmt
        .query_map(params![provider_id.as_str()], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })
        .map_err(|e| CoreError::Database {
            message: format!("query active models: {}", e),
            source: Some(Box::new(e)),
        })?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| CoreError::Database {
            message: format!("read active models: {}", e),
            source: Some(Box::new(e)),
        })?;

    let mut added = 0usize;
    for (row_id, _model_id) in &rows {
        // Ignore UNIQUE collisions: this helper is reused by a code path
        // that may run after a manual add. Better to keep the existing
        // row than to bubble an error up.
        let res = conn.execute(
            "INSERT OR IGNORE INTO combo_targets(\
                 combo_id, provider_id, account_id, model_row_id, priority_order\
             ) VALUES (?1, ?2, NULL, ?3, ?4)",
            params![combo_id.0, provider_id.as_str(), row_id, row_id],
        );
        match res {
            Ok(n) if n > 0 => added += 1,
            Ok(_) => {} // UNIQUE collision, no-op
            Err(e) => {
                return Err(CoreError::Database {
                    message: format!("insert auto target: {}", e),
                    source: Some(Box::new(e)),
                });
            }
        }
    }
    Ok(added)
}

/// Expand a target with account_id=None into N targets (one per healthy account of the provider).
/// Used at request time, not stored in DB.
///
/// Sub-combo targets (`model_row_id = None`, `sub_combo_id = Some(_)`)
/// are passed through unchanged: they have no concept of "account of
/// this provider" (the virtual `combo` provider has none) and are
/// resolved by [`resolve_combo_to_targets`] before the pipeline sees
/// them. If a sub-combo target reaches this function it is the
/// caller's bug; the function is lenient and just keeps the row as-is
/// so a future code path that mixes flat + sub-combo targets in the
/// same input still gets a sensible result.
///
/// When 0 healthy accounts exist for a provider the target is kept
/// with `account_id = None` — the pipeline handles authentication
/// (or lack thereof), not the combo.
pub fn expand_account_rotation(
    conn: &Connection,
    targets: Vec<ComboTarget>,
) -> Result<Vec<ComboTarget>> {
    let mut out: Vec<ComboTarget> = Vec::with_capacity(targets.len());
    for t in targets {
        // Sub-combo target: pass through; it will be flattened by
        // `resolve_combo_to_targets` upstream of this function.
        if t.sub_combo_id.is_some() {
            out.push(t);
            continue;
        }
        if t.account_id.is_some() {
            out.push(t);
            continue;
        }
        // account_id is None: look up healthy accounts for this provider
        // and fan out one target per account.
        let mut stmt = conn
            .prepare(
                "SELECT id FROM accounts \
                 WHERE provider_id = ?1 AND health_status = 'healthy' \
                 ORDER BY priority ASC, id ASC",
            )
            .map_err(|e| CoreError::Database {
                message: format!("prepare expand_account_rotation: {}", e),
                source: Some(Box::new(e)),
            })?;
        let rows = stmt
            .query_map(params![t.provider_id.as_str()], |row| {
                row.get::<_, i64>(0)
            })
            .map_err(|e| CoreError::Database {
                message: format!("query expand_account_rotation: {}", e),
                source: Some(Box::new(e)),
            })?;
        let mut count = 0usize;
        for r in rows {
            let account_id = r.map_err(|e| CoreError::Database {
                message: format!("read account id row: {}", e),
                source: Some(Box::new(e)),
            })?;
            let mut clone = t.clone();
            clone.account_id = Some(AccountId(account_id));
            out.push(clone);
            count += 1;
        }
        // If 0 healthy accounts: keep the target with account_id=None.
        // The pipeline is responsible for authentication decisions,
        // not the combo.
        if count == 0 {
            out.push(t);
        }
    }
    Ok(out)
}

fn row_to_combo(row: &Row<'_>) -> rusqlite::Result<Combo> {
    let id: i64 = row.get(0)?;
    let name: String = row.get(1)?;
    let strategy_str: String = row.get(2)?;
    let race_size: i64 = row.get(3)?;
    let created_at: String = row.get(4)?;

    let strategy = Strategy::parse(&strategy_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            2,
            rusqlite::types::Type::Text,
            Box::new(FromStrError(format!("{}", e))),
        )
    })?;

    if race_size < 1 || race_size > 8 {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            3,
            rusqlite::types::Type::Integer,
            Box::new(FromStrError(format!(
                "race_size out of range: {}",
                race_size
            ))),
        ));
    }

    Ok(Combo {
        id: ComboId(id),
        name,
        strategy,
        race_size: race_size as u8,
        created_at,
    })
}

fn row_to_target(row: &Row<'_>) -> rusqlite::Result<ComboTarget> {
    let id: i64 = row.get(0)?;
    let combo_id: i64 = row.get(1)?;
    let provider_id: String = row.get(2)?;
    let account_id: Option<i64> = row.get(3)?;
    let model_row_id: Option<i64> = row.get(4)?;
    let sub_combo_id: Option<i64> = row.get(5)?;
    let priority_order: i32 = row.get(6)?;

    Ok(ComboTarget {
        id: ComboTargetId(id),
        combo_id: ComboId(combo_id),
        provider_id: ProviderId::new(provider_id),
        account_id: account_id.map(AccountId),
        model_row_id: model_row_id.map(ModelRowId),
        sub_combo_id: sub_combo_id.map(ComboId),
        priority_order,
    })
}

fn row_to_target_with_model(row: &Row<'_>) -> rusqlite::Result<ComboTargetWithModel> {
    let id: i64 = row.get(0)?;
    let combo_id: i64 = row.get(1)?;
    let provider_id: String = row.get(2)?;
    let account_id: Option<i64> = row.get(3)?;
    let model_row_id: Option<i64> = row.get(4)?;
    let sub_combo_id: Option<i64> = row.get(5)?;
    let sub_combo_name: Option<String> = row.get(6)?;
    // `model_id` is COALESCEd in the SELECT, so a NULL is never observed
    // here in practice; the empty string is the documented fallback.
    let model_id: String = row.get(7)?;
    let model_display_name: Option<String> = row.get(8)?;
    let priority_order: i32 = row.get(9)?;
    // The cooldown columns come from a `LEFT JOIN`; missing rows
    // surface as `None` / `0` here. `in_cooldown` is the 0/1
    // collapse of the `cooldown_until > now` expression in the
    // SELECT — we trust the DB to do the timestamp compare so the
    // dashboard doesn't have to redo it client-side.
    let cooldown_until: Option<String> = row.get(10)?;
    let in_cooldown: i64 = row.get(11)?;
    let cooldown_reason: Option<String> = row.get(12)?;

    Ok(ComboTargetWithModel {
        id: ComboTargetId(id),
        combo_id: ComboId(combo_id),
        provider_id: ProviderId::new(provider_id),
        account_id: account_id.map(AccountId),
        model_row_id: model_row_id.map(ModelRowId),
        sub_combo_id: sub_combo_id.map(ComboId),
        sub_combo_name,
        model_id,
        model_display_name,
        priority_order,
        in_cooldown: in_cooldown != 0,
        cooldown_until,
        cooldown_reason,
    })
}

#[derive(Debug)]
struct FromStrError(String);
impl std::fmt::Display for FromStrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for FromStrError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::conn::DbPool;
    use crate::db::migrations;
    use crate::providers::{self, AuthType, ProviderFormat};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    /// Build a fresh in-process pool: temp dir on disk, migrations applied.
    fn fresh_pool() -> (DbPool, PathBuf) {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "openproxy-combos-test-{}-{}-{}",
            pid, nanos, n
        ));
        std::fs::create_dir_all(&dir).expect("mkdir tempdir");
        let path = dir.join("combos.db");
        let pool = DbPool::open(&path).expect("open pool");
        {
            let mut w = pool.writer();
            migrations::run(&mut w).expect("migrations");
        }
        (pool, path)
    }

    /// Seed a provider so combo_targets FKs can be satisfied.
    fn seed_provider(conn: &Connection, id: &str) {
        providers::create(
            conn,
            &ProviderId::new(id),
            id,
            "https://example.com",
            AuthType::Bearer,
            ProviderFormat::Openai,
            None,
            None,
        )
        .expect("seed provider");
    }

    /// Seed a model row and return its row_id.
    fn seed_model(conn: &Connection, provider: &str, model_id: &str) -> ModelRowId {
        conn.execute(
            "INSERT INTO models(provider_id, model_id, target_format) VALUES (?1, ?2, 'openai')",
            params![provider, model_id],
        )
        .expect("seed model");
        let id: i64 = conn
            .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
            .expect("last_insert_rowid");
        ModelRowId(id)
    }

    #[test]
    fn create_combo_and_get() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();

        let id = create_combo(&conn, "primary", Strategy::Priority, 1).expect("create");
        let got = get_combo(&conn, id).expect("get").expect("present");
        assert_eq!(got.id, id);
        assert_eq!(got.name, "primary");
        assert_eq!(got.strategy, Strategy::Priority);
        assert_eq!(got.race_size, 1);
        assert!(!got.created_at.is_empty());
    }

    #[test]
    fn create_combo_duplicate_name_fails() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();

        create_combo(&conn, "dup", Strategy::Priority, 1).expect("first");
        let err = create_combo(&conn, "dup", Strategy::RoundRobin, 2).expect_err("dup");
        match err {
            CoreError::Validation(msg) => assert!(msg.contains("combo name already exists")),
            other => panic!("expected Validation, got {:?}", other),
        }
    }

    #[test]
    fn add_and_list_targets() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "p1");
        seed_provider(&conn, "p2");
        let m1 = seed_model(&conn, "p1", "m1");
        let m2 = seed_model(&conn, "p2", "m2");

        let cid = create_combo(&conn, "c", Strategy::Priority, 1).expect("create");
        let t1 = add_target(
            &conn,
            AddTargetInput {
                combo_id: cid,
                provider_id: ProviderId::new("p1"),
                account_id: None,
                model_row_id: Some(m1),
                sub_combo_id: None,
                priority_order: 10,
            },
        )
        .expect("add t1");
        let t2 = add_target(
            &conn,
            AddTargetInput {
                combo_id: cid,
                provider_id: ProviderId::new("p2"),
                account_id: None,
                model_row_id: Some(m2),
                sub_combo_id: None,
                priority_order: 20,
            },
        )
        .expect("add t2");

        let targets = list_targets(&conn, cid).expect("list");
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].id, t1);
        assert_eq!(targets[1].id, t2);
        assert_eq!(targets[0].provider_id, ProviderId::new("p1"));
        assert_eq!(targets[1].provider_id, ProviderId::new("p2"));
        assert!(targets[0].account_id.is_none());
    }

    #[test]
    fn priority_strategy_preserves_order() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "p");
        let m = seed_model(&conn, "p", "m");
        let cid = create_combo(&conn, "prio", Strategy::Priority, 1).expect("create");

        // Insert in reverse priority order to confirm list_targets sorts.
        for (prov_suffix, prio) in [("z", 30), ("a", 10), ("m", 20)] {
            seed_provider(&conn, &format!("px-{}", prov_suffix));
            let mp = seed_model(&conn, &format!("px-{}", prov_suffix), "mx");
            add_target(
                &conn,
                AddTargetInput {
                    combo_id: cid,
                    provider_id: ProviderId::new(format!("px-{}", prov_suffix)),
                    account_id: None,
                    model_row_id: Some(mp),
                    sub_combo_id: None,
                    priority_order: prio,
                },
            )
            .expect("add");
        }
        // We added 3 targets using the "p" model's id? No: we re-seeded per
        // provider with separate model rows. Re-check the list is sorted by
        // priority_order ASC.
        let targets = list_targets(&conn, cid).expect("list");
        assert_eq!(targets.len(), 3);
        assert_eq!(targets[0].priority_order, 10);
        assert_eq!(targets[1].priority_order, 20);
        assert_eq!(targets[2].priority_order, 30);

        // resolve_target_order with priority must return the same order.
        let rr = Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let resolved =
            resolve_target_order(&conn, cid, Strategy::Priority, &rr).expect("resolve");
        assert_eq!(resolved.len(), 3);
        assert_eq!(resolved[0].priority_order, 10);
        assert_eq!(resolved[1].priority_order, 20);
        assert_eq!(resolved[2].priority_order, 30);
        // Sanity: m was created but never used in targets.
        let _ = m;
    }

    #[test]
    fn round_robin_rotates_order() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "p");
        let cid = create_combo(&conn, "rr", Strategy::RoundRobin, 1).expect("create");
        for prov in ["a", "b", "c"] {
            seed_provider(&conn, prov);
            let mp = seed_model(&conn, prov, "mx");
            let n = match prov {
                "a" => 10,
                "b" => 20,
                "c" => 30,
                _ => unreachable!(),
            };
            add_target(
                &conn,
                AddTargetInput {
                    combo_id: cid,
                    provider_id: ProviderId::new(prov),
                    account_id: None,
                    model_row_id: Some(mp),
                    sub_combo_id: None,
                    priority_order: n,
                },
            )
            .expect("add");
        }

        let rr = Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let r1 = resolve_target_order(&conn, cid, Strategy::RoundRobin, &rr).expect("r1");
        let r2 = resolve_target_order(&conn, cid, Strategy::RoundRobin, &rr).expect("r2");
        let r3 = resolve_target_order(&conn, cid, Strategy::RoundRobin, &rr).expect("r3");

        // Same content, just permuted.
        let ids1: Vec<i32> = r1.iter().map(|t| t.priority_order).collect();
        let ids2: Vec<i32> = r2.iter().map(|t| t.priority_order).collect();
        let ids3: Vec<i32> = r3.iter().map(|t| t.priority_order).collect();
        assert_eq!(ids1, vec![10, 20, 30]);
        assert_eq!(ids2, vec![20, 30, 10]);
        assert_eq!(ids3, vec![30, 10, 20]);
    }

    #[test]
    fn expand_account_rotation_with_no_account() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "lonely");
        let m = seed_model(&conn, "lonely", "m");
        let cid = create_combo(&conn, "c", Strategy::Priority, 1).expect("create");
        let _tid = add_target(
            &conn,
            AddTargetInput {
                combo_id: cid,
                provider_id: ProviderId::new("lonely"),
                account_id: None,
                model_row_id: Some(m),
                sub_combo_id: None,
                priority_order: 1,
            },
        )
        .expect("add");

        // No accounts registered at all → 0 healthy → target kept with
        // account_id=None (the pipeline handles auth, not the combo).
        let targets = list_targets(&conn, cid).expect("list");
        assert_eq!(targets.len(), 1);
        let expanded = expand_account_rotation(&conn, targets).expect("expand");
        assert_eq!(expanded.len(), 1, "0 healthy accounts → target kept as-is");
        assert!(expanded[0].account_id.is_none());
    }

    #[test]
    fn expand_account_rotation_with_one_account() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "solo");
        let m = seed_model(&conn, "solo", "m");
        let cid = create_combo(&conn, "c", Strategy::Priority, 1).expect("create");
        let _tid = add_target(
            &conn,
            AddTargetInput {
                combo_id: cid,
                provider_id: ProviderId::new("solo"),
                account_id: None,
                model_row_id: Some(m),
                sub_combo_id: None,
                priority_order: 1,
            },
        )
        .expect("add");

        // Insert one healthy account via raw SQL (avoids needing MasterKey for
        // an explicit test of the rotation logic).
        conn.execute(
            "INSERT INTO accounts(provider_id, api_key_encrypted) VALUES ('solo', X'00')",
            [],
        )
        .expect("seed account");

        let targets = list_targets(&conn, cid).expect("list");
        let expanded = expand_account_rotation(&conn, targets).expect("expand");
        assert_eq!(expanded.len(), 1, "1 healthy account → 1 target");
        assert!(expanded[0].account_id.is_some());
    }

    #[test]
    fn expand_account_rotation_with_multiple_accounts_only_healthy() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "multi");
        let m = seed_model(&conn, "multi", "m");
        let cid = create_combo(&conn, "c", Strategy::Priority, 1).expect("create");
        let _tid = add_target(
            &conn,
            AddTargetInput {
                combo_id: cid,
                provider_id: ProviderId::new("multi"),
                account_id: None,
                model_row_id: Some(m),
                sub_combo_id: None,
                priority_order: 1,
            },
        )
        .expect("add");

        // 3 accounts: 2 healthy, 1 unhealthy.
        conn.execute(
            "INSERT INTO accounts(provider_id, api_key_encrypted, health_status, priority) \
             VALUES ('multi', X'00', 'healthy', 10)",
            [],
        )
        .expect("seed a1");
        conn.execute(
            "INSERT INTO accounts(provider_id, api_key_encrypted, health_status, priority) \
             VALUES ('multi', X'00', 'healthy', 20)",
            [],
        )
        .expect("seed a2");
        conn.execute(
            "INSERT INTO accounts(provider_id, api_key_encrypted, health_status, priority) \
             VALUES ('multi', X'00', 'unhealthy', 5)",
            [],
        )
        .expect("seed a3");

        let targets = list_targets(&conn, cid).expect("list");
        let expanded = expand_account_rotation(&conn, targets).expect("expand");
        assert_eq!(expanded.len(), 2, "1 unhealthy skipped → 2 targets");
        for t in &expanded {
            assert!(t.account_id.is_some());
        }
        // Ordered by (priority ASC, id ASC) per the SELECT: the lower-priority-id
        // account comes first.
        assert!(expanded[0].account_id.unwrap().0 < expanded[1].account_id.unwrap().0);
    }

    #[test]
    fn strategy_parse_roundtrip() {
        for (variant, s) in [
            (Strategy::Priority, "priority"),
            (Strategy::RoundRobin, "round_robin"),
        ] {
            assert_eq!(variant.as_str(), s);
            assert_eq!(Strategy::parse(s).expect("parse"), variant);
            // Serde roundtrip too.
            let j = serde_json::to_string(&variant).unwrap();
            assert_eq!(j, format!("\"{}\"", s));
            let back: Strategy = serde_json::from_str(&j).unwrap();
            assert_eq!(back, variant);
        }
        assert!(Strategy::parse("bogus").is_err());
    }

    #[test]
    fn update_target_priority_changes_order() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "p");
        let m = seed_model(&conn, "p", "m");
        let cid = create_combo(&conn, "u", Strategy::Priority, 1).expect("create");
        let t1 = add_target(
            &conn,
            AddTargetInput {
                combo_id: cid,
                provider_id: ProviderId::new("p"),
                account_id: None,
                model_row_id: Some(m),
                sub_combo_id: None,
                priority_order: 10,
            },
        )
        .expect("add t1");

        // Move t1 from 10 → 99.
        update_target_priority(&conn, t1, 99).expect("update");
        let targets = list_targets(&conn, cid).expect("list");
        assert_eq!(targets[0].id, t1);
        assert_eq!(targets[0].priority_order, 99, "new order persisted");

        // Missing id is a silent no-op, not an error.
        update_target_priority(&conn, ComboTargetId(77777), 5).expect("no-op");
    }

    #[test]
    fn update_combo_changes_race_size() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        let cid = create_combo(&conn, "uc", Strategy::Priority, 1).expect("create");

        // Valid update.
        update_combo(&conn, cid, Some(4)).expect("update race_size");
        let got = get_combo(&conn, cid).expect("get").expect("present");
        assert_eq!(got.race_size, 4);

        // None leaves race_size untouched.
        update_combo(&conn, cid, None).expect("update none");
        let got = get_combo(&conn, cid).expect("get").expect("present");
        assert_eq!(got.race_size, 4, "None is a no-op");

        // Out-of-range rejected before reaching SQL.
        let err = update_combo(&conn, cid, Some(0)).expect_err("rejects 0");
        assert!(matches!(err, CoreError::Validation(_)));
        let err = update_combo(&conn, cid, Some(9)).expect_err("rejects 9");
        assert!(matches!(err, CoreError::Validation(_)));

        // Missing combo surfaces as ComboNotFound (only relevant for Some —
        // the None branch can't tell the difference between a missing row
        // and a present one, which matches its "leave alone" contract).
        let err = update_combo(&conn, ComboId(424242), Some(2)).expect_err("missing");
        assert!(matches!(err, CoreError::ComboNotFound(424242)));
    }

    #[test]
    fn list_targets_filters_inactive_providers() {
        // Targets whose provider has been deactivated must not appear
        // in the routable target list, even though the row is still in
        // `combo_targets`. This is what makes "deactivate" a soft,
        // reversible operation: a later reactivation brings the
        // target back automatically.
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "live");
        seed_provider(&conn, "dead");
        let m_live = seed_model(&conn, "live", "ml");
        let m_dead = seed_model(&conn, "dead", "md");
        let cid = create_combo(&conn, "c", Strategy::Priority, 1).expect("create");
        add_target(
            &conn,
            AddTargetInput {
                combo_id: cid,
                provider_id: ProviderId::new("live"),
                account_id: None,
                model_row_id: Some(m_live),
                sub_combo_id: None,
                priority_order: 10,
            },
        )
        .expect("add live");
        add_target(
            &conn,
            AddTargetInput {
                combo_id: cid,
                provider_id: ProviderId::new("dead"),
                account_id: None,
                model_row_id: Some(m_dead),
                sub_combo_id: None,
                priority_order: 20,
            },
        )
        .expect("add dead");

        // Both visible while the providers are active.
        let targets = list_targets(&conn, cid).expect("list");
        assert_eq!(targets.len(), 2);

        // Deactivate `dead` and confirm only `live` survives.
        providers::set_active(&conn, &ProviderId::new("dead"), false).expect("deactivate");
        let targets = list_targets(&conn, cid).expect("list after deactivate");
        assert_eq!(targets.len(), 1, "inactive provider's target is hidden");
        assert_eq!(targets[0].provider_id, ProviderId::new("live"));

        // Deactivate `live` too — combo is now empty (pipeline will
        // surface NoHealthyTargets).
        providers::set_active(&conn, &ProviderId::new("live"), false).expect("deactivate live");
        let targets = list_targets(&conn, cid).expect("list all-inactive");
        assert_eq!(targets.len(), 0, "no active providers → empty target list");

        // Reactivate `dead` and it comes back without any combo-side
        // mutation: this is the "reversible soft-disable" guarantee.
        providers::set_active(&conn, &ProviderId::new("dead"), true).expect("reactivate");
        let targets = list_targets(&conn, cid).expect("list after reactivate");
        assert_eq!(targets.len(), 1, "reactivated provider's target reappears");
        assert_eq!(targets[0].provider_id, ProviderId::new("dead"));
    }

    // -----------------------------------------------------------------
    // list_targets orphan filter (Gate E3)
    // -----------------------------------------------------------------

    #[test]
    fn list_targets_excludes_orphan_targets() {
        // Gate E3: when a `models` row is deleted and the FK
        // `combo_targets.model_row_id ... ON DELETE SET NULL`
        // fires, the surviving `combo_targets` row has
        // `(model_row_id IS NULL, sub_combo_id IS NULL)`. `list_targets`
        // must drop that row from the routable result. The row is
        // NOT deleted from the table — it stays for audit and
        // re-activation when the model reappears.
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "prov-x");
        seed_provider(&conn, "prov-y");
        let m_keep = seed_model(&conn, "prov-x", "m-keep");
        let m_drop = seed_model(&conn, "prov-y", "m-drop");

        let cid = create_combo(&conn, "c", Strategy::Priority, 1).expect("create");
        let t_keep = add_target(
            &conn,
            AddTargetInput {
                combo_id: cid,
                provider_id: ProviderId::new("prov-x"),
                account_id: None,
                model_row_id: Some(m_keep),
                sub_combo_id: None,
                priority_order: 10,
            },
        )
        .expect("add t_keep");
        let t_orphan = add_target(
            &conn,
            AddTargetInput {
                combo_id: cid,
                provider_id: ProviderId::new("prov-y"),
                account_id: None,
                model_row_id: Some(m_drop),
                sub_combo_id: None,
                priority_order: 20,
            },
        )
        .expect("add t_orphan");

        // Sanity: both visible while the underlying models are alive.
        let before = list_targets(&conn, cid).expect("list before delete");
        assert_eq!(before.len(), 2);

        // Trigger the orphan state by deleting the `models` row
        // directly. We deliberately do NOT go through
        // `models::delete` here: that admin path explicitly pre-cleans
        // `combo_targets` rows pointing at the model (see
        // `models::delete` in models.rs), which would suppress the
        // orphan state we are testing for. Bypassing the helper lets
        // the FK `ON DELETE SET NULL` cascade do its Gate-D job.
        conn.execute("DELETE FROM models WHERE id = ?1", params![m_drop.0])
            .expect("raw delete model");

        // Routing-layer listing drops the orphan.
        let after = list_targets(&conn, cid).expect("list after delete");
        assert_eq!(after.len(), 1, "the (NULL, NULL) orphan is filtered out");
        assert_eq!(after[0].id, t_keep, "the live target survives");
        assert!(!after.iter().any(|t| t.id == t_orphan), "orphan is gone from result");

        // The orphan row is still in the table — the filter is
        // read-time only, the row is the audit trail.
        let raw: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM combo_targets WHERE id = ?1",
                params![t_orphan.0],
                |r| r.get(0),
            )
            .expect("count raw");
        assert_eq!(raw, 1, "orphan row is preserved in combo_targets");
        // And its model_row_id really is NULL now.
        let model_col: Option<i64> = conn
            .query_row(
                "SELECT model_row_id FROM combo_targets WHERE id = ?1",
                params![t_orphan.0],
                |r| r.get(0),
            )
            .expect("read model_row_id");
        assert!(
            model_col.is_none(),
            "the cascade set model_row_id to NULL (the orphan state)"
        );
    }

    #[test]
    fn list_targets_returns_empty_for_fully_orphaned_combo() {
        // Every target of this combo is an orphan. `list_targets`
        // must return an empty vec — the routing layer then builds
        // a `RoutingPlan::Combo` with `targets: vec![]`, which the
        // pipeline already handles by surfacing `NoHealthyTargets`.
        // This protects against a future refactor that flips the
        // orphan→error semantic back on at a different layer.
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "prov-1");
        seed_provider(&conn, "prov-2");
        let m1 = seed_model(&conn, "prov-1", "m1");
        let m2 = seed_model(&conn, "prov-2", "m2");

        let cid = create_combo(&conn, "all-orphans", Strategy::Priority, 1).expect("create");
        add_target(
            &conn,
            AddTargetInput {
                combo_id: cid,
                provider_id: ProviderId::new("prov-1"),
                account_id: None,
                model_row_id: Some(m1),
                sub_combo_id: None,
                priority_order: 10,
            },
        )
        .expect("add t1");
        add_target(
            &conn,
            AddTargetInput {
                combo_id: cid,
                provider_id: ProviderId::new("prov-2"),
                account_id: None,
                model_row_id: Some(m2),
                sub_combo_id: None,
                priority_order: 20,
            },
        )
        .expect("add t2");

        // Sanity: both visible before the cascade.
        let before = list_targets(&conn, cid).expect("list before");
        assert_eq!(before.len(), 2);

        // Bypass `models::delete` so the cascade fires and creates
        // orphans (see the note in the previous test).
        conn.execute("DELETE FROM models WHERE id = ?1", params![m1.0])
            .expect("raw delete m1");
        conn.execute("DELETE FROM models WHERE id = ?1", params![m2.0])
            .expect("raw delete m2");

        // Every target is now an orphan → empty routing result.
        let after = list_targets(&conn, cid).expect("list after");
        assert!(
            after.is_empty(),
            "fully-orphaned combo surfaces zero routable targets"
        );

        // The orphan rows are still in the table (audit trail).
        let raw_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM combo_targets WHERE combo_id = ?1",
                params![cid.0],
                |r| r.get(0),
            )
            .expect("count raw rows");
        assert_eq!(raw_count, 2, "the orphan rows are still in the table");

        // Belt-and-braces: the routing layer agrees, because
        // `list_targets` is the only place a routing read can pick
        // up rows from `combo_targets`. With an empty target vec the
        // plan is a `Combo` with no usable members — the pipeline
        // will surface `NoHealthyTargets`, not a 5xx.
        use crate::routing;
        let plan = routing::resolve(&conn, "combo:all-orphans").expect("resolve");
        match plan {
            routing::RoutingPlan::Combo { targets, .. } => {
                assert!(
                    targets.is_empty(),
                    "routing::resolve mirrors the data-layer filter"
                );
            }
            other => panic!("expected RoutingPlan::Combo, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------
    // auto_populate_empty_combo
    // -----------------------------------------------------------------

    /// Helper: seed a healthy account for `provider_id` so the
    /// auto-populate query's "exists healthy account" branch is satisfied.
    /// We use raw SQL to avoid pulling a MasterKey into the test surface
    /// (the `accounts::create` path requires one).
    fn seed_healthy_account(conn: &Connection, provider_id: &str) {
        conn.execute(
            "INSERT INTO accounts(provider_id, api_key_encrypted, health_status) \
             VALUES (?1, X'00', 'healthy')",
            params![provider_id],
        )
        .expect("seed healthy account");
    }

    #[test]
    fn auto_populate_fills_empty_combo_with_active_models() {
        // Single provider, one healthy account, two active models.
        // After `auto_populate_empty_combo` the combo has 2 targets.
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "p");
        seed_healthy_account(&conn, "p");
        seed_model(&conn, "p", "m1");
        seed_model(&conn, "p", "m2");
        let cid = create_combo(&conn, "nerd", Strategy::Priority, 1).expect("create");

        // Sanity: combo starts empty.
        assert!(list_targets(&conn, cid).expect("list").is_empty());

        let added = auto_populate_empty_combo(&conn, cid).expect("populate");
        assert_eq!(added, 2, "one target per active model");

        let targets = list_targets(&conn, cid).expect("list");
        assert_eq!(targets.len(), 2);
        // account_id is None so account rotation kicks in at request time.
        for t in &targets {
            assert!(t.account_id.is_none(), "auto-populate leaves account_id NULL");
            assert_eq!(t.provider_id, ProviderId::new("p"));
        }
    }

    #[test]
    fn auto_populate_returns_zero_when_no_healthy_account() {
        // No healthy account → no candidate provider → 0 added.
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "p");
        seed_model(&conn, "p", "m1");
        let cid = create_combo(&conn, "c", Strategy::Priority, 1).expect("create");
        let added = auto_populate_empty_combo(&conn, cid).expect("populate");
        assert_eq!(added, 0);
        assert!(list_targets(&conn, cid).expect("list").is_empty());
    }

    #[test]
    fn auto_populate_returns_zero_when_no_active_models() {
        // Healthy account but no active models → 0 added.
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "p");
        seed_healthy_account(&conn, "p");
        let cid = create_combo(&conn, "c", Strategy::Priority, 1).expect("create");
        let added = auto_populate_empty_combo(&conn, cid).expect("populate");
        assert_eq!(added, 0);
    }

    #[test]
    fn auto_populate_skips_inactive_providers() {
        // A deactivated provider must not be picked.
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "live");
        seed_provider(&conn, "dead");
        seed_healthy_account(&conn, "live");
        seed_healthy_account(&conn, "dead");
        seed_model(&conn, "live", "ml");
        seed_model(&conn, "dead", "md");
        providers::set_active(&conn, &ProviderId::new("dead"), false).expect("deactivate");
        let cid = create_combo(&conn, "c", Strategy::Priority, 1).expect("create");

        let added = auto_populate_empty_combo(&conn, cid).expect("populate");
        assert_eq!(added, 1, "only the live provider contributes a target");
        let targets = list_targets(&conn, cid).expect("list");
        assert_eq!(targets[0].provider_id, ProviderId::new("live"));
    }

    #[test]
    fn auto_populate_adds_target_when_one_already_exists() {
        // The combo_targets UNIQUE constraint is
        // (combo_id, provider_id, account_id, model_id) but SQLite
        // treats NULLs as distinct in UNIQUE indexes, so a target
        // with account_id=NULL does not collide with another target
        // that has the same (combo, provider, model) and account_id=NULL.
        // The auto-populate helper therefore adds a row even when one
        // already exists. The test pins down this behavior so a future
        // schema change that flips NULL handling won't go unnoticed.
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "p");
        seed_healthy_account(&conn, "p");
        let m = seed_model(&conn, "p", "m");
        let cid = create_combo(&conn, "c", Strategy::Priority, 1).expect("create");
        add_target(
            &conn,
            AddTargetInput {
                combo_id: cid,
                provider_id: ProviderId::new("p"),
                account_id: None,
                model_row_id: Some(m),
                sub_combo_id: None,
                priority_order: 42,
            },
        )
        .expect("seed target");

        let added = auto_populate_empty_combo(&conn, cid).expect("populate");
        // SQLite's UNIQUE allows the new (NULL,NULL) tuple to coexist
        // with the existing one, so 1 row is added.
        assert_eq!(added, 1, "NULL account_id is distinct under UNIQUE");
        let targets = list_targets(&conn, cid).expect("list");
        assert_eq!(targets.len(), 2);
    }

    // -----------------------------------------------------------------
    // list_targets_with_model
    // -----------------------------------------------------------------

    #[test]
    fn list_targets_with_model_joins_display_name() {
        // The enriched variant must return the upstream model id and
        // the display name from the `models` row, ordered like
        // `list_targets`. We seed two models with distinct display
        // names and assert both come through unchanged.
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "p1");
        seed_provider(&conn, "p2");
        // Add display names by direct UPDATE — seed_model doesn't
        // take a display name parameter.
        let m1 = seed_model(&conn, "p1", "anthropic/claude-3.5-sonnet");
        let m2 = seed_model(&conn, "p2", "openai/gpt-4o");
        conn.execute(
            "UPDATE models SET display_name = ?1 WHERE id = ?2",
            rusqlite::params!["Claude 3.5 Sonnet", m1.0],
        )
        .expect("update m1 name");
        conn.execute(
            "UPDATE models SET display_name = ?1 WHERE id = ?2",
            rusqlite::params!["GPT-4o", m2.0],
        )
        .expect("update m2 name");

        let cid = create_combo(&conn, "enrich", Strategy::Priority, 1).expect("create");
        add_target(
            &conn,
            AddTargetInput {
                combo_id: cid,
                provider_id: ProviderId::new("p1"),
                account_id: None,
                model_row_id: Some(m1),
                sub_combo_id: None,
                priority_order: 10,
            },
        )
        .expect("add p1");
        add_target(
            &conn,
            AddTargetInput {
                combo_id: cid,
                provider_id: ProviderId::new("p2"),
                account_id: None,
                model_row_id: Some(m2),
                sub_combo_id: None,
                priority_order: 20,
            },
        )
        .expect("add p2");

        let enriched = list_targets_with_model(&conn, cid).expect("enriched list");
        assert_eq!(enriched.len(), 2);
        assert_eq!(enriched[0].model_id, "anthropic/claude-3.5-sonnet");
        assert_eq!(
            enriched[0].model_display_name.as_deref(),
            Some("Claude 3.5 Sonnet")
        );
        assert_eq!(enriched[1].model_id, "openai/gpt-4o");
        assert_eq!(enriched[1].model_display_name.as_deref(), Some("GPT-4o"));
        // model_row_id survives the join too.
        assert_eq!(enriched[0].model_row_id, Some(m1));
        assert_eq!(enriched[1].model_row_id, Some(m2));
        // And it is *not* a sub-combo target.
        assert!(enriched[0].sub_combo_id.is_none());
        assert!(enriched[0].sub_combo_name.is_none());
    }

    // -----------------------------------------------------------------
    // reorder_targets
    // -----------------------------------------------------------------

    #[test]
    fn reorder_targets_assigns_sequential_priorities() {
        // Three targets, then a full reorder → priority_order becomes
        // 1, 2, 3 in the order passed in, regardless of the previous
        // values. The second call confirms the function is idempotent
        // and not relying on a "diff" computation.
        let (pool, _path) = fresh_pool();
        let mut conn = pool.writer();
        seed_provider(&conn, "p");
        let m = seed_model(&conn, "p", "m");
        let cid = create_combo(&conn, "reorder", Strategy::Priority, 1).expect("create");
        let t1 = add_target(
            &conn,
            AddTargetInput {
                combo_id: cid,
                provider_id: ProviderId::new("p"),
                account_id: None,
                model_row_id: Some(m),
                sub_combo_id: None,
                priority_order: 10,
            },
        )
        .expect("t1");
        let t2 = add_target(
            &conn,
            AddTargetInput {
                combo_id: cid,
                provider_id: ProviderId::new("p"),
                account_id: None,
                model_row_id: Some(m),
                sub_combo_id: None,
                priority_order: 20,
            },
        )
        .expect("t2");
        let t3 = add_target(
            &conn,
            AddTargetInput {
                combo_id: cid,
                provider_id: ProviderId::new("p"),
                account_id: None,
                model_row_id: Some(m),
                sub_combo_id: None,
                priority_order: 30,
            },
        )
        .expect("t3");

        // Reorder to [t3, t1, t2] — non-trivial swap.
        reorder_targets(&mut conn, cid, &[t3, t1, t2]).expect("reorder");
        let targets = list_targets(&conn, cid).expect("list");
        assert_eq!(targets[0].id, t3);
        assert_eq!(targets[0].priority_order, 1);
        assert_eq!(targets[1].id, t1);
        assert_eq!(targets[1].priority_order, 2);
        assert_eq!(targets[2].id, t2);
        assert_eq!(targets[2].priority_order, 3);

        // Calling again with the same order is a no-op.
        reorder_targets(&mut conn, cid, &[t3, t1, t2]).expect("reorder again");
        let targets = list_targets(&conn, cid).expect("list again");
        assert_eq!(targets[0].priority_order, 1);
    }

    #[test]
    fn reorder_targets_rejects_missing_id() {
        // Sending only a subset of the combo's current target ids
        // must be rejected with Validation, and the on-disk
        // priorities must be left untouched.
        let (pool, _path) = fresh_pool();
        let mut conn = pool.writer();
        seed_provider(&conn, "p");
        let m = seed_model(&conn, "p", "m");
        let cid = create_combo(&conn, "r", Strategy::Priority, 1).expect("create");
        let t1 = add_target(
            &conn,
            AddTargetInput {
                combo_id: cid,
                provider_id: ProviderId::new("p"),
                account_id: None,
                model_row_id: Some(m),
                sub_combo_id: None,
                priority_order: 10,
            },
        )
        .expect("t1");
        // t2 is added only to populate the combo so the reorder
        // has something to drop; its id is intentionally unused.
        let _t2 = add_target(
            &conn,
            AddTargetInput {
                combo_id: cid,
                provider_id: ProviderId::new("p"),
                account_id: None,
                model_row_id: Some(m),
                sub_combo_id: None,
                priority_order: 20,
            },
        )
        .expect("t2");

        // Snapshot.
        let before: Vec<i32> = list_targets(&conn, cid)
            .expect("list")
            .into_iter()
            .map(|t| t.priority_order)
            .collect();

        // Drop t2.
        let err = reorder_targets(&mut conn, cid, &[t1])
            .expect_err("missing id must be rejected");
        assert!(matches!(err, CoreError::Validation(_)));

        let after: Vec<i32> = list_targets(&conn, cid)
            .expect("list")
            .into_iter()
            .map(|t| t.priority_order)
            .collect();
        assert_eq!(before, after, "rejected reorder must not touch priorities");
    }

    #[test]
    fn reorder_targets_rejects_extra_id() {
        // An id not in the combo must be rejected, even if it
        // happens to be a real `combo_targets` row from a different
        // combo. The validation's `combo_id` scope is what closes
        // that hole.
        let (pool, _path) = fresh_pool();
        let mut conn = pool.writer();
        seed_provider(&conn, "p");
        let m = seed_model(&conn, "p", "m");
        let cid1 = create_combo(&conn, "c1", Strategy::Priority, 1).expect("create");
        let cid2 = create_combo(&conn, "c2", Strategy::Priority, 1).expect("create");
        let t1a = add_target(
            &conn,
            AddTargetInput {
                combo_id: cid1,
                provider_id: ProviderId::new("p"),
                account_id: None,
                model_row_id: Some(m),
                sub_combo_id: None,
                priority_order: 10,
            },
        )
        .expect("t1a");
        let t1b = add_target(
            &conn,
            AddTargetInput {
                combo_id: cid1,
                provider_id: ProviderId::new("p"),
                account_id: None,
                model_row_id: Some(m),
                sub_combo_id: None,
                priority_order: 20,
            },
        )
        .expect("t1b");
        let t2a = add_target(
            &conn,
            AddTargetInput {
                combo_id: cid2,
                provider_id: ProviderId::new("p"),
                account_id: None,
                model_row_id: Some(m),
                sub_combo_id: None,
                priority_order: 30,
            },
        )
        .expect("t2a");

        // Try to insert t2a into cid1's reorder.
        let err = reorder_targets(&mut conn, cid1, &[t1a, t1b, t2a])
            .expect_err("cross-combo id must be rejected");
        assert!(matches!(err, CoreError::Validation(_)));
    }

    // -----------------------------------------------------------------
    // add_target provider/model cross-check
    // -----------------------------------------------------------------

    #[test]
    fn add_target_rejects_model_from_other_provider() {
        // The model/provider cross-check in add_target: a target
        // with provider_id="p2" referencing a model that belongs to
        // "p1" is meaningless to the routing layer and must be
        // rejected. This used to slip through silently (the FK on
        // combo_targets.provider_id only checked the provider row,
        // not whether the model was *owned* by it).
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "p1");
        seed_provider(&conn, "p2");
        let m1 = seed_model(&conn, "p1", "m1");
        let cid = create_combo(&conn, "xcheck", Strategy::Priority, 1).expect("create");

        let err = add_target(
            &conn,
            AddTargetInput {
                combo_id: cid,
                provider_id: ProviderId::new("p2"), // wrong provider for the model
                account_id: None,
                model_row_id: Some(m1),
                sub_combo_id: None,
                priority_order: 10,
            },
        )
        .expect_err("cross-provider model must be rejected");
        match &err {
            CoreError::Validation(msg) => assert!(
                msg.contains("belongs to provider"),
                "error must explain the cross-provider mismatch, got: {}",
                msg
            ),
            other => panic!("expected Validation, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------
    // Sub-combo (combo-in-combo) support
    // -----------------------------------------------------------------

    /// Helper: seed a combo and one flat target. Returns the
    /// combo's id; used as a building block for the sub-combo tests
    /// below. Idempotent on the provider id: if `provider` already
    /// exists we don't try to recreate it (some tests reuse the
    /// same provider across combos).
    fn seed_combo_with_one_model(
        conn: &Connection,
        combo_name: &str,
        provider: &str,
        model: &str,
    ) -> (ComboId, ModelRowId) {
        let already: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM providers WHERE id = ?1)",
                rusqlite::params![provider],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            != 0;
        if !already {
            seed_provider(conn, provider);
        }
        let m = seed_model(conn, provider, model);
        let cid = create_combo(conn, combo_name, Strategy::Priority, 1).expect("create combo");
        add_target(
            conn,
            AddTargetInput {
                combo_id: cid,
                provider_id: ProviderId::new(provider),
                account_id: None,
                model_row_id: Some(m),
                sub_combo_id: None,
                priority_order: 10,
            },
        )
        .expect("add flat target");
        (cid, m)
    }

    #[test]
    fn add_target_with_sub_combo_succeeds() {
        // Combo B is created with one flat target. Adding B as a
        // sub-combo of A produces a row whose `sub_combo_id` is
        // populated and whose `model_row_id` is NULL.
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        let (cid_a, _) =
            seed_combo_with_one_model(&conn, "A", "p1", "ma");
        let (cid_b, _) =
            seed_combo_with_one_model(&conn, "B", "p2", "mb");

        // We need the "combo" virtual provider to satisfy the
        // `combo_targets.provider_id` NOT-NULL + FK; the boot
        // sequence would normally insert it, but in this test we
        // are running with raw migrations so we seed it by hand.
        let _ = seed_virtual_combo(&conn);

        let tid = add_target(
            &conn,
            AddTargetInput {
                combo_id: cid_a,
                provider_id: ProviderId::new("combo"),
                account_id: None,
                model_row_id: None,
                sub_combo_id: Some(cid_b),
                priority_order: 50,
            },
        )
        .expect("add sub-combo target");
        assert!(tid.0 > 0);

        // A's target list now has the auto-populated flat target
        // (from create_combo) plus the sub-combo entry we just
        // added. The auto-populate behaviour is verified
        // elsewhere; here we only assert that the sub-combo
        // target is present and well-formed.
        let listed = list_targets(&conn, cid_a).expect("list A");
        let sub_row = listed
            .iter()
            .find(|t| t.sub_combo_id.is_some())
            .expect("sub-combo target present in A");
        assert_eq!(sub_row.sub_combo_id, Some(cid_b));
        assert!(sub_row.model_row_id.is_none());
    }

    #[test]
    fn add_target_with_self_loop_fails() {
        // Adding A as a sub-combo of itself must be rejected before
        // any row is written.
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        let (cid_a, _) =
            seed_combo_with_one_model(&conn, "self", "p1", "m");
        let _ = seed_virtual_combo(&conn);

        let err = add_target(
            &conn,
            AddTargetInput {
                combo_id: cid_a,
                provider_id: ProviderId::new("combo"),
                account_id: None,
                model_row_id: None,
                sub_combo_id: Some(cid_a),
                priority_order: 1,
            },
        )
        .expect_err("self-loop must be rejected");
        match &err {
            CoreError::Validation(msg) => {
                assert!(msg.contains("combo cannot contain itself"), "msg={}", msg)
            }
            other => panic!("expected Validation, got {:?}", other),
        }
    }

    #[test]
    fn add_target_with_cycle_fails() {
        // Build A → B (A contains B as a sub-combo), then try to
        // add A as a sub-combo of B. The would-be cycle A→B→A
        // must be rejected by the row-level probe.
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        let (cid_a, _) =
            seed_combo_with_one_model(&conn, "A", "p1", "ma");
        let (cid_b, _) =
            seed_combo_with_one_model(&conn, "B", "p2", "mb");
        let _ = seed_virtual_combo(&conn);

        add_target(
            &conn,
            AddTargetInput {
                combo_id: cid_a,
                provider_id: ProviderId::new("combo"),
                account_id: None,
                model_row_id: None,
                sub_combo_id: Some(cid_b),
                priority_order: 1,
            },
        )
        .expect("add B as sub-combo of A");

        let err = add_target(
            &conn,
            AddTargetInput {
                combo_id: cid_b,
                provider_id: ProviderId::new("combo"),
                account_id: None,
                model_row_id: None,
                sub_combo_id: Some(cid_a),
                priority_order: 1,
            },
        )
        .expect_err("cycle A->B->A must be rejected");
        match &err {
            CoreError::Validation(msg) => assert!(
                msg.contains("cycle"),
                "error must mention the cycle, got: {}",
                msg
            ),
            other => panic!("expected Validation, got {:?}", other),
        }
    }

    #[test]
    fn add_target_with_both_model_and_subcombo_fails() {
        // Exactly one of model_row_id / sub_combo_id must be set;
        // sending both is a 400.
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        let (cid_a, m1) =
            seed_combo_with_one_model(&conn, "A", "p1", "m1");
        let (cid_b, _) =
            seed_combo_with_one_model(&conn, "B", "p2", "m2");
        let _ = seed_virtual_combo(&conn);

        let err = add_target(
            &conn,
            AddTargetInput {
                combo_id: cid_a,
                provider_id: ProviderId::new("p1"),
                account_id: None,
                model_row_id: Some(m1),
                sub_combo_id: Some(cid_b),
                priority_order: 1,
            },
        )
        .expect_err("XOR must reject both fields set");
        match &err {
            CoreError::Validation(msg) => assert!(
                msg.contains("exactly one of"),
                "error must explain the XOR, got: {}",
                msg
            ),
            other => panic!("expected Validation, got {:?}", other),
        }
    }

    #[test]
    fn add_target_with_neither_fails() {
        // Sending neither model_row_id nor sub_combo_id is a 400.
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        let (cid_a, _) =
            seed_combo_with_one_model(&conn, "A", "p1", "m1");
        let _ = seed_virtual_combo(&conn);

        let err = add_target(
            &conn,
            AddTargetInput {
                combo_id: cid_a,
                provider_id: ProviderId::new("combo"),
                account_id: None,
                model_row_id: None,
                sub_combo_id: None,
                priority_order: 1,
            },
        )
        .expect_err("XOR must reject both fields unset");
        match &err {
            CoreError::Validation(msg) => assert!(
                msg.contains("exactly one of"),
                "error must explain the XOR, got: {}",
                msg
            ),
            other => panic!("expected Validation, got {:?}", other),
        }
    }

    #[test]
    fn resolve_combo_flattens_sub_combo() {
        // A has 1 flat target (m1) + 1 sub-combo B which has 2
        // flat targets (m2, m3). After resolve, A has 3 flat
        // entries in priority order: m1 (prio 10), then m2/m3
        // (prio 20/30 from B).
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        let (cid_a, m1) =
            seed_combo_with_one_model(&conn, "A", "p1", "m1");
        let (cid_b, m2) =
            seed_combo_with_one_model(&conn, "B", "p2", "m2");
        // p3 needs an explicit provider row because
        // `seed_combo_with_one_model` only registers "p2" for B.
        seed_provider(&conn, "p3");
        let m3 = seed_model(&conn, "p3", "m3");
        add_target(
            &conn,
            AddTargetInput {
                combo_id: cid_b,
                provider_id: ProviderId::new("p3"),
                account_id: None,
                model_row_id: Some(m3),
                sub_combo_id: None,
                priority_order: 30,
            },
        )
        .expect("add m3 to B");
        let _ = seed_virtual_combo(&conn);
        add_target(
            &conn,
            AddTargetInput {
                combo_id: cid_a,
                provider_id: ProviderId::new("combo"),
                account_id: None,
                model_row_id: None,
                sub_combo_id: Some(cid_b),
                priority_order: 50,
            },
        )
        .expect("add B as sub-combo of A");

        let flat = resolve_combo_to_targets(&conn, cid_a, &mut vec![], 0)
            .expect("resolve");
        assert_eq!(flat.len(), 3, "A=1 flat + B=2 flat → 3 total");
        let model_ids: Vec<Option<ModelRowId>> =
            flat.iter().map(|t| t.model_row_id).collect();
        // All flattened entries must be directly executable.
        assert!(flat.iter().all(|t| t.sub_combo_id.is_none()));
        assert_eq!(model_ids, vec![Some(m1), Some(m2), Some(m3)]);
    }

    #[test]
    fn resolve_combo_respects_max_depth() {
        // Build a chain of nested sub-combos that exceeds
        // MAX_SUB_COMBO_DEPTH. The runtime resolver must reject
        // the chain even if the row-level check in `add_target`
        // lets it through. We construct the chain by successive
        // inserts — `add_target`'s depth probe walks 8 levels
        // deep, so the 9-deep chain still inserts cleanly. The
        // runtime probe (`depth > 8`) is the one that fires.
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        let _ = seed_virtual_combo(&conn);

        // Chain: C0 → C1 → C2 → … → C9 (10 combos, 9 edges).
        // Each combo owns a distinct (provider, model) pair so
        // the UNIQUE constraint on `models(provider_id, model_id)`
        // doesn't fire.
        let mut combos: Vec<ComboId> = Vec::new();
        for i in 0..=9 {
            let name = format!("C{}", i);
            let provider = format!("px-{}", i);
            let model = format!("mx-{}", i);
            let (cid, _) =
                seed_combo_with_one_model(&conn, &name, &provider, &model);
            combos.push(cid);
        }
        for i in 0..combos.len() - 1 {
            add_target(
                &conn,
                AddTargetInput {
                    combo_id: combos[i],
                    provider_id: ProviderId::new("combo"),
                    account_id: None,
                    model_row_id: None,
                    sub_combo_id: Some(combos[i + 1]),
                    priority_order: (i + 1) as i32,
                },
            )
            .expect("chain insert ok");
        }

        let err = resolve_combo_to_targets(&conn, combos[0], &mut vec![], 0)
            .expect_err("depth > MAX_SUB_COMBO_DEPTH must be rejected");
        match &err {
            CoreError::Validation(msg) => assert!(
                msg.contains("max depth"),
                "error must mention max depth, got: {}",
                msg
            ),
            other => panic!("expected Validation, got {:?}", other),
        }
    }

    #[test]
    fn resolve_combo_detects_cycle() {
        // The row-level `combo_in_chain` probe catches the
        // insert-time A→B→A cycle, so we don't need to forge one
        // by hand. This test just confirms the validator rejects
        // it; the runtime probe is exercised by the
        // `resolve_combo_respects_max_depth` test above.
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        let (cid_a, _) =
            seed_combo_with_one_model(&conn, "A", "p1", "m1");
        let (cid_b, _) =
            seed_combo_with_one_model(&conn, "B", "p2", "m2");
        let _ = seed_virtual_combo(&conn);

        add_target(
            &conn,
            AddTargetInput {
                combo_id: cid_a,
                provider_id: ProviderId::new("combo"),
                account_id: None,
                model_row_id: None,
                sub_combo_id: Some(cid_b),
                priority_order: 1,
            },
        )
        .expect("add A->B");

        let err = add_target(
            &conn,
            AddTargetInput {
                combo_id: cid_b,
                provider_id: ProviderId::new("combo"),
                account_id: None,
                model_row_id: None,
                sub_combo_id: Some(cid_a),
                priority_order: 1,
            },
        )
        .expect_err("validator rejects the cycle");
        match &err {
            CoreError::Validation(msg) => assert!(
                msg.contains("cycle"),
                "error must mention the cycle, got: {}",
                msg
            ),
            other => panic!("expected Validation, got {:?}", other),
        }
    }

    #[test]
    fn resolve_combo_no_cycle_when_sub_combo_independent() {
        // Two independent combos that reference each other through
        // a shared common combo: not a cycle. (A is the root; the
        // chain is short enough not to trip the depth probe.)
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        let (cid_a, _) =
            seed_combo_with_one_model(&conn, "A", "p1", "m1");
        let (cid_b, _) =
            seed_combo_with_one_model(&conn, "B", "p2", "m2");
        let _ = seed_virtual_combo(&conn);
        add_target(
            &conn,
            AddTargetInput {
                combo_id: cid_a,
                provider_id: ProviderId::new("combo"),
                account_id: None,
                model_row_id: None,
                sub_combo_id: Some(cid_b),
                priority_order: 1,
            },
        )
        .expect("add B as sub-combo of A");
        let flat = resolve_combo_to_targets(&conn, cid_a, &mut vec![], 0)
            .expect("resolve");
        // 1 flat in A + 1 flat in B = 2
        assert_eq!(flat.len(), 2);
    }

    #[test]
    fn list_targets_with_model_includes_sub_combo_name() {
        // The enriched variant must surface `sub_combo_id` and
        // `sub_combo_name` for sub-combo targets, with `model_id`
        // empty and `model_row_id` None.
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        let (cid_a, _) =
            seed_combo_with_one_model(&conn, "parent", "p1", "m1");
        let (cid_b, _) =
            seed_combo_with_one_model(&conn, "child", "p2", "m2");
        let _ = seed_virtual_combo(&conn);
        add_target(
            &conn,
            AddTargetInput {
                combo_id: cid_a,
                provider_id: ProviderId::new("combo"),
                account_id: None,
                model_row_id: None,
                sub_combo_id: Some(cid_b),
                priority_order: 1,
            },
        )
        .expect("add sub-combo");

        let enriched =
            list_targets_with_model(&conn, cid_a).expect("enriched list");
        // 1 flat in A + 1 sub-combo target = 2 rows
        assert_eq!(enriched.len(), 2);
        let sub_row = enriched
            .iter()
            .find(|t| t.sub_combo_id.is_some())
            .expect("sub-combo row present");
        assert_eq!(sub_row.sub_combo_id, Some(cid_b));
        assert_eq!(sub_row.sub_combo_name.as_deref(), Some("child"));
        assert!(sub_row.model_row_id.is_none());
        assert_eq!(sub_row.model_id, "");
    }

    /// Insert the virtual "combo" provider row by hand. In
    /// production the `AppState::new` boot path calls
    /// `seed::seed_virtual_combo_provider`, but the per-test
    /// `fresh_pool()` here skips that step; this helper papers over
    /// the gap so each test can opt in cleanly.
    fn seed_virtual_combo(conn: &Connection) -> std::result::Result<(), String> {
        conn.execute(
            "INSERT OR IGNORE INTO providers(id, name, base_url, auth_type, format) \
             VALUES ('combo', 'combo', 'https://invalid.local/combo', 'bearer', 'openai')",
            [],
        )
        .map_err(|e| format!("{}", e))?;
        Ok(())
    }
}
