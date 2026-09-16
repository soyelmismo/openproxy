use openproxy_types::{
    Model, ModelId, ModelRowId, ProviderId, Result, TargetFormat, normalize_model_id,
};
use rusqlite::{Connection, params};

use super::{map_row, model_select};
use crate::error::{map_db_error, map_db_error_ctx};

pub fn list_active(conn: &Connection, provider: &ProviderId) -> Result<Vec<Model>> {
    crate::db_query_all!(
        conn,
        model_select!(
            "WHERE provider_id = ?1 AND active = 1 \
             AND provider_id IN (SELECT id FROM providers WHERE active = 1)"
        ),
        params![provider.as_str()],
        map_row,
        format!("list active models for {provider}")
    )
}

pub fn list_active_all(conn: &Connection) -> Result<Vec<Model>> {
    crate::db_query_all!(
        conn,
        model_select!(
            "WHERE active = 1 \
             AND provider_id IN (SELECT id FROM providers WHERE active = 1)"
        ),
        [],
        map_row,
        "list active models"
    )
}

pub fn list_all(conn: &Connection) -> Result<Vec<Model>> {
    crate::db_query_all!(conn, model_select!(), [], map_row, "list all models")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProviderModelCounts {
    pub active_models: i64,
    pub total_models: i64,
}

pub fn count_by_provider(conn: &Connection, provider: &ProviderId) -> Result<ProviderModelCounts> {
    let (active_models, total_models) = conn
        .query_row(
            "SELECT \
                COALESCE(SUM(CASE WHEN active = 1 THEN 1 ELSE 0 END), 0), \
                COUNT(*) \
             FROM models WHERE provider_id = ?1",
            [provider.as_str()],
            |row| crate::map_row_tuple!(row => ((0, i64), (1, i64))),
        )
        .map_err(map_db_error)?;
    Ok(ProviderModelCounts {
        active_models,
        total_models,
    })
}

pub fn mark_expired(conn: &Connection) -> Result<usize> {
    let n = conn
        .execute(
            "DELETE FROM models \
             WHERE expires_at IS NOT NULL \
                AND expires_at < datetime('now', '-7 days')",
            [],
        )
        .map_err(map_db_error)?;
    Ok(n)
}

pub fn get_by_row_id(conn: &Connection, row_id: ModelRowId) -> Result<Option<Model>> {
    crate::db_query_one!(
        conn,
        model_select!("WHERE id = ?1"),
        params![row_id.0],
        map_row,
        format!("get model by row id {}", row_id.0)
    )
}

pub fn get_by_row_ids(conn: &Connection, row_ids: &[ModelRowId]) -> Result<Vec<Model>> {
    if row_ids.is_empty() {
        return Ok(Vec::new());
    }
    let query = model_select!("WHERE id IN ({})");
    crate::batch::query_in_chunks_by(
        conn,
        query,
        row_ids,
        crate::batch::DEFAULT_CHUNK_SIZE,
        |id| id.0,
        map_row,
    )
    .map_err(map_db_error)
}

pub fn find_active_by_name(conn: &Connection, model_id: &str) -> Result<Option<Model>> {
    crate::db_query_one!(
        conn,
        model_select!(
            "WHERE model_id = ?1 AND active = 1 \
             AND provider_id IN (SELECT id FROM providers WHERE active = 1) \
             ORDER BY id ASC LIMIT 1"
        ),
        params![model_id],
        map_row,
        format!("find active model by name {model_id}")
    )
}

pub fn find_active_by_provider_and_name(
    conn: &Connection,
    provider_id: &ProviderId,
    model_id: &str,
) -> Result<Option<Model>> {
    crate::db_query_one!(
        conn,
        model_select!(
            "WHERE provider_id = ?1 AND model_id = ?2 AND active = 1 \
             AND provider_id IN (SELECT id FROM providers WHERE active = 1) \
             ORDER BY id ASC LIMIT 1"
        ),
        params![provider_id.as_str(), model_id],
        map_row,
        format!("find active model for provider {provider_id} and name {model_id}")
    )
}

pub fn set_test_status(conn: &Connection, id: ModelRowId, status: i32) -> Result<()> {
    conn.execute(
        "UPDATE models \
         SET last_test_status = ?1, last_test_at = datetime('now') \
         WHERE id = ?2",
        params![status, id.0],
    )
    .map_err(map_db_error_ctx(format!(
        "update test status for model {}",
        id.0
    )))?;
    Ok(())
}

pub fn delete(conn: &Connection, id: ModelRowId) -> Result<u64> {
    crate::error::with_busy_retry("delete_model", || {
        let tx = conn.unchecked_transaction().map_err(map_db_error)?;

        let removed = tx
            .execute("DELETE FROM models WHERE id = ?1", params![id.0])
            .map_err(map_db_error_ctx(format!("delete model {}", id.0)))?;

        tx.commit().map_err(map_db_error)?;

        Ok(removed as u64)
    })
}

pub fn create_custom(
    conn: &Connection,
    provider_id: &ProviderId,
    model_id: &ModelId,
    display_name: Option<&str>,
    target_format: TargetFormat,
    ttl_seconds: i64,
    model_type: Option<&str>,
) -> Result<ModelRowId> {
    let normalized = normalize_model_id(model_id.as_str());
    let effective_type = model_type.unwrap_or("chat");
    let ttl_param = if ttl_seconds > 0 {
        Some(ttl_seconds)
    } else {
        None
    };

    let row_id: i64 = conn
        .query_row(
            "INSERT INTO models \
                (provider_id, model_id, display_name, target_format, \
                 discovered_at, expires_at, active, custom, model_id_normalized, model_type) \
             VALUES (?1, ?2, ?3, ?4, datetime('now'), \
                     CASE WHEN ?7 IS NOT NULL THEN datetime('now', '+' || ?7 || ' seconds') ELSE NULL END, \
                     1, 1, ?5, ?6) \
             ON CONFLICT(provider_id, model_id) DO UPDATE SET \
                display_name = excluded.display_name, \
                target_format = excluded.target_format, \
                discovered_at = datetime('now'), \
                expires_at = excluded.expires_at, \
                active = 1, \
                custom = 1, \
                model_type = excluded.model_type, \
                model_id_normalized = COALESCE(excluded.model_id_normalized, model_id_normalized) \
             RETURNING id",
            params![
                provider_id.as_str(),
                model_id.as_str(),
                display_name,
                target_format.as_str(),
                &normalized,
                effective_type,
                ttl_param,
            ],
            |r| r.get(0),
        )
        .map_err(|e| {
            if crate::error::classify_sqlite_error(&e) == crate::error::DbErrorKind::ForeignKeyViolation {
                openproxy_types::CoreError::Validation(format!(
                    "provider_id does not exist: {provider_id}"
                ))
            } else {
                map_db_error_ctx(format!("create_custom model for {provider_id}"))(e)
            }
        })?;

    Ok(ModelRowId(row_id))
}

pub fn update_model_type(conn: &Connection, id: ModelRowId, model_type: &str) -> Result<()> {
    crate::db_update_field!(
        conn,
        "models",
        model_type = model_type,
        WHERE id = id.0,
        format!("update model_type for model {}", id.0)
    )?;
    Ok(())
}

fn update_model_display_name(
    conn: &Connection,
    id: ModelRowId,
    display_name: Option<&str>,
) -> Result<()> {
    if let Some(dn) = display_name {
        conn.execute(
            "UPDATE models SET display_name = ?1 WHERE id = ?2",
            params![dn, id.0],
        )
        .map_err(crate::error::map_db_error)?;
    }
    Ok(())
}

fn update_model_type_opt(
    conn: &Connection,
    id: ModelRowId,
    model_type: Option<&str>,
) -> Result<()> {
    if let Some(mt) = model_type {
        conn.execute(
            "UPDATE models SET model_type = ?1 WHERE id = ?2",
            params![mt, id.0],
        )
        .map_err(crate::error::map_db_error)?;
    }
    Ok(())
}

fn update_model_target_format(
    conn: &Connection,
    id: ModelRowId,
    target_format: Option<TargetFormat>,
) -> Result<()> {
    if let Some(tf) = target_format {
        conn.execute(
            "UPDATE models SET target_format = ?1 WHERE id = ?2",
            params![tf.as_str(), id.0],
        )
        .map_err(crate::error::map_db_error)?;
    }
    Ok(())
}

pub fn update_model_details(
    conn: &Connection,
    id: ModelRowId,
    display_name: Option<&str>,
    model_type: Option<&str>,
    target_format: Option<TargetFormat>,
) -> Result<()> {
    update_model_display_name(conn, id, display_name)?;
    update_model_type_opt(conn, id, model_type)?;
    update_model_target_format(conn, id, target_format)
}
