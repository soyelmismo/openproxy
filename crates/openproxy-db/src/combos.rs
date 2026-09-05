use openproxy_types::ProviderId;
use openproxy_types::combos::{
    Combo, ComboTarget, ComboTargetWithModel, MAX_SUB_COMBO_DEPTH, PriorityMode, Strategy,
};
use openproxy_types::config::CooldownMode;
use openproxy_types::error::CoreError;
use openproxy_types::error::Result;
use openproxy_types::ids::{AccountId, ComboId, ComboTargetId, ModelRowId};
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

crate::def_table_select!(
    combo_select,
    "combos",
    "id, name, strategy, race_size, created_at, context_window, \
     priority_mode, cooldown_mode, cooldown_base_secs, cooldown_max_secs, \
     cooldown_factor, lkgp_exploration_rate, selection_window_secs, \
     COALESCE(preventive_rate_limit, 0)"
);

crate::def_table_select!(
    combo_target_select,
    "combo_targets ct INNER JOIN providers p ON p.id = ct.provider_id",
    "ct.id, ct.combo_id, ct.provider_id, ct.account_id, ct.model_row_id, \
     ct.sub_combo_id, ct.priority_order, ct.weight, p.rate_limit_scope, ct.active, \
     ct.cooldown_mode, ct.cooldown_base_secs, ct.cooldown_max_secs, ct.cooldown_factor"
);

crate::def_table_select!(
    combo_target_with_model_select,
    "combo_targets ct \
     LEFT JOIN providers p ON p.id = ct.provider_id \
     LEFT JOIN models m ON m.id = ct.model_row_id \
     LEFT JOIN combos sc ON sc.id = ct.sub_combo_id \
     LEFT JOIN target_cooldowns tc ON tc.combo_target_id = ct.id",
    "ct.id, ct.combo_id, ct.provider_id, ct.account_id, ct.model_row_id, \
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
     COALESCE(p.active, 0) as provider_active, \
     ct.active, \
     ct.cooldown_mode, \
     ct.cooldown_base_secs, \
     ct.cooldown_max_secs, \
     ct.cooldown_factor"
);

crate::def_table_select!(model_provider_id_select, "models", "provider_id");

crate::def_table_select!(model_upstream_id_select, "models", "model_id");

crate::def_table_select!(model_context_length_select, "models", "context_length");

crate::def_table_select!(combo_context_window_select, "combos", "context_window");

crate::def_table_select!(combo_target_ids_select, "combo_targets", "id");

crate::def_table_select!(
    combo_target_model_sub_select,
    "combo_targets ct",
    "ct.model_row_id, ct.sub_combo_id"
);

crate::def_table_select!(account_healthy_ids_select, "accounts", "id");

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
///
/// Used by the routing layer: a chat request whose `model` field matches
/// `combo:<name>` is dispatched to the combo with `name = <name>`. The
/// match is case-sensitive to match how the names are stored and surfaced
/// in the admin / `/v1/models` endpoints.
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

fn validate_flat_target(
    conn: &Connection,
    model_row_id: ModelRowId,
    provider_id: &ProviderId,
) -> Result<()> {
    let model_exists = crate::db_exists!(
        conn,
        "models",
        WHERE id = model_row_id.0,
        format!("check model {} exists", model_row_id.0)
    )?;
    if !model_exists {
        return Err(CoreError::Validation(format!(
            "model_row_id does not exist: {}",
            model_row_id.0
        )));
    }

    let model_provider: String = conn
        .query_row(
            model_provider_id_select!("WHERE id = ?1"),
            params![model_row_id.0],
            |r| r.get::<_, String>(0),
        )
        .map_err(|e| {
            crate::error::map_db_error_ctx(format!("read model {} provider_id", model_row_id.0))(e)
        })?;

    if model_provider != provider_id.as_str() {
        return Err(CoreError::Validation(format!(
            "model {} belongs to provider '{}', not '{}'",
            model_row_id.0, model_provider, provider_id
        )));
    }

    Ok(())
}

fn check_sub_combo_cycle(conn: &Connection, combo_id: ComboId, sub_id: ComboId) -> Result<()> {
    if combo_in_chain(conn, combo_id, sub_id, MAX_SUB_COMBO_DEPTH)? {
        return Err(CoreError::Validation(format!(
            "adding sub-combo {} to combo {} would create a cycle",
            sub_id.0, combo_id.0
        )));
    }
    Ok(())
}

