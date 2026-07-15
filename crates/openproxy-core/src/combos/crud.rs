use super::*;
use crate::error::*;
use crate::ids::*;
use rusqlite::{Connection, Row, params};
pub fn create_combo(
    conn: &Connection,
    name: &str,
    strategy: Strategy,
    race_size: u8,
) -> Result<ComboId> {
    // Validate race_size against the schema CHECK constraint (1..=8).
    if !(1..=8).contains(&race_size) {
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
            return Err(crate::error::map_db_error_ctx(format!(
                "insert combo {}",
                name
            ))(e));
        }
    }

    let id: i64 = conn
        .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
        .map_err(crate::error::map_db_error)?;
    Ok(ComboId(id))
}

pub fn get_combo(conn: &Connection, id: ComboId) -> Result<Option<Combo>> {
    let row = conn
        .query_row(
            "SELECT id, name, strategy, race_size, created_at, context_window, \
                    priority_mode, cooldown_mode, cooldown_base_secs, cooldown_max_secs, \
                    cooldown_factor, lkgp_exploration_rate, selection_window_secs \
             FROM combos WHERE id = ?1",
            params![id.0],
            row_to_combo,
        )
        .optional()
        .map_err(crate::error::map_db_error_ctx(format!(
            "get combo {}",
            id.0
        )))?;
    Ok(row)
}

pub fn list_combos(conn: &Connection) -> Result<Vec<Combo>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, name, strategy, race_size, created_at, context_window, \
                   priority_mode, cooldown_mode, cooldown_base_secs, cooldown_max_secs, \
                   cooldown_factor, lkgp_exploration_rate, selection_window_secs \
             FROM combos ORDER BY id",
        )
        .map_err(crate::error::map_db_error)?;
    let rows = stmt
        .query_map([], row_to_combo)
        .map_err(crate::error::map_db_error)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(crate::error::map_db_error)?);
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
            "SELECT id, name, strategy, race_size, created_at, context_window, \
                   priority_mode, cooldown_mode, cooldown_base_secs, cooldown_max_secs, \
                   cooldown_factor, lkgp_exploration_rate, selection_window_secs \
             FROM combos WHERE name = ?1",
        )
        .map_err(crate::error::map_db_error)?;
    let mut rows = stmt
        .query_map(params![name], row_to_combo)
        .map_err(crate::error::map_db_error)?;
    match rows.next() {
        Some(row) => Ok(Some(row.map_err(crate::error::map_db_error)?)),
        None => Ok(None),
    }
}

pub fn delete_combo(conn: &Connection, id: ComboId) -> Result<()> {
    conn.execute("DELETE FROM combos WHERE id = ?1", params![id.0])
        .map_err(crate::error::map_db_error_ctx(format!(
            "delete combo {}",
            id.0
        )))?;
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
        .map_err(crate::error::map_db_error_ctx(format!(
            "check combo {} exists",
            combo_id.0
        )))?;
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
            .map_err(crate::error::map_db_error_ctx(format!(
                "check model {} exists",
                model_row_id.0
            )))?;
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
            .map_err(crate::error::map_db_error_ctx(format!(
                "read model {} provider_id",
                model_row_id.0
            )))?;
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
            return Err(CoreError::Validation("combo cannot contain itself".into()));
        }
        let sub_exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM combos WHERE id = ?1)",
                params![sub_id.0],
                |r| r.get::<_, i64>(0),
            )
            .map(|v| v != 0)
            .map_err(crate::error::map_db_error_ctx(format!(
                "check sub-combo {} exists",
                sub_id.0
            )))?;
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
            .map_err(crate::error::map_db_error_ctx(format!(
                "check account {} exists",
                aid.0
            )))?;
        if !account_exists {
            return Err(CoreError::AccountNotFound(aid.0));
        }
    }

    // Look up the upstream `model_id` from the `models` table so we
    // can stamp it onto `combo_targets.upstream_model_id` (Gate F1).
    // Sub-combo targets have no associated `models` row and get
    // `NULL` (they reference a combo, not a model). This is the
    // bookkeeping the reconnect path in [`models::upsert_many`]
    // uses to re-bind an orphan target when its upstream model
    // reappears after a transient absence (Gate B → Gate D cascade).
    let upstream_model_id: Option<String> = if let Some(mrid) = model_row_id {
        Some(
            conn.query_row(
                "SELECT model_id FROM models WHERE id = ?1",
                params![mrid.0],
                |r| r.get::<_, String>(0),
            )
            .map_err(crate::error::map_db_error_ctx(format!(
                "read model {} upstream model_id",
                mrid.0
            )))?,
        )
    } else {
        None
    };

    // Programmatic duplicate check to prevent duplicate targets (since SQLite's UNIQUE
    // constraint does not prevent duplicates when account_id is NULL).
    let target_exists: bool = conn
        .query_row(
            "SELECT EXISTS( \
             SELECT 1 FROM combo_targets \
             WHERE combo_id = ?1 \
               AND provider_id = ?2 \
               AND COALESCE(account_id, -1) = COALESCE(?3, -1) \
               AND COALESCE(model_row_id, -1) = COALESCE(?4, -1) \
               AND COALESCE(sub_combo_id, -1) = COALESCE(?5, -1))",
            params![
                combo_id.0,
                provider_id.as_str(),
                account_id.map(|a| a.0),
                model_row_id.map(|m| m.0),
                sub_combo_id.map(|c| c.0),
            ],
            |r| r.get::<_, i64>(0),
        )
        .map(|v| v != 0)
        .unwrap_or(false);

    if target_exists {
        return Err(CoreError::Validation(format!(
            "duplicate target for combo {} (provider={}, account={:?}, model={:?}, sub_combo={:?})",
            combo_id.0, provider_id, account_id, model_row_id, sub_combo_id
        )));
    }

    let result = conn.execute(
        "INSERT INTO combo_targets(combo_id, provider_id, account_id, model_row_id, sub_combo_id, upstream_model_id, priority_order) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            combo_id.0,
            provider_id.as_str(),
            account_id.map(|a| a.0),
            model_row_id.map(|m| m.0),
            sub_combo_id.map(|c| c.0),
            upstream_model_id,
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
            return Err(crate::error::map_db_error_ctx("insert combo_target")(e));
        }
    }

    let id: i64 = conn
        .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
        .map_err(crate::error::map_db_error)?;
    Ok(ComboTargetId(id))
}

