use super::*;
use crate::error::*;
use crate::ids::*;
use rusqlite::Connection;
pub fn compute_effective_context_window(
    conn: &Connection,
    combo_id: ComboId,
) -> Result<Option<i64>> {
    let combo = match get_combo(conn, combo_id)? {
        Some(c) => c,
        None => return Ok(None),
    };
    // Explicit override wins.
    if let Some(cw) = combo.context_window {
        return Ok(Some(cw));
    }
    // Auto-compute: recursively find the min context_length across
    // all targets.
    let targets = list_targets(conn, combo_id)?;
    if targets.is_empty() {
        return Ok(None);
    }
    let mut min_cw: Option<i64> = None;
    for t in &targets {
        let target_cw = if let Some(sub_id) = t.sub_combo_id {
            // Sub-combo: recurse with cycle guard.
            compute_context_window_recursive(conn, sub_id, &mut Vec::new())?
        } else if let Some(model_row_id) = t.model_row_id {
            // Flat target: look up the model's context_length.
            get_model_context_length(conn, model_row_id)?
        } else {
            None
        };
        if let Some(cw) = target_cw {
            min_cw = Some(min_cw.map_or(cw, |m| m.min(cw)));
        }
    }
    Ok(min_cw)
}

/// Recursive helper for `compute_effective_context_window`. The
/// `visited` vector tracks the combo ids already seen in the current
/// recursion chain to detect cycles.
fn compute_context_window_recursive(
    conn: &Connection,
    combo_id: ComboId,
    visited: &mut Vec<ComboId>,
) -> Result<Option<i64>> {
    // Cycle guard.
    if visited.contains(&combo_id) {
        tracing::warn!(
            combo_id = combo_id.0,
            "cycle detected in sub-combo context window computation; returning None"
        );
        return Ok(None);
    }
    visited.push(combo_id);

    let combo = match get_combo(conn, combo_id)? {
        Some(c) => c,
        None => {
            visited.pop();
            return Ok(None);
        }
    };
    // Explicit override wins.
    if let Some(cw) = combo.context_window {
        visited.pop();
        return Ok(Some(cw));
    }

    let targets = list_targets(conn, combo_id)?;
    if targets.is_empty() {
        visited.pop();
        return Ok(None);
    }

    let mut min_cw: Option<i64> = None;
    for t in &targets {
        let target_cw = if let Some(sub_id) = t.sub_combo_id {
            compute_context_window_recursive(conn, sub_id, visited)?
        } else if let Some(model_row_id) = t.model_row_id {
            get_model_context_length(conn, model_row_id)?
        } else {
            None
        };
        if let Some(cw) = target_cw {
            min_cw = Some(min_cw.map_or(cw, |m| m.min(cw)));
        }
    }
    visited.pop();
    Ok(min_cw)
}

/// Look up a model's `context_length` by its `model_row_id`.
fn get_model_context_length(conn: &Connection, model_row_id: ModelRowId) -> Result<Option<i64>> {
    let cw: Option<i64> = conn
        .query_row(
            "SELECT context_length FROM models WHERE id = ?1",
            params![model_row_id.0],
            |row| row.get(0),
        )
        .optional()
        .map_err(crate::error::map_db_error_ctx(format!(
            "get_model_context_length {}",
            model_row_id.0
        )))?
        .flatten();
    Ok(cw)
}

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
    if visited.contains(&combo_id) {
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
        .map_err(crate::error::map_db_error)?;

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
        .map_err(crate::error::map_db_error)?;
    let rows: Vec<(i64, String)> = stmt
        .query_map(params![provider_id.as_str()], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })
        .map_err(crate::error::map_db_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(crate::error::map_db_error)?;

    let mut added = 0usize;
    {
        let mut stmt = conn
            .prepare(
                "INSERT OR IGNORE INTO combo_targets(\
                 combo_id, provider_id, account_id, model_row_id, priority_order\
             ) VALUES (?1, ?2, NULL, ?3, ?4)",
            )
            .map_err(crate::error::map_db_error)?;
        for (row_id, _model_id) in &rows {
            // Ignore UNIQUE collisions: this helper is reused by a code path
            // that may run after a manual add. Better to keep the existing
            // row than to bubble an error up.
            let res = stmt.execute(params![combo_id.0, provider_id.as_str(), row_id, row_id]);
            match res {
                Ok(n) if n > 0 => added += 1,
                Ok(_) => {} // UNIQUE collision, no-op
                Err(e) => {
                    return Err(crate::error::map_db_error_ctx("insert auto target execute")(e));
                }
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
            .map_err(crate::error::map_db_error)?;
        let rows = stmt
            .query_map(params![t.provider_id.as_str()], |row| row.get::<_, i64>(0))
            .map_err(crate::error::map_db_error)?;
        let mut count = 0usize;
        for r in rows {
            let account_id = r.map_err(crate::error::map_db_error)?;
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