fn validate_sub_combo_target(conn: &Connection, combo_id: ComboId, sub_id: ComboId) -> Result<()> {
    if sub_id == combo_id {
        return Err(CoreError::Validation("combo cannot contain itself".into()));
    }
    let sub_exists = crate::db_exists!(
        conn,
        "combos",
        WHERE id = sub_id.0,
        format!("check sub-combo {} exists", sub_id.0)
    )?;
    if !sub_exists {
        return Err(CoreError::Validation(format!(
            "sub_combo_id does not exist: {}",
            sub_id.0
        )));
    }
    check_sub_combo_cycle(conn, combo_id, sub_id)
}

fn validate_account(
    conn: &Connection,
    account_id: Option<AccountId>,
    model_row_id: Option<ModelRowId>,
) -> Result<()> {
    if let Some(aid) = account_id {
        if model_row_id.is_none() {
            return Err(CoreError::Validation(
                "account_id is only valid on flat (model) targets".into(),
            ));
        }
        let account_exists = crate::db_exists!(
            conn,
            "accounts",
            WHERE id = aid.0,
            format!("check account {} exists", aid.0)
        )?;
        if !account_exists {
            return Err(CoreError::AccountNotFound(aid.0));
        }
    }

    Ok(())
}

fn fetch_upstream_model_id(
    conn: &Connection,
    model_row_id: Option<ModelRowId>,
) -> Result<Option<String>> {
    if let Some(mrid) = model_row_id {
        let upstream_id: String = conn
            .query_row(
                model_upstream_id_select!("WHERE id = ?1"),
                params![mrid.0],
                |r| r.get::<_, String>(0),
            )
            .map_err(|e| {
                crate::error::map_db_error_ctx(format!("read model {} upstream model_id", mrid.0))(
                    e,
                )
            })?;
        Ok(Some(upstream_id))
    } else {
        Ok(None)
    }
}

fn check_duplicate_target(conn: &Connection, input: &AddTargetInput) -> Result<()> {
    let target_exists: bool = crate::db_exists!(
        conn,
        "SELECT EXISTS( \
         SELECT 1 FROM combo_targets \
         WHERE combo_id = ?1 \
           AND provider_id = ?2 \
           AND COALESCE(account_id, -1) = COALESCE(?3, -1) \
           AND COALESCE(model_row_id, -1) = COALESCE(?4, -1) \
           AND COALESCE(sub_combo_id, -1) = COALESCE(?5, -1))",
        params![
            input.combo_id.0,
            input.provider_id.as_str(),
            input.account_id.map(|a| a.0),
            input.model_row_id.map(|m| m.0),
            input.sub_combo_id.map(|c| c.0),
        ],
        "check target exists"
    )
    .unwrap_or(false);

    if target_exists {
        return Err(CoreError::Validation(format!(
            "duplicate target for combo {} (provider={}, account={:?}, model={:?}, sub_combo={:?})",
            input.combo_id.0,
            input.provider_id,
            input.account_id,
            input.model_row_id,
            input.sub_combo_id
        )));
    }
    Ok(())
}

fn validate_combo_exists(conn: &Connection, combo_id: ComboId) -> Result<()> {
    let combo_exists = crate::db_exists!(
        conn,
        "combos",
        WHERE id = combo_id.0,
        format!("check combo {} exists", combo_id.0)
    )?;
    if !combo_exists {
        return Err(CoreError::ComboNotFound(combo_id.0));
    }
    Ok(())
}

fn validate_add_target(conn: &Connection, input: &AddTargetInput) -> Result<()> {
    if input.model_row_id.is_some() == input.sub_combo_id.is_some() {
        return Err(CoreError::Validation(
            "must provide exactly one of model_row_id or sub_combo_id".into(),
        ));
    }

    validate_combo_exists(conn, input.combo_id)?;

    if let Some(model_row_id) = input.model_row_id {
        validate_flat_target(conn, model_row_id, &input.provider_id)?;
    }

    if let Some(sub_id) = input.sub_combo_id {
        validate_sub_combo_target(conn, input.combo_id, sub_id)?;
    }

    validate_account(conn, input.account_id, input.model_row_id)?;
    check_duplicate_target(conn, input)
}