/// Gate F1: re-bind orphaned `combo_targets` rows that referenced a
/// vanished upstream model.
///
/// This helper is the heart of the reconnect path. The call shape is
/// `reconnect_orphan_targets(conn, provider, upstream_model_id,
/// new_model_row_id)` and is intended to be called from
/// [`crate::models::upsert_many`] *inside the same transaction* that
/// just deleted the old `models` row and inserted the new one. The
/// atomicity is the whole point: the re-bind cannot survive a
/// crash between the model INSERT and the UPDATE here.
///
/// Matching is by `(provider_id, upstream_model_id)`. Only rows with
/// `model_row_id IS NULL` (the orphan state that `ON DELETE SET NULL`
/// used to leave behind) and `sub_combo_id IS NULL`
/// (flat targets only — sub-combo targets are out of scope, per the
/// spec) are candidates.
///
/// NOTE: Under the current schema (migration 000030, `ON DELETE CASCADE`),
/// combo_targets rows are cascade-deleted when their referenced model
/// is deleted, so orphan rows with `model_row_id IS NULL` no longer
/// exist in practice. This function is retained as dead code for
/// forward-compatibility in case the FK semantics change again.
///
/// Returns the number of rows updated. A row whose
/// `upstream_model_id` is `NULL` (because the orphan existed BEFORE
/// the 000026 migration ran, or because the operator created a
/// target without recording an upstream model id) is left alone —
/// it cannot be re-bound without manual intervention, exactly as
/// the spec documents.
///
/// `conn` is typed as `&Connection` because rusqlite's
/// `Transaction<'_>` derefs to `Connection` and `&mut Connection`
/// is what `unchecked_transaction()` returns; either caller shape
/// compiles against this signature.
pub fn reconnect_orphan_targets(
    conn: &Connection,
    provider: &ProviderId,
    upstream_model_id: &str,
    new_model_row_id: ModelRowId,
) -> Result<usize> {
    let updated = conn
        .execute(
            "UPDATE combo_targets \
             SET model_row_id = ?1 \
             WHERE provider_id = ?2 \
               AND upstream_model_id = ?3 \
               AND model_row_id IS NULL \
               AND sub_combo_id IS NULL",
            params![new_model_row_id.0, provider.as_str(), upstream_model_id],
        )
        .map_err(|e| CoreError::Database {
            message: format!(
                "execute reconnect_orphan_targets (provider={}, upstream={}, new_id={}): {}",
                provider, upstream_model_id, new_model_row_id.0, e
            ),
            source: Some(Box::new(e)),
        })?;
    Ok(updated)
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

    let mut current_level = vec![start_combo_id.0];

    for _ in 0..max_depth {
        if current_level.is_empty() {
            break;
        }

        let placeholders = vec!["?"; current_level.len()].join(",");
        let query = format!(
            "SELECT DISTINCT sub_combo_id FROM combo_targets \
             WHERE combo_id IN ({}) AND sub_combo_id IS NOT NULL",
            placeholders
        );

        let mut stmt = conn.prepare(&query).map_err(crate::error::map_db_error)?;

        let params_vec: Vec<&dyn rusqlite::ToSql> = current_level
            .iter()
            .map(|id| id as &dyn rusqlite::ToSql)
            .collect();

        let sub_ids: Vec<i64> = stmt
            .query_map(rusqlite::params_from_iter(params_vec), |r| {
                r.get::<_, Option<i64>>(0)
            })
            .map_err(crate::error::map_db_error)?
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

        current_level = sub_ids;
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
    // Orphan targets — rows where the upstream model was deleted,
    // leaving `(model_row_id IS NULL, sub_combo_id IS NULL)` — are
    // excluded. Under the current schema (ON DELETE CASCADE, migration
    // 000030) these rows no longer exist, but the filter is retained
    // as a safety net. Without this filter a surviving row would be
    // passed to `RoutingPlan::Combo` and then to `execute_single`,
    // which surfaces a confusing `5xx Internal: ... sub-combo target`
    // (Gate E3).
    let mut stmt = conn
        .prepare(
            "SELECT ct.id, ct.combo_id, ct.provider_id, ct.account_id, ct.model_row_id, \
                    ct.sub_combo_id, ct.priority_order, ct.weight, p.rate_limit_scope \
             FROM combo_targets ct \
             INNER JOIN providers p ON p.id = ct.provider_id \
             WHERE ct.combo_id = ?1 AND p.active = 1 \
                 AND NOT (ct.model_row_id IS NULL AND ct.sub_combo_id IS NULL) \
             ORDER BY ct.priority_order ASC, ct.id ASC",
        )
        .map_err(crate::error::map_db_error)?;
    let rows = stmt
        .query_map(params![combo_id.0], row_to_target)
        .map_err(crate::error::map_db_error)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(crate::error::map_db_error)?);
    }
    Ok(out)
}

