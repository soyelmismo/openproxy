//! Targets CRUD and cycle validation for combos.

use openproxy_types::ProviderId;
use openproxy_types::combos::{
    AddTargetInput, ComboTarget, ComboTargetWithModel, MAX_SUB_COMBO_DEPTH,
};
use openproxy_types::error::{CoreError, Result};
use openproxy_types::ids::{AccountId, ComboId, ComboTargetId, ModelRowId};
use rusqlite::{Connection, params};

use super::mapping::{
    combo_target_ids_select, combo_target_select, combo_target_with_model_select,
    model_provider_id_select, model_upstream_id_select, row_to_target, row_to_target_with_model,
};

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

pub fn reconnect_orphan_targets(
    conn: &Connection,
    provider_id: &ProviderId,
    upstream_model_id: &str,
    model_row_id: ModelRowId,
) -> Result<usize> {
    let affected = conn
        .execute(
            "UPDATE combo_targets \
             SET model_row_id = ?1 \
             WHERE provider_id = ?2 \
               AND upstream_model_id = ?3 \
               AND model_row_id IS NULL",
            params![model_row_id.0, provider_id.as_str(), upstream_model_id],
        )
        .map_err(crate::error::map_db_error_ctx(format!(
            "reconnect orphan targets for {provider_id}:{upstream_model_id} -> model_row_id={}",
            model_row_id.0
        )))?;

    if affected > 0 {
        tracing::info!(
            target: "openproxy::db::combos",
            provider = provider_id.as_str(),
            upstream_model_id,
            model_row_id = model_row_id.0,
            reconnected_targets = affected,
            "reconnected orphan combo_targets after model row upsert"
        );
    }
    Ok(affected)
}

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
    crate::db_query_all!(
        conn,
        combo_target_select!(
            "LEFT JOIN target_cooldowns tc ON tc.combo_target_id = ct.id \
             LEFT JOIN models m ON m.id = ct.model_row_id \
             LEFT JOIN ( \
                 SELECT ct2.model_row_id, MAX(tc2.cooldown_until) as model_cooldown_until \
                 FROM target_cooldowns tc2 \
                 INNER JOIN combo_targets ct2 ON ct2.id = tc2.combo_target_id \
                 WHERE ct2.model_row_id IS NOT NULL \
                   AND datetime(tc2.cooldown_until) > datetime('now') \
                 GROUP BY ct2.model_row_id \
             ) mc ON mc.model_row_id = ct.model_row_id \
             WHERE ct.combo_id = ?1 AND p.active = 1 AND ct.active = 1 \
                 AND (ct.model_row_id IS NULL OR (m.id IS NOT NULL AND m.active = 1)) \
                 AND (tc.cooldown_until IS NULL OR datetime(tc.cooldown_until) <= datetime('now')) \
                 AND mc.model_cooldown_until IS NULL \
                 AND NOT (ct.model_row_id IS NULL AND ct.sub_combo_id IS NULL) \
             ORDER BY ct.priority_order ASC, ct.id ASC"
        ),
        params![combo_id.0],
        row_to_target,
        "list targets"
    )
}

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

fn validate_reorder_target_ids(
    tx: &rusqlite::Transaction<'_>,
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
    tx: &rusqlite::Transaction<'_>,
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
        let mut query = String::with_capacity(160 + vals.len());
        query.push_str("WITH updates(id, priority) AS (VALUES ");
        query.push_str(&vals);
        query.push_str(") UPDATE combo_targets SET priority_order = updates.priority FROM updates WHERE combo_targets.id = updates.id AND combo_targets.combo_id = ?");

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