fn map_add_target_error(input: &AddTargetInput, err: rusqlite::Error) -> CoreError {
    match crate::error::classify_sqlite_error(&err) {
        crate::error::DbErrorKind::ForeignKeyViolation => CoreError::Validation(format!(
            "provider_id or sub_combo_id does not exist: {}",
            input.provider_id
        )),
        crate::error::DbErrorKind::UniqueViolation => CoreError::Validation(format!(
            "duplicate target for combo {} (provider={}, account={:?}, model={:?}, sub_combo={:?})",
            input.combo_id.0,
            input.provider_id,
            input.account_id,
            input.model_row_id,
            input.sub_combo_id
        )),
        _ => crate::error::map_db_error_ctx("insert combo_target")(err),
    }
}

pub fn add_target(conn: &Connection, input: AddTargetInput) -> Result<ComboTargetId> {
    validate_add_target(conn, &input)?;

    // Look up the upstream `model_id` from the `models` table so we
    // can stamp it onto `combo_targets.upstream_model_id` (Gate F1).
    let upstream_model_id = fetch_upstream_model_id(conn, input.model_row_id)?;

    conn.execute(
        "INSERT INTO combo_targets(combo_id, provider_id, account_id, model_row_id, sub_combo_id, upstream_model_id, priority_order) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            input.combo_id.0,
            input.provider_id.as_str(),
            input.account_id.map(|a| a.0),
            input.model_row_id.map(|m| m.0),
            input.sub_combo_id.map(|c| c.0),
            upstream_model_id,
            input.priority_order,
        ],
    )
    .map_err(|e| map_add_target_error(&input, e))?;

    Ok(ComboTargetId(conn.last_insert_rowid()))
}

/// Gate F1: re-bind orphaned `combo_targets` rows that referenced a
/// vanished upstream model.
///
/// This helper is the heart of the reconnect path. The call shape is
/// `reconnect_orphan_targets(conn, provider, upstream_model_id,
/// new_model_row_id)` and is intended to be called from
/// [`openproxy_types::models::upsert_many`] *inside the same transaction* that
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
            source: Some(std::sync::Arc::new(e)),
        })?;
    Ok(updated)
}

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
fn fetch_sub_combo_ids(conn: &Connection, current_level: &[i64]) -> Result<Vec<i64>> {
    let json_arr = serde_json::to_string(current_level).map_err(crate::error::map_db_error_ctx(
        "Failed to serialize current_level",
    ))?;
    let query = "SELECT DISTINCT sub_combo_id FROM combo_targets \
                 WHERE combo_id IN (SELECT value FROM json_each(?)) AND sub_combo_id IS NOT NULL";

    let mut stmt = conn.prepare(query).map_err(crate::error::map_db_error)?;

    let sub_ids: Vec<i64> = stmt
        .query_map([json_arr], |r| r.get::<_, Option<i64>>(0))
        .map_err(crate::error::map_db_error)?
        .filter_map(|x| x.ok().flatten())
        .collect();

    Ok(sub_ids)
}

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

        let sub_ids = fetch_sub_combo_ids(conn, &current_level)?;
        if sub_ids.contains(&target_combo_id.0) {
            return Ok(true);
        }

        current_level = sub_ids;
    }
    Ok(false)
}

pub fn list_targets(conn: &Connection, combo_id: ComboId) -> Result<Vec<ComboTarget>> {
    // Targets whose provider has been deactivated (active = 0) or are
    // in active cooldown in `target_cooldowns` (cooldown_until > now)
    // are excluded from the routable result.
    crate::db_query_all!(
        conn,
        combo_target_select!(
            "LEFT JOIN target_cooldowns tc ON tc.combo_target_id = ct.id \
             WHERE ct.combo_id = ?1 AND p.active = 1 AND ct.active = 1 \
                 AND (tc.cooldown_until IS NULL OR datetime(tc.cooldown_until) <= datetime('now')) \
                 AND NOT (ct.model_row_id IS NULL AND ct.sub_combo_id IS NULL) \
             ORDER BY ct.priority_order ASC, ct.id ASC"
        ),
        params![combo_id.0],
        row_to_target,
        "list targets"
    )
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
    crate::db_query_all!(
        conn,
        combo_target_with_model_select!(
            "WHERE ct.combo_id = ?1 \
             ORDER BY ct.priority_order ASC, ct.id ASC"
        ),
        params![combo_id.0],
        row_to_target_with_model,
        "list targets with model"
    )
}