/// Like [`list_targets`], but joins against the `models` table so the
/// caller gets the upstream model id and the optional human-readable
/// display name alongside the target's own columns. The order, the
/// "inactive providers are hidden" filter, and the `priority_order`
/// semantics are identical to [`list_targets`].
///
/// Used by the admin `GET /admin/combos/:id/targets` endpoint; the
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
    // The join against `providers` is a `LEFT JOIN` (not INNER) so that
    // targets with a deactivated provider are STILL returned. The
    // dashboard needs to see and manage all targets (including inactive
    // ones) so the operator can reorder them, delete them, or reactivate
    // the provider. The `provider_active` flag (from `p.active`) tells
    // the frontend which targets are currently routable.
    //
    // CRITICAL: the routing path (`list_targets` below) still uses
    // `INNER JOIN providers p ON p.id = ct.provider_id WHERE p.active = 1`
    // — only active targets are used for actual request routing. This
    // dashboard view is a SUPERSET (includes inactive-provider targets)
    // so the reorder validation (which operates on ALL combo_targets
    // rows) is consistent with what the dashboard shows.
    //
    // Without this fix, the GET returned [A, B] (excluding C whose
    // provider was inactive), the frontend sent `target_ids: [A, B]`,
    // but the reorder validation compared against [A, B, C] (all
    // combo_targets rows) → mismatch → 400 "target_ids must be a
    // permutation of the combo's current targets".
    //
    // The `LEFT JOIN models m` is retained (sub-combo targets have
    // `model_row_id = NULL`). The `LEFT JOIN combos sc` is retained
    // (for the sub-combo's name). The `LEFT JOIN target_cooldowns tc`
    // is retained (for the cooldown badge).
    let mut stmt = conn
        .prepare(
            "SELECT ct.id, ct.combo_id, ct.provider_id, ct.account_id, ct.model_row_id, \
                    ct.sub_combo_id, sc.name as sub_combo_name, \
                    COALESCE(m.model_id, ''), m.display_name, ct.priority_order, \
                    tc.cooldown_until, \
                    CASE WHEN tc.cooldown_until IS NOT NULL \
                              AND datetime(tc.cooldown_until) > datetime('now') \
                         THEN 1 ELSE 0 END as in_cooldown, \
                    tc.reason, \
                    m.context_length, \
                    m.max_output_tokens, \
                    ct.weight, \
                    COALESCE(p.active, 0) as provider_active \
             FROM combo_targets ct \
             LEFT JOIN providers p ON p.id = ct.provider_id \
             LEFT JOIN models m ON m.id = ct.model_row_id \
             LEFT JOIN combos sc ON sc.id = ct.sub_combo_id \
             LEFT JOIN target_cooldowns tc ON tc.combo_target_id = ct.id \
             WHERE ct.combo_id = ?1 \
             ORDER BY ct.priority_order ASC, ct.id ASC",
        )
        .map_err(crate::error::map_db_error)?;
    let rows = stmt
        .query_map(params![combo_id.0], row_to_target_with_model)
        .map_err(crate::error::map_db_error)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(crate::error::map_db_error)?);
    }
    Ok(out)
}

pub fn get_target(conn: &Connection, id: ComboTargetId) -> Result<Option<ComboTarget>> {
    let row = conn
        .query_row(
            "SELECT ct.id, ct.combo_id, ct.provider_id, ct.account_id, ct.model_row_id, \
                    ct.sub_combo_id, ct.priority_order, ct.weight, p.rate_limit_scope \
             FROM combo_targets ct \
             INNER JOIN providers p ON p.id = ct.provider_id \
             WHERE ct.id = ?1",
            params![id.0],
            row_to_target,
        )
        .optional()
        .map_err(crate::error::map_db_error_ctx(format!(
            "get combo_target {}",
            id.0
        )))?;
    Ok(row)
}

pub fn delete_target(conn: &Connection, id: ComboTargetId) -> Result<()> {
    conn.execute("DELETE FROM combo_targets WHERE id = ?1", params![id.0])
        .map_err(crate::error::map_db_error_ctx(format!(
            "delete combo_target {}",
            id.0
        )))?;
    Ok(())
}

/// Update the `priority_order` of a single target. Idempotent: a
/// missing row is a silent no-op, matching the existing convention.
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
        message: format!(
            "update priority_order for combo_target {}: {}",
            target_id.0, e
        ),
        source: Some(Box::new(e)),
    })?;
    Ok(())
}

/// Update the `weight` column of a single target. The column is
/// `INTEGER NOT NULL DEFAULT 1` (migration 000035); the dashboard
/// exposes this as an editable input next to each row in the combo
/// editor so the operator can dial in the relative weight used by
/// the `weighted` priority mode. Weights `<= 0` are rejected
/// because the weighted-random algorithm divides by the sum of
/// weights — a zero or negative sum is undefined behavior.
///
/// Idempotent: a missing row is a silent no-op (the UPDATE affects
/// 0 rows), matching [`update_target_priority`].
pub fn update_target_weight(
    conn: &Connection,
    target_id: ComboTargetId,
    weight: i32,
) -> Result<()> {
    if weight <= 0 {
        return Err(CoreError::Validation(format!(
            "weight must be a positive integer, got {}",
            weight
        )));
    }
    conn.execute(
        "UPDATE combo_targets SET weight = ?1 WHERE id = ?2",
        params![weight as i64, target_id.0],
    )
    .map_err(crate::error::map_db_error_ctx(format!(
        "update weight for combo_target {}",
        target_id.0
    )))?;
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
        .map_err(crate::error::map_db_error)?;

    // Pull the current target ids for this combo, scoped by `combo_id`
    // so a stray id from another combo can never sneak into the
    // validation set.
    let mut stmt = tx
        .prepare("SELECT id FROM combo_targets WHERE combo_id = ?1")
        .map_err(crate::error::map_db_error)?;
    let current: Vec<i64> = stmt
        .query_map(params![combo_id.0], |r| r.get::<_, i64>(0))
        .map_err(crate::error::map_db_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(crate::error::map_db_error)?;
    drop(stmt);

    // Multiset equality via sorted Vec<i64>: identical multisets
    // produce identical sorted lists, so this check catches missing
    // ids, duplicate ids, and extra ids all at once. The
    // "not belonging to this combo" case falls out for free because
    // the SELECT above is scoped by `combo_id`.
    let mut current_sorted = current.clone();
    current_sorted.sort();
    let mut incoming: Vec<i64> = ordered_ids.iter().map(|i| i.0).collect();
    incoming.sort();
    if current_sorted != incoming {
        return Err(CoreError::Validation(
            "target_ids must be a permutation of the combo's current targets".into(),
        ));
    }

    // Assign priority_order = 1, 2, 3, ... in the order received. The
    // `combo_id = ?3` guard is what closes the cross-combo rename
    // hole even if the validation above is ever loosened.
    {
        let mut stmt = tx
            .prepare("UPDATE combo_targets SET priority_order = ?1 WHERE id = ?2 AND combo_id = ?3")
            .map_err(crate::error::map_db_error)?;
        for (idx, tid) in ordered_ids.iter().enumerate() {
            stmt.execute(params![(idx as i32) + 1, tid.0, combo_id.0])
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
    }
    tx.commit().map_err(crate::error::map_db_error)?;
    Ok(())
}

/// Update mutable fields of a combo. Currently only `race_size` is
/// supported; passing `None` leaves the existing value untouched. The
/// `1..=8` CHECK constraint from migration 000004 is enforced by SQLite.
pub fn update_combo(conn: &Connection, id: ComboId, race_size: Option<u8>) -> Result<()> {
    if let Some(rs) = race_size {
        if !(1..=8).contains(&rs) {
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

/// Update the `context_window` override for a combo. `None` means
/// "auto-compute from targets" (the default). `Some(n)` pins the
/// reported context window to `n` tokens.
pub fn update_context_window(
    conn: &Connection,
    id: ComboId,
    context_window: Option<i64>,
) -> Result<()> {
    let affected = conn
        .execute(
            "UPDATE combos SET context_window = ?1 WHERE id = ?2",
            params![context_window, id.0],
        )
        .map_err(crate::error::map_db_error_ctx(format!(
            "update context_window for combo {}",
            id.0
        )))?;
    if affected == 0 {
        return Err(CoreError::ComboNotFound(id.0));
    }
    Ok(())
}

/// Update the `priority_mode` of a combo. `None` clears the column
/// back to `NULL`, which [`PriorityMode::from_db`] interprets as
/// `Strict` (the legacy walk). A non-`None` string is parsed via
/// [`PriorityMode::parse`]; an unknown value surfaces as
/// [`CoreError::Validation`].
///
/// The mode is only consulted when the combo's `strategy` is
/// [`Strategy::Priority`]; for `RoundRobin` and `Shuffle` the
/// column is stored but ignored. We don't reject the call in those
/// cases so the operator can flip the strategy back to `Priority`
/// later without losing the mode they configured.
pub fn update_priority_mode(conn: &Connection, id: ComboId, mode: Option<&str>) -> Result<()> {
    let value: Option<String> = match mode {
        None => None,
        Some(s) => {
            // Validate the string before persisting so a typo doesn't
            // land in the DB only to surface as `Strict` on the next
            // read (silently masking the misconfiguration).
            let parsed = PriorityMode::parse(s)?;
            Some(parsed.as_str().to_string())
        }
    };
    let affected = conn
        .execute(
            "UPDATE combos SET priority_mode = ?1 WHERE id = ?2",
            params![value, id.0],
        )
        .map_err(crate::error::map_db_error_ctx(format!(
            "update priority_mode for combo {}",
            id.0
        )))?;
    if affected == 0 {
        return Err(CoreError::ComboNotFound(id.0));
    }
    Ok(())
}

/// Update the cooldown-related settings on a combo. All four
/// parameters are written in a single UPDATE so the dashboard's
/// "Cooldown" form can POST them atomically; passing `None` for any
/// individual field clears that column back to `NULL` (which the
/// pipeline interprets as "use the global `[cooldown]` default").
///
/// `mode` is parsed via [`CooldownMode::parse`]; an unknown value
/// surfaces as [`CoreError::Validation`]. `base`, `max`, and
/// `factor` are passed through as raw integers because they are
/// only meaningful when the operator picks the matching mode —
/// the pipeline's `record_failure_with_mode` does the final
/// "override or fall back to global config" resolution.
pub fn update_cooldown_settings(
    conn: &Connection,
    id: ComboId,
    mode: Option<&str>,
    base: Option<u64>,
    max: Option<u64>,
    factor: Option<u32>,
) -> Result<()> {
    let mode_value: Option<String> = match mode {
        None => None,
        Some(s) => {
            let parsed = CooldownMode::parse(s)?;
            Some(parsed.as_str().to_string())
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
                factor.map(|v| v as i64),
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

/// Update ONLY the cooldown_mode column, leaving base/max/factor
/// untouched. This is the per-field update used by the dashboard's
/// individual cooldown setting inputs.
pub fn update_cooldown_mode(conn: &Connection, id: ComboId, mode: Option<&str>) -> Result<()> {
    let mode_value: Option<String> = match mode {
        None => None,
        Some(s) => {
            let parsed = CooldownMode::parse(s)?;
            Some(parsed.as_str().to_string())
        }
    };
    let affected = conn
        .execute(
            "UPDATE combos SET cooldown_mode = ?1 WHERE id = ?2",
            params![mode_value, id.0],
        )
        .map_err(crate::error::map_db_error_ctx(format!(
            "update cooldown_mode for combo {}",
            id.0
        )))?;
    if affected == 0 {
        return Err(CoreError::ComboNotFound(id.0));
    }
    Ok(())
}

/// Update ONLY the cooldown_base_secs column.
pub fn update_cooldown_base(conn: &Connection, id: ComboId, base: Option<u64>) -> Result<()> {
    let affected = conn
        .execute(
            "UPDATE combos SET cooldown_base_secs = ?1 WHERE id = ?2",
            params![base.map(|v| v as i64), id.0],
        )
        .map_err(crate::error::map_db_error_ctx(format!(
            "update cooldown_base_secs for combo {}",
            id.0
        )))?;
    if affected == 0 {
        return Err(CoreError::ComboNotFound(id.0));
    }
    Ok(())
}

/// Update ONLY the cooldown_max_secs column.
pub fn update_cooldown_max(conn: &Connection, id: ComboId, max: Option<u64>) -> Result<()> {
    let affected = conn
        .execute(
            "UPDATE combos SET cooldown_max_secs = ?1 WHERE id = ?2",
            params![max.map(|v| v as i64), id.0],
        )
        .map_err(crate::error::map_db_error_ctx(format!(
            "update cooldown_max_secs for combo {}",
            id.0
        )))?;
    if affected == 0 {
        return Err(CoreError::ComboNotFound(id.0));
    }
    Ok(())
}

/// Update ONLY the cooldown_factor column.
pub fn update_cooldown_factor(conn: &Connection, id: ComboId, factor: Option<u32>) -> Result<()> {
    let affected = conn
        .execute(
            "UPDATE combos SET cooldown_factor = ?1 WHERE id = ?2",
            params![factor.map(|v| v as i64), id.0],
        )
        .map_err(crate::error::map_db_error_ctx(format!(
            "update cooldown_factor for combo {}",
            id.0
        )))?;
    if affected == 0 {
        return Err(CoreError::ComboNotFound(id.0));
    }
    Ok(())
}

/// Update the LKGP exploration rate. `None` clears the column back
/// to `NULL`, which the pipeline interprets as the default 0.1
/// (10%). A non-`None` value must be in `[0.0, 1.0]`; outside that
/// range surfaces as [`CoreError::Validation`].
///
/// Only meaningful when `priority_mode = Lkgp`; the column is
/// stored unconditionally so the operator can switch modes without
/// losing the configured rate.
pub fn update_lkgp_settings(
    conn: &Connection,
    id: ComboId,
    exploration_rate: Option<f64>,
) -> Result<()> {
    if let Some(rate) = exploration_rate
        && !(0.0..=1.0).contains(&rate)
    {
        return Err(CoreError::Validation(format!(
            "lkgp_exploration_rate must be in [0.0, 1.0], got {}",
            rate
        )));
    }
    let affected = conn
        .execute(
            "UPDATE combos SET lkgp_exploration_rate = ?1 WHERE id = ?2",
            params![exploration_rate, id.0],
        )
        .map_err(crate::error::map_db_error_ctx(format!(
            "update lkgp_exploration_rate for combo {}",
            id.0
        )))?;
    if affected == 0 {
        return Err(CoreError::ComboNotFound(id.0));
    }
    Ok(())
}

/// Update the selection window (in seconds) used by the `least_used`
/// and `p2c` priority modes. `None` clears the column back to `NULL`,
/// which the pipeline interprets as the default 3600 (1 hour).
///
/// Only meaningful when `priority_mode` is `LeastUsed` or `P2c`; the
/// column is stored unconditionally so the operator can switch modes
/// without losing the configured window.
pub fn update_selection_window(
    conn: &Connection,
    id: ComboId,
    window_secs: Option<u64>,
) -> Result<()> {
    let affected = conn
        .execute(
            "UPDATE combos SET selection_window_secs = ?1 WHERE id = ?2",
            params![window_secs.map(|v| v as i64), id.0],
        )
        .map_err(crate::error::map_db_error_ctx(format!(
            "update selection_window_secs for combo {}",
            id.0
        )))?;
    if affected == 0 {
        return Err(CoreError::ComboNotFound(id.0));
    }
    Ok(())
}

/// Compute the effective context window for a combo. If the combo has
/// an explicit `context_window` override, return that. Otherwise,
/// recursively compute the minimum `context_length` across all targets
/// (including sub-combo targets, resolved transitively).
///
/// Sub-combo targets are resolved recursively: if combo A contains
/// sub-combo B, and B has an explicit override, that override is used;
/// otherwise B's targets are recursed into. A cycle guard prevents
/// infinite loops (returns `None` if a cycle is detected).
///
/// Returns `None` if:
/// - The combo has no targets.
/// - No target has a known `context_length`.
/// - A cycle is detected among sub-combos.
fn row_to_combo(row: &Row<'_>) -> rusqlite::Result<Combo> {
    let id: i64 = row.get(0)?;
    let name: String = row.get(1)?;
    let strategy_str: String = row.get(2)?;
    let race_size: i64 = row.get(3)?;
    let created_at: String = row.get(4)?;
    // Column 5 is `context_window` (added by migration 000034). Older
    // rows / older databases that haven't run the migration yet get
    // NULL → `None` → auto-compute.
    let context_window: Option<i64> = row.get(5).ok().flatten();
    // Columns 6-12 (migration 000035): priority / cooldown knobs.
    // All nullable; `NULL` reads back as the legacy default via the
    // `from_db` helpers.
    let priority_mode_str: Option<String> = row.get(6).ok().flatten();
    let cooldown_mode_str: Option<String> = row.get(7).ok().flatten();
    let cooldown_base_secs: Option<i64> = row.get(8).ok().flatten();
    let cooldown_max_secs: Option<i64> = row.get(9).ok().flatten();
    let cooldown_factor: Option<i64> = row.get(10).ok().flatten();
    let lkgp_exploration_rate: Option<f64> = row.get(11).ok().flatten();
    let selection_window_secs: Option<i64> = row.get(12).ok().flatten();

    let strategy = Strategy::parse(&strategy_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            2,
            rusqlite::types::Type::Text,
            Box::new(FromStrError(format!("{}", e))),
        )
    })?;

    if !(1..=8).contains(&race_size) {
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
        context_window,
        priority_mode: PriorityMode::from_db(priority_mode_str.as_deref()),
        cooldown_mode: CooldownMode::from_db(cooldown_mode_str.as_deref()),
        cooldown_base_secs: cooldown_base_secs.map(|v| v as u64),
        cooldown_max_secs: cooldown_max_secs.map(|v| v as u64),
        cooldown_factor: cooldown_factor.map(|v| v as u32),
        lkgp_exploration_rate,
        selection_window_secs: selection_window_secs.map(|v| v as u64),
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
    // Column 7 (migration 000035): per-target weight. The column is
    // `INTEGER NOT NULL DEFAULT 1`; we still defend against NULL
    // with `unwrap_or(1)` so a hand-rolled SELECT that drops the
    // `NOT NULL` guarantee (or a row inserted before the migration
    // backfilled defaults) does not poison the routing layer.
    let weight: i32 = row.get::<_, Option<i64>>(7)?.unwrap_or(1) as i32;
    let rate_limit_scope: String = row.get(8)?;

    Ok(ComboTarget {
        id: ComboTargetId(id),
        combo_id: ComboId(combo_id),
        provider_id: ProviderId::new(provider_id),
        account_id: account_id.map(AccountId),
        model_row_id: model_row_id.map(ModelRowId),
        sub_combo_id: sub_combo_id.map(ComboId),
        priority_order,
        weight,
        rate_limit_scope: crate::providers::RateLimitScope::parse(&rate_limit_scope).unwrap_or_default(),
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
    // Columns 13-14: model context_length + max_output_tokens from
    // the `LEFT JOIN models m`. `None` for sub-combo targets or
    // models without metadata.
    let context_length: Option<i64> = row.get(13)?;
    let max_output_tokens: Option<i64> = row.get(14)?;
    // Column 15 (migration 000035): per-target weight.
    let weight: i32 = row.get::<_, Option<i64>>(15)?.unwrap_or(1) as i32;
    // Column 16: `provider_active` from `COALESCE(p.active, 0)`. This
    // is `0` when the provider was deactivated (or the LEFT JOIN
    // didn't match — which shouldn't happen because `provider_id` is
    // NOT NULL, but COALESCE defends against it anyway).
    let provider_active: i64 = row.get(16)?;

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
        weight,
        in_cooldown: in_cooldown != 0,
        cooldown_until,
        cooldown_reason,
        context_length,
        max_output_tokens,
        provider_active: provider_active != 0,
    })
}

#[derive(Debug)]
pub struct FromStrError(String);
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
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;

    fn fresh_pool() -> (DbPool, PathBuf) {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("openproxy-crud-test-{}-{}-{}", pid, nanos, n));
        std::fs::create_dir_all(&dir).expect("mkdir tempdir");
        let path = dir.join("crud.db");
        let pool = DbPool::open(&path).expect("open pool");
        {
            let mut w = pool.writer();
            migrations::run(&mut w).expect("migrations");
        }
        (pool, path)
    }

    #[test]
    fn test_create_combo_success() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();

        let combo_name = "test_combo_1";
        let strategy = Strategy::Priority;
        let race_size = 2;

        let combo_id =
            create_combo(&conn, combo_name, strategy, race_size).expect("create combo failed");

        let combo = get_combo(&conn, combo_id)
            .expect("get combo failed")
            .expect("combo not found");

        assert_eq!(combo.name, combo_name);
        assert_eq!(combo.strategy, strategy);
        assert_eq!(combo.race_size, race_size);
    }

    #[test]
    fn test_create_combo_invalid_race_size() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();

        let combo_name = "test_combo_2";
        let strategy = Strategy::RoundRobin;

        // Race size 0 is invalid
        let err = create_combo(&conn, combo_name, strategy, 0)
            .expect_err("create combo should fail with race size 0");
        match err {
            CoreError::Validation(msg) => assert!(msg.contains("race_size must be in 1..=8")),
            _ => panic!("Expected Validation error, got {:?}", err),
        }

        // Race size 9 is invalid
        let err2 = create_combo(&conn, combo_name, strategy, 9)
            .expect_err("create combo should fail with race size 9");
        match err2 {
            CoreError::Validation(msg) => assert!(msg.contains("race_size must be in 1..=8")),
            _ => panic!("Expected Validation error, got {:?}", err2),
        }
    }

    #[test]
    fn test_create_combo_duplicate_name() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();

        let combo_name = "test_combo_dup";
        let strategy = Strategy::Priority;
        let race_size = 1;

        let _combo_id =
            create_combo(&conn, combo_name, strategy, race_size).expect("create combo failed");

        // Attempting to create a combo with the same name should fail
        let err = create_combo(&conn, combo_name, strategy, race_size)
            .expect_err("create duplicate combo should fail");

        match err {
            CoreError::Validation(msg) => assert!(msg.contains("combo name already exists")),
            _ => panic!(
                "Expected Validation error for duplicate name, got {:?}",
                err
            ),
        }
    }

    #[test]
    fn test_create_combo_invalid_strategy() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();

        let combo_name = "test_combo_shuffle";
        // Strategy::Shuffle is not permitted by DB constraint for combos.
        let strategy = Strategy::Shuffle;
        let race_size = 1;

        let err = create_combo(&conn, combo_name, strategy, race_size)
            .expect_err("create combo with shuffle strategy should fail");

        // The error will be mapped to a database error due to CHECK constraint violation
        if let CoreError::Database { message, source } = &err {
            assert!(message.contains("insert combo"));
            assert!(
                source
                    .as_ref()
                    .unwrap()
                    .to_string()
                    .contains("CHECK constraint failed")
            );
        } else {
            panic!(
                "Expected Database error with CHECK constraint failure, got {:?}",
                err
            );
        }
    }

    #[test]
    fn test_combo_in_chain_no_cycle() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();

        // C1 -> C2 -> C3
        let c1 = ComboId(1);
        let _c2 = ComboId(2);
        let c3 = ComboId(3);

        conn.execute_batch(
            "
            INSERT INTO providers (id, name, base_url, auth_type, format) VALUES ('p1', 'P1', 'url', 'bearer', 'openai');
            INSERT INTO combos (id, name, strategy) VALUES (1, 'c1', 'priority');
            INSERT INTO combos (id, name, strategy) VALUES (2, 'c2', 'priority');
            INSERT INTO combos (id, name, strategy) VALUES (3, 'c3', 'priority');

            INSERT INTO combo_targets (combo_id, provider_id, sub_combo_id, priority_order)
            VALUES (1, 'p1', 2, 1);

            INSERT INTO combo_targets (combo_id, provider_id, sub_combo_id, priority_order)
            VALUES (2, 'p1', 3, 1);
            "
        ).expect("insert test data");

        // checking if C1 is in C1's subchain?
        // start = C1, target = C1 -> but since current logic is `start == target` => true
        let result = combo_in_chain(&conn, c1, c3, MAX_SUB_COMBO_DEPTH).expect("query success");
        assert!(!result);
    }

    #[test]
    fn test_combo_in_chain_has_cycle() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();

        // C1 -> C2 -> C3 -> C1
        let c1 = ComboId(1);
        let _c2 = ComboId(2);
        let _c3 = ComboId(3);

        conn.execute_batch(
            "
            INSERT INTO providers (id, name, base_url, auth_type, format) VALUES ('p1', 'P1', 'url', 'bearer', 'openai');
            INSERT INTO combos (id, name, strategy) VALUES (1, 'c1', 'priority');
            INSERT INTO combos (id, name, strategy) VALUES (2, 'c2', 'priority');
            INSERT INTO combos (id, name, strategy) VALUES (3, 'c3', 'priority');

            INSERT INTO combo_targets (combo_id, provider_id, sub_combo_id, priority_order)
            VALUES (1, 'p1', 2, 1);

            INSERT INTO combo_targets (combo_id, provider_id, sub_combo_id, priority_order)
            VALUES (2, 'p1', 3, 1);

            INSERT INTO combo_targets (combo_id, provider_id, sub_combo_id, priority_order)
            VALUES (3, 'p1', 1, 1);
            "
        ).expect("insert test data");

        // checking if C1 is in C2's chain -> since C2 -> C3 -> C1
        let result = combo_in_chain(&conn, c1, c1, MAX_SUB_COMBO_DEPTH).expect("query success");
        assert!(result); // start == target

        let result2 =
            combo_in_chain(&conn, c1, ComboId(2), MAX_SUB_COMBO_DEPTH).expect("query success");
        assert!(result2);
    }

    #[test]
    fn test_combo_in_chain_start_equals_target() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();

        let c1 = ComboId(1);

        let result = combo_in_chain(&conn, c1, c1, MAX_SUB_COMBO_DEPTH).expect("query success");
        assert!(result);
    }

    #[test]
    fn test_combo_in_chain_max_depth_exceeded() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();

        // C1 -> C2 -> C3 -> C4 -> C5
        let c5 = ComboId(5);
        let c1 = ComboId(1);

        conn.execute_batch(
            "
            INSERT INTO providers (id, name, base_url, auth_type, format) VALUES ('p1', 'P1', 'url', 'bearer', 'openai');
            INSERT INTO combos (id, name, strategy) VALUES (1, 'c1', 'priority');
            INSERT INTO combos (id, name, strategy) VALUES (2, 'c2', 'priority');
            INSERT INTO combos (id, name, strategy) VALUES (3, 'c3', 'priority');
            INSERT INTO combos (id, name, strategy) VALUES (4, 'c4', 'priority');
            INSERT INTO combos (id, name, strategy) VALUES (5, 'c5', 'priority');

            INSERT INTO combo_targets (combo_id, provider_id, sub_combo_id, priority_order)
            VALUES (1, 'p1', 2, 1);
            INSERT INTO combo_targets (combo_id, provider_id, sub_combo_id, priority_order)
            VALUES (2, 'p1', 3, 1);
            INSERT INTO combo_targets (combo_id, provider_id, sub_combo_id, priority_order)
            VALUES (3, 'p1', 4, 1);
            INSERT INTO combo_targets (combo_id, provider_id, sub_combo_id, priority_order)
            VALUES (4, 'p1', 5, 1);
            "
        ).expect("insert test data");

        // Depth 3 allows reaching up to 3 links down. C1 -> (C2)1 -> (C3)2 -> (C4)3 -> C5 (4th)
        let result = combo_in_chain(&conn, c5, c1, 3).expect("query success");
        assert!(!result);

        // Depth 4 should reach C5
        let result2 = combo_in_chain(&conn, c5, c1, 4).expect("query success");
        assert!(result2);
    }

    #[test]
    fn test_combo_in_chain_mutual_cycle() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();

        // C1 -> C2 -> C1 (cycle)
        // We want to verify that cycle detection works correctly and returns true/false
        // as appropriate without looping infinitely.
        let c1 = ComboId(1);
        let c2 = ComboId(2);
        let c3 = ComboId(3); // Not in the cycle

        conn.execute_batch(
            "
            INSERT INTO providers (id, name, base_url, auth_type, format) VALUES ('p1', 'P1', 'url', 'bearer', 'openai');
            INSERT INTO combos (id, name, strategy) VALUES (1, 'c1', 'priority');
            INSERT INTO combos (id, name, strategy) VALUES (2, 'c2', 'priority');
            INSERT INTO combos (id, name, strategy) VALUES (3, 'c3', 'priority');

            -- C1 points to C2
            INSERT INTO combo_targets (combo_id, provider_id, sub_combo_id, priority_order)
            VALUES (1, 'p1', 2, 1);

            -- C2 points to C1
            INSERT INTO combo_targets (combo_id, provider_id, sub_combo_id, priority_order)
            VALUES (2, 'p1', 1, 1);
            "
        ).expect("insert test data");

        // Verify C2 is in C1's chain
        let result_c2_in_c1 = combo_in_chain(&conn, c2, c1, 10).expect("query success");
        assert!(result_c2_in_c1, "C1 points to C2, so C2 is in C1's chain");

        // Verify C1 is in C2's chain
        let result_c1_in_c2 = combo_in_chain(&conn, c1, c2, 10).expect("query success");
        assert!(result_c1_in_c2, "C2 points to C1, so C1 is in C2's chain");

        // Verify that searching for a non-existent combo (C3) safely terminates
        // and returns false despite the cycle.
        let result_c3_in_c1 = combo_in_chain(&conn, c3, c1, 10).expect("query success");
        assert!(
            !result_c3_in_c1,
            "C3 is not in the cycle, so it should return false"
        );
    }

    #[test]
    fn test_combo_in_chain_multi_branch() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();

        // C1 -> C2 (priority 1)
        // C1 -> C3 (priority 2)
        // C3 -> C4 (priority 1)
        let c1 = ComboId(1);
        let c4 = ComboId(4);

        conn.execute_batch(
            "
            INSERT INTO providers (id, name, base_url, auth_type, format) VALUES ('p1', 'P1', 'url', 'bearer', 'openai');
            INSERT INTO combos (id, name, strategy) VALUES (1, 'c1', 'priority');
            INSERT INTO combos (id, name, strategy) VALUES (2, 'c2', 'priority');
            INSERT INTO combos (id, name, strategy) VALUES (3, 'c3', 'priority');
            INSERT INTO combos (id, name, strategy) VALUES (4, 'c4', 'priority');

            INSERT INTO combo_targets (combo_id, provider_id, sub_combo_id, priority_order)
            VALUES (1, 'p1', 2, 1);
            INSERT INTO combo_targets (combo_id, provider_id, sub_combo_id, priority_order)
            VALUES (1, 'p1', 3, 2);

            INSERT INTO combo_targets (combo_id, provider_id, sub_combo_id, priority_order)
            VALUES (3, 'p1', 4, 1);
            "
        ).expect("insert test data");

        // The logic must be able to explore C3 (second branch) and find C4.
        let result = combo_in_chain(&conn, c4, c1, 10).expect("query success");
        assert!(result, "C1 should be able to reach C4 via C3");
    }
}