pub fn get_target(conn: &Connection, id: ComboTargetId) -> Result<Option<ComboTarget>> {
    crate::db_query_one!(
        conn,
        combo_target_select!("WHERE ct.id = ?1"),
        params![id.0],
        row_to_target,
        format!("get combo_target {}", id.0)
    )
}

pub fn target_belongs_to_combo(
    conn: &Connection,
    combo_id: ComboId,
    target_id: ComboTargetId,
) -> Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM combo_targets WHERE id = ?1 AND combo_id = ?2)",
        params![target_id.0, combo_id.0],
        |r| r.get::<_, i64>(0),
    )
    .map(|v| v != 0)
    .map_err(crate::error::map_db_error_ctx(format!(
        "check combo_target {} belongs to combo {}",
        target_id.0, combo_id.0
    )))
}

pub fn delete_target(conn: &Connection, id: ComboTargetId) -> Result<()> {
    crate::db_execute!(
        conn,
        "DELETE FROM combo_targets WHERE id = ?1",
        params![id.0],
        format!("delete combo_target {}", id.0)
    )?;
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
        source: Some(std::sync::Arc::new(e)),
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
            "weight must be a positive integer, got {weight}"
        )));
    }
    conn.execute(
        "UPDATE combo_targets SET weight = ?1 WHERE id = ?2",
        params![i64::from(weight), target_id.0],
    )
    .map_err(crate::error::map_db_error_ctx(format!(
        "update weight for combo_target {}",
        target_id.0
    )))?;
    Ok(())
}

pub fn update_target_active(
    conn: &Connection,
    target_id: ComboTargetId,
    active: bool,
) -> Result<()> {
    conn.execute(
        "UPDATE combo_targets SET active = ?1 WHERE id = ?2",
        params![i64::from(active), target_id.0],
    )
    .map_err(crate::error::map_db_error_ctx(format!(
        "update active for combo_target {}",
        target_id.0
    )))?;
    Ok(())
}

pub fn update_target_cooldown_mode(
    conn: &Connection,
    target_id: ComboTargetId,
    mode: Option<&str>,
) -> Result<()> {
    let mode_str = match mode {
        Some(s) if !s.is_empty() => {
            let parsed = CooldownMode::parse(s).map_err(CoreError::Validation)?;
            Some(parsed.as_str().to_string())
        }
        _ => None,
    };
    conn.execute(
        "UPDATE combo_targets SET cooldown_mode = ?1 WHERE id = ?2",
        params![mode_str, target_id.0],
    )
    .map_err(crate::error::map_db_error_ctx(format!(
        "update cooldown_mode for combo_target {}",
        target_id.0
    )))?;
    Ok(())
}

fn update_target_column<T: rusqlite::ToSql>(
    conn: &Connection,
    target_id: ComboTargetId,
    column: &'static str,
    value: T,
) -> Result<()> {
    let sql = format!("UPDATE combo_targets SET {column} = ?1 WHERE id = ?2");
    conn.execute(&sql, params![value, target_id.0])
        .map_err(crate::error::map_db_error_ctx(format!(
            "update {column} for combo_target {}",
            target_id.0
        )))?;
    Ok(())
}

pub fn update_target_cooldown_base(
    conn: &Connection,
    target_id: ComboTargetId,
    base: Option<u64>,
) -> Result<()> {
    update_target_column(
        conn,
        target_id,
        "cooldown_base_secs",
        base.map(|v| v as i64),
    )
}

pub fn update_target_cooldown_max(
    conn: &Connection,
    target_id: ComboTargetId,
    max: Option<u64>,
) -> Result<()> {
    update_target_column(conn, target_id, "cooldown_max_secs", max.map(|v| v as i64))
}

pub fn update_target_cooldown_factor(
    conn: &Connection,
    target_id: ComboTargetId,
    factor: Option<u32>,
) -> Result<()> {
    update_target_column(conn, target_id, "cooldown_factor", factor.map(i64::from))
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
fn validate_reorder_target_ids(
    tx: &rusqlite::Transaction,
    combo_id: ComboId,
    ordered_ids: &[ComboTargetId],
) -> Result<()> {
    let mut stmt = tx
        .prepare(combo_target_ids_select!("WHERE combo_id = ?1"))
        .map_err(crate::error::map_db_error)?;
    let mut current: Vec<i64> = stmt
        .query_map(params![combo_id.0], |r| r.get::<_, i64>(0))
        .map_err(crate::error::map_db_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(crate::error::map_db_error)?;

    current.sort_unstable();
    let mut incoming: Vec<i64> = ordered_ids.iter().map(|i| i.0).collect();
    incoming.sort_unstable();
    if current != incoming {
        return Err(CoreError::Validation(
            "target_ids must be a permutation of the combo's current targets".into(),
        ));
    }
    Ok(())
}

fn apply_target_priority_chunks(
    tx: &rusqlite::Transaction,
    combo_id: ComboId,
    ordered_ids: &[ComboTargetId],
) -> Result<()> {
    if ordered_ids.is_empty() {
        return Ok(());
    }
    const CHUNK_SIZE: usize = 400;
    for (chunk_idx, chunk) in ordered_ids.chunks(CHUNK_SIZE).enumerate() {
        let chunk_start_priority = chunk_idx * CHUNK_SIZE;
        let vals = crate::batch::values_placeholders(chunk.len(), 2);
        let query = format!(
            "WITH updates(id, priority) AS (VALUES {vals}) \
             UPDATE combo_targets SET priority_order = updates.priority \
             FROM updates WHERE combo_targets.id = updates.id AND combo_targets.combo_id = ?"
        );

        let mut params = Vec::with_capacity(chunk.len() * 2 + 1);
        for (i, tid) in chunk.iter().enumerate() {
            params.push(rusqlite::types::Value::Integer(tid.0));
            params.push(rusqlite::types::Value::Integer(
                (chunk_start_priority + i + 1) as i64,
            ));
        }
        params.push(rusqlite::types::Value::Integer(combo_id.0));

        let mut stmt = tx
            .prepare_cached(&query)
            .map_err(crate::error::map_db_error)?;
        stmt.execute(rusqlite::params_from_iter(params))
            .map_err(crate::error::map_db_error)?;
    }
    Ok(())
}

pub fn reorder_targets(
    conn: &mut Connection,
    combo_id: ComboId,
    ordered_ids: &[ComboTargetId],
) -> Result<()> {
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(crate::error::map_db_error)?;

    validate_reorder_target_ids(&tx, combo_id, ordered_ids)?;
    apply_target_priority_chunks(&tx, combo_id, ordered_ids)?;
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
    let value: Option<&str> = match mode {
        None => None,
        Some(s) => {
            // Validate the string before persisting so a typo doesn't
            // land in the DB only to surface as `Strict` on the next
            // read (silently masking the misconfiguration).
            let parsed = PriorityMode::parse(s).map_err(CoreError::Validation)?;
            Some(parsed.as_str())
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

/// Update ONLY the cooldown_mode column, leaving base/max/factor
/// untouched. This is the per-field update used by the dashboard's
/// individual cooldown setting inputs.
pub fn update_cooldown_mode(conn: &Connection, id: ComboId, mode: Option<&str>) -> Result<()> {
    let mode_value: Option<&str> = match mode {
        None => None,
        Some(s) => {
            let parsed = CooldownMode::parse(s).map_err(CoreError::Validation)?;
            Some(parsed.as_str())
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

fn update_combo_column<T: rusqlite::ToSql>(
    conn: &Connection,
    id: ComboId,
    column: &'static str,
    value: T,
) -> Result<()> {
    let sql = format!("UPDATE combos SET {column} = ?1 WHERE id = ?2");
    let affected =
        conn.execute(&sql, params![value, id.0])
            .map_err(crate::error::map_db_error_ctx(format!(
                "update {column} for combo {}",
                id.0
            )))?;
    if affected == 0 {
        return Err(CoreError::ComboNotFound(id.0));
    }
    Ok(())
}

/// Update ONLY the cooldown_base_secs column.
pub fn update_cooldown_base(conn: &Connection, id: ComboId, base: Option<u64>) -> Result<()> {
    update_combo_column(conn, id, "cooldown_base_secs", base.map(|v| v as i64))
}

/// Update ONLY the cooldown_max_secs column.
pub fn update_cooldown_max(conn: &Connection, id: ComboId, max: Option<u64>) -> Result<()> {
    update_combo_column(conn, id, "cooldown_max_secs", max.map(|v| v as i64))
}

/// Update ONLY the cooldown_factor column.
pub fn update_cooldown_factor(conn: &Connection, id: ComboId, factor: Option<u32>) -> Result<()> {
    update_combo_column(conn, id, "cooldown_factor", factor.map(i64::from))
}

/// Update the preventive_rate_limit toggle for a combo.
pub fn update_preventive_rate_limit(conn: &Connection, id: ComboId, enabled: bool) -> Result<()> {
    update_combo_column(conn, id, "preventive_rate_limit", i64::from(enabled))
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
            "lkgp_exploration_rate must be in [0.0, 1.0], got {rate}"
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

fn row_to_target(row: &Row<'_>) -> rusqlite::Result<ComboTarget> {
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
    })
}

fn row_to_target_with_model(row: &Row<'_>) -> rusqlite::Result<ComboTargetWithModel> {
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
    })
}

fn resolve_target_context_window(
    conn: &rusqlite::Connection,
    model_row_id: Option<i64>,
    sub_combo_id: Option<i64>,
    visited: &mut Vec<openproxy_types::ComboId>,
    depth: u32,
) -> openproxy_types::error::Result<Option<i64>> {
    if let Some(sub_id) = sub_combo_id {
        compute_effective_context_window_recursive(
            conn,
            openproxy_types::ComboId(sub_id),
            visited,
            depth + 1,
        )
    } else if let Some(row_id) = model_row_id {
        let model_cw: Option<i64> = conn
            .query_row(
                model_context_length_select!("WHERE id = ?1"),
                rusqlite::params![row_id],
                |row| row.get(0),
            )
            .map_err(crate::error::map_db_error_ctx(format!(
                "get context_length for model {row_id}"
            )))?;
        Ok(model_cw)
    } else {
        Ok(None)
    }
}

fn validate_recursion_depth(
    combo_id: ComboId,
    visited: &mut Vec<ComboId>,
    depth: u32,
) -> Result<()> {
    if depth > openproxy_types::MAX_SUB_COMBO_DEPTH {
        return Err(openproxy_types::error::CoreError::Validation(format!(
            "max sub-combo depth ({}) exceeded",
            openproxy_types::MAX_SUB_COMBO_DEPTH
        )));
    }
    if visited.contains(&combo_id) {
        return Err(openproxy_types::error::CoreError::Validation(format!(
            "cyclic combo detected at id {}",
            combo_id.0
        )));
    }
    visited.push(combo_id);
    Ok(())
}

fn extract_and_resolve_window(
    conn: &rusqlite::Connection,
    row: &rusqlite::Row,
    visited: &mut Vec<ComboId>,
    depth: u32,
) -> Result<Option<i64>> {
    let (model_row_id, sub_combo_id): (Option<i64>, Option<i64>) =
        crate::map_row_tuple!(row => (0, 1)).map_err(crate::error::map_db_error)?;
    resolve_target_context_window(conn, model_row_id, sub_combo_id, visited, depth)
}

fn aggregate_target_context_windows(
    conn: &rusqlite::Connection,
    combo_id: ComboId,
    visited: &mut Vec<ComboId>,
    depth: u32,
) -> Result<Option<i64>> {
    let mut stmt = conn
        .prepare(combo_target_model_sub_select!("WHERE ct.combo_id = ?1"))
        .map_err(crate::error::map_db_error)?;
    let mut rows = stmt
        .query(rusqlite::params![combo_id.0])
        .map_err(crate::error::map_db_error)?;

    let mut min_window: Option<i64> = None;
    while let Some(row) = rows.next().map_err(crate::error::map_db_error)? {
        if let Some(cw) = extract_and_resolve_window(conn, row, visited, depth)? {
            min_window = Some(min_window.map_or(cw, |min| std::cmp::min(min, cw)));
        }
    }
    Ok(min_window)
}

fn compute_effective_context_window_recursive(
    conn: &rusqlite::Connection,
    combo_id: openproxy_types::ComboId,
    visited: &mut Vec<openproxy_types::ComboId>,
    depth: u32,
) -> openproxy_types::error::Result<Option<i64>> {
    validate_recursion_depth(combo_id, visited, depth)?;

    let cw: Option<i64> = conn
        .query_row(
            combo_context_window_select!("WHERE id = ?1"),
            rusqlite::params![combo_id.0],
            |row| row.get(0),
        )
        .map_err(crate::error::map_db_error_ctx(format!(
            "get context_window for combo {}",
            combo_id.0
        )))?;

    let res = if cw.is_some() {
        Ok(cw)
    } else {
        aggregate_target_context_windows(conn, combo_id, visited, depth)
    };

    visited.pop();
    res
}

pub fn compute_effective_context_window(
    conn: &rusqlite::Connection,
    combo_id: ComboId,
) -> Result<Option<i64>> {
    let mut visited = Vec::new();
    compute_effective_context_window_recursive(conn, combo_id, &mut visited, 0)
}

pub fn resolve_combo_to_targets(
    conn: &rusqlite::Connection,
    combo_id: ComboId,
    visited: &mut Vec<ComboId>,
    depth: u32,
) -> Result<Vec<ComboTarget>> {
    validate_recursion_depth(combo_id, visited, depth)?;

    let targets = list_targets(conn, combo_id)?;
    let mut flat = Vec::new();
    for t in targets {
        if let Some(sub_id) = t.sub_combo_id {
            let sub = resolve_combo_to_targets(conn, sub_id, visited, depth + 1)?;
            flat.extend(sub);
        } else {
            flat.push(t);
        }
    }
    visited.pop();
    Ok(flat)
}

fn fetch_healthy_accounts(
    conn: &rusqlite::Connection,
    provider_id: &ProviderId,
) -> Result<Vec<AccountId>> {
    let mut stmt = conn
        .prepare(account_healthy_ids_select!(
            "WHERE provider_id = ?1 AND health_status = 'healthy' ORDER BY priority ASC, id ASC"
        ))
        .map_err(crate::error::map_db_error)?;
    let rows = stmt
        .query_map(params![provider_id.as_str()], |r| r.get::<_, i64>(0))
        .map_err(crate::error::map_db_error)?;
    let mut accounts = Vec::new();
    for r in rows.flatten() {
        accounts.push(AccountId(r));
    }
    Ok(accounts)
}

fn expand_single_target_rotation(
    conn: &rusqlite::Connection,
    target: ComboTarget,
    out: &mut Vec<ComboTarget>,
) -> Result<()> {
    if target.account_id.is_some() || target.sub_combo_id.is_some() {
        out.push(target);
        return Ok(());
    }
    let healthy_accounts = fetch_healthy_accounts(conn, &target.provider_id)?;
    if healthy_accounts.is_empty() {
        out.push(target);
    } else {
        for acc_id in healthy_accounts {
            let mut ct = target.clone();
            ct.account_id = Some(acc_id);
            out.push(ct);
        }
    }
    Ok(())
}

pub fn expand_account_rotation(
    conn: &rusqlite::Connection,
    targets: Vec<ComboTarget>,
) -> Result<Vec<ComboTarget>> {
    let mut out = Vec::with_capacity(targets.len());
    for t in targets {
        expand_single_target_rotation(conn, t, &mut out)?;
    }
    Ok(out)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conn::DbPool;

    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;

    fn fresh_pool() -> (DbPool, PathBuf) {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!("openproxy-crud-test-{pid}-{nanos}-{n}"));
        std::fs::create_dir_all(&dir).expect("mkdir tempdir");
        let path = dir.join("crud.db");
        let pool = DbPool::open(&path).expect("open pool");
        {
            let mut w = pool.writer();
            crate::migrations::run(&mut w).expect("migrations");
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
        assert!(
            matches!(err, CoreError::Validation(ref msg) if msg.contains("race_size must be in 1..=8")),
            "Expected Validation error, got {err:?}"
        );

        // Race size 9 is invalid
        let err2 = create_combo(&conn, combo_name, strategy, 9)
            .expect_err("create combo should fail with race size 9");
        assert!(
            matches!(err2, CoreError::Validation(ref msg) if msg.contains("race_size must be in 1..=8")),
            "Expected Validation error, got {err2:?}"
        );
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

        assert!(
            matches!(err, CoreError::Validation(ref msg) if msg.contains("combo name already exists")),
            "Expected Validation error for duplicate name, got {err:?}"
        );
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
            panic!("Expected Database error with CHECK constraint failure, got {err:?}");
        }
    }

    #[test]
    fn test_combo_in_chain_no_cycle() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();

        // C1 -> C2 -> C3
        let c1 = ComboId(1);
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
