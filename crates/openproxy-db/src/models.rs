//! Database access layer and repository for the `models` table.

use openproxy_types::{
    DiscoveredModel, Model, ModelId, ModelRowId, ProviderId, Result, TargetFormat, UpsertResult,
    normalize_model_id,
};
use rusqlite::{Connection, OptionalExtension, Row, params};
use std::sync::Arc;
use std::time::Duration;

use crate::conn::DbPool;
use crate::error::{map_db_error, map_db_error_ctx};

fn map_row(row: &Row<'_>) -> rusqlite::Result<Model> {
    let target_format_str: String = row.get("target_format")?;
    let target_format = match target_format_str.as_str() {
        "openai" => TargetFormat::Openai,
        "anthropic" => TargetFormat::Anthropic,
        "gemini" => TargetFormat::Gemini,
        "responses" => TargetFormat::Responses,
        other => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid target_format in db: {}", other),
                )),
            ));
        }
    };

    let active_bit: i64 = row.get("active")?;
    let active = match active_bit {
        0 => false,
        1 => true,
        other => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Integer,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid active bit in db: {}", other),
                )),
            ));
        }
    };

    let custom_bit: i64 = row.get("custom")?;
    let custom = match custom_bit {
        0 => false,
        1 => true,
        other => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Integer,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid custom bit in db: {}", other),
                )),
            ));
        }
    };

    Ok(Model {
        row_id: ModelRowId(row.get::<_, i64>("id")?),
        provider_id: ProviderId::new(row.get::<_, String>("provider_id")?),
        model_id: ModelId::new(row.get::<_, String>("model_id")?),
        display_name: row.get::<_, Option<String>>("display_name")?,
        target_format,
        discovered_at: row.get::<_, String>("discovered_at")?,
        expires_at: row.get::<_, Option<String>>("expires_at")?,
        timeout_overrides_json: row.get::<_, Option<String>>("timeout_overrides_json")?,
        active,
        last_test_status: row.get::<_, Option<i32>>("last_test_status")?,
        last_test_at: row.get::<_, Option<String>>("last_test_at")?,
        custom,
        context_length: row.get::<_, Option<i64>>("context_length")?,
        max_output_tokens: row.get::<_, Option<i64>>("max_output_tokens")?,
        capabilities_json: row.get::<_, Option<String>>("capabilities_json")?,
        family: row.get::<_, Option<String>>("family")?,
        model_type: row
            .get::<_, Option<String>>("model_type")?
            .unwrap_or_else(|| "chat".to_string()),
        input_modalities_json: row.get::<_, Option<String>>("input_modalities_json")?,
        output_modalities_json: row.get::<_, Option<String>>("output_modalities_json")?,
    })
}

pub fn list_active(conn: &Connection, provider: &ProviderId) -> Result<Vec<Model>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, provider_id, model_id, display_name, target_format, \
                    discovered_at, expires_at, timeout_overrides_json, active, \
                    last_test_status, last_test_at, custom, \
                    context_length, max_output_tokens, capabilities_json, \
                    family, model_type, input_modalities_json, \
                    output_modalities_json \
             FROM models \
             WHERE provider_id = ? \
               AND active = 1",
        )
        .map_err(map_db_error)?;

    let rows = stmt
        .query_map([provider.as_str()], map_row)
        .map_err(map_db_error)?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(map_db_error)?);
    }
    Ok(out)
}

pub fn list_active_all(conn: &Connection) -> Result<Vec<Model>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, provider_id, model_id, display_name, target_format, \
                    discovered_at, expires_at, timeout_overrides_json, active, \
                    last_test_status, last_test_at, custom, \
                    context_length, max_output_tokens, capabilities_json, \
                    family, model_type, input_modalities_json, \
                    output_modalities_json \
             FROM models \
             WHERE active = 1",
        )
        .map_err(map_db_error)?;

    let rows = stmt.query_map([], map_row).map_err(map_db_error)?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(map_db_error)?);
    }
    Ok(out)
}

pub fn list_all(conn: &Connection) -> Result<Vec<Model>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, provider_id, model_id, display_name, target_format, \
                    discovered_at, expires_at, timeout_overrides_json, active, \
                    last_test_status, last_test_at, custom, \
                    context_length, max_output_tokens, capabilities_json, \
                    family, model_type, input_modalities_json, \
                    output_modalities_json \
             FROM models",
        )
        .map_err(map_db_error)?;

    let rows = stmt.query_map([], map_row).map_err(map_db_error)?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(map_db_error)?);
    }
    Ok(out)
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

pub fn set_active(conn: &Connection, id: ModelRowId, active: bool) -> Result<()> {
    let bit = if active { 1i64 } else { 0i64 };
    conn.execute(
        "UPDATE models SET active = ?1 WHERE id = ?2",
        params![bit, id.0],
    )
    .map_err(map_db_error_ctx(format!(
        "update active for model {}",
        id.0
    )))?;
    Ok(())
}

pub fn set_active_bulk(conn: &Connection, provider: &ProviderId, active: bool) -> Result<u64> {
    let bit = if active { 1i64 } else { 0i64 };
    let n = conn
        .execute(
            "UPDATE models SET active = ?1 WHERE provider_id = ?2 AND custom = 0",
            params![bit, provider.as_str()],
        )
        .map_err(map_db_error_ctx(format!(
            "set_active_bulk for {}",
            provider
        )))?;
    Ok(n as u64)
}

pub fn get_by_row_id(conn: &Connection, row_id: ModelRowId) -> Result<Option<Model>> {
    let res = conn
        .query_row(
            "SELECT id, provider_id, model_id, display_name, target_format, \
                    discovered_at, expires_at, timeout_overrides_json, active, \
                    last_test_status, last_test_at, custom, \
                    context_length, max_output_tokens, capabilities_json, \
                    family, model_type, input_modalities_json, \
                    output_modalities_json \
             FROM models WHERE id = ?",
            [row_id.0],
            map_row,
        )
        .optional()
        .map_err(map_db_error)?;
    Ok(res)
}

pub fn get_by_row_ids(conn: &Connection, row_ids: &[ModelRowId]) -> Result<Vec<Model>> {
    if row_ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = std::iter::repeat_n("?", row_ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let query = format!(
        "SELECT id, provider_id, model_id, display_name, target_format, \
                discovered_at, expires_at, timeout_overrides_json, active, \
                last_test_status, last_test_at, custom, \
                context_length, max_output_tokens, capabilities_json, \
                family, model_type, input_modalities_json, \
                output_modalities_json \
         FROM models WHERE id IN ({})",
        placeholders
    );
    let mut stmt = conn.prepare_cached(&query).map_err(map_db_error)?;
    let ids: Vec<&dyn rusqlite::ToSql> = row_ids
        .iter()
        .map(|id| &id.0 as &dyn rusqlite::ToSql)
        .collect();
    let rows = stmt.query_map(&*ids, map_row).map_err(map_db_error)?;
    let mut models = Vec::with_capacity(row_ids.len());
    for row in rows {
        models.push(row.map_err(map_db_error)?);
    }
    Ok(models)
}

pub fn find_active_by_name(conn: &Connection, model_id: &str) -> Result<Option<Model>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, provider_id, model_id, display_name, target_format, \
                    discovered_at, expires_at, timeout_overrides_json, active, \
                    last_test_status, last_test_at, custom, \
                    context_length, max_output_tokens, capabilities_json, \
                    family, model_type, input_modalities_json, \
                    output_modalities_json \
             FROM models \
             WHERE model_id = ?1 \
               AND active = 1 \
             ORDER BY id ASC \
             LIMIT 1",
        )
        .map_err(map_db_error)?;
    let mut rows = stmt.query_map([model_id], map_row).map_err(map_db_error)?;
    match rows.next() {
        Some(row) => Ok(Some(row.map_err(map_db_error)?)),
        None => Ok(None),
    }
}

pub fn find_active_by_provider_and_name(
    conn: &Connection,
    provider_id: &ProviderId,
    model_id: &str,
) -> Result<Option<Model>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, provider_id, model_id, display_name, target_format, \
                    discovered_at, expires_at, timeout_overrides_json, active, \
                    last_test_status, last_test_at, custom, \
                    context_length, max_output_tokens, capabilities_json, \
                    family, model_type, input_modalities_json, \
                    output_modalities_json \
             FROM models \
             WHERE provider_id = ?1 \
               AND model_id = ?2 \
               AND active = 1 \
             ORDER BY id ASC \
             LIMIT 1",
        )
        .map_err(map_db_error)?;
    let mut rows = stmt
        .query_map(params![provider_id.as_str(), model_id], map_row)
        .map_err(map_db_error)?;
    match rows.next() {
        Some(row) => Ok(Some(row.map_err(map_db_error)?)),
        None => Ok(None),
    }
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
    let tx = conn.unchecked_transaction().map_err(map_db_error)?;

    let removed = tx
        .execute("DELETE FROM models WHERE id = ?1", params![id.0])
        .map_err(map_db_error_ctx(format!("delete model {}", id.0)))?;

    tx.commit().map_err(map_db_error)?;

    Ok(removed as u64)
}

pub fn create_custom(
    conn: &Connection,
    provider_id: &ProviderId,
    model_id: &ModelId,
    display_name: Option<&str>,
    target_format: TargetFormat,
    ttl_seconds: i64,
) -> Result<ModelRowId> {
    let expires_expr = if ttl_seconds <= 0 {
        "NULL".to_string()
    } else {
        format!("datetime('now', '+' || {} || ' seconds')", ttl_seconds)
    };

    let normalized = normalize_model_id(model_id.as_str());
    let sql = format!(
        "INSERT INTO models \
            (provider_id, model_id, display_name, target_format, \
             discovered_at, expires_at, active, custom, model_id_normalized) \
         VALUES (?1, ?2, ?3, ?4, datetime('now'), {expires_expr}, 1, 1, ?5) \
         ON CONFLICT(provider_id, model_id) DO UPDATE SET \
            display_name = excluded.display_name, \
            target_format = excluded.target_format, \
            discovered_at = datetime('now'), \
            expires_at = {expires_expr}, \
            active = 1, \
            custom = 1, \
            model_id_normalized = COALESCE(excluded.model_id_normalized, model_id_normalized) \
         RETURNING id",
    );

    let row_id: i64 = conn
        .query_row(
            &sql,
            params![
                provider_id.as_str(),
                model_id.as_str(),
                display_name,
                target_format.as_str(),
                &normalized,
            ],
            |r| r.get(0),
        )
        .map_err(|e| {
            let msg = e.to_string();
            if msg.contains("FOREIGN KEY") {
                openproxy_types::CoreError::Validation(format!(
                    "provider_id does not exist: {}",
                    provider_id
                ))
            } else {
                openproxy_types::CoreError::Database {
                    message: format!("create_custom model for {}: {}", provider_id, e),
                    source: Some(Box::new(e)),
                }
            }
        })?;

    Ok(ModelRowId(row_id))
}

pub fn apply_auto_activation(
    conn: &Connection,
    provider: &ProviderId,
    keyword: Option<&str>,
) -> Result<u64> {
    let tx = conn.unchecked_transaction().map_err(map_db_error)?;

    let newly_active: Vec<(String, Option<String>)> = match keyword {
        Some(k) => {
            let mut stmt = tx
                .prepare(
                    "SELECT model_id, display_name FROM models \
                     WHERE provider_id = ?1 AND custom = 0 \
                       AND discovered_at >= datetime('now', '-60 seconds') \
                       AND active = 0 \
                       AND model_id LIKE '%' || ?2 || '%'",
                )
                .map_err(map_db_error)?;
            let rows = stmt
                .query_map(params![provider.as_str(), k], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
                })
                .map_err(map_db_error)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(map_db_error)?);
            }
            out
        }
        None => {
            let mut stmt = tx
                .prepare(
                    "SELECT model_id, display_name FROM models \
                     WHERE provider_id = ?1 AND custom = 0 \
                       AND discovered_at >= datetime('now', '-60 seconds') \
                       AND active = 0",
                )
                .map_err(map_db_error)?;
            let rows = stmt
                .query_map(params![provider.as_str()], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
                })
                .map_err(map_db_error)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(map_db_error)?);
            }
            out
        }
    };

    let updated = match keyword {
        Some(k) => tx.execute(
            "UPDATE models \
              SET active = CASE WHEN model_id LIKE '%' || ?1 || '%' THEN 1 ELSE 0 END \
              WHERE provider_id = ?2 \
                AND custom = 0 \
                AND discovered_at >= datetime('now', '-60 seconds')",
            params![k, provider.as_str()],
        ),
        None => tx.execute(
            "UPDATE models SET active = 1 \
              WHERE provider_id = ?1 \
                AND custom = 0 \
                AND discovered_at >= datetime('now', '-60 seconds')",
            params![provider.as_str()],
        ),
    }
    .map_err(map_db_error_ctx(format!(
        "apply_auto_activation for {}",
        provider
    )))?;

    let notifications_present: bool = tx
        .query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = 'notifications'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n != 0)
        .unwrap_or(false);

    if notifications_present {
        for (model_id, display_name) in &newly_active {
            let payload = serde_json::json!({
                "provider_id": provider.as_str(),
                "model_id": model_id,
                "display_name": display_name,
                "matched_keyword": keyword,
            });
            let dedup = format!("{}:{}:auto", provider.as_str(), model_id);
            let _ = tx.execute(
                "INSERT OR IGNORE INTO notifications (kind, payload_json, dedup_key, provider_id) VALUES (?1, ?2, ?3, ?4)",
                params!["model_auto_activated", payload.to_string(), dedup, provider.as_str()],
            );
        }
    }

    tx.commit().map_err(map_db_error)?;

    Ok(updated as u64)
}

pub fn upsert_many(
    conn: &Connection,
    provider: &ProviderId,
    discovered: &[DiscoveredModel],
    ttl: Duration,
) -> Result<UpsertResult> {
    let ttl_secs = ttl.as_secs() as i64;
    let mut total = 0usize;
    let mut new_model_ids: Vec<ModelId> = Vec::new();
    let mut inserted_model_ids: Vec<&str> = Vec::new();

    let existing_rows: Vec<(String, i64, Option<String>)> = {
        let mut stmt = conn
            .prepare("SELECT model_id, id, display_name FROM models WHERE provider_id = ?")
            .map_err(map_db_error)?;
        let rows = stmt
            .query_map([provider.as_str()], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })
            .map_err(map_db_error)?;
        let mut out = Vec::new();
        for id in rows {
            out.push(id.map_err(map_db_error)?);
        }
        out
    };

    let existing: std::collections::HashSet<&str> =
        existing_rows.iter().map(|(m, _, _)| m.as_str()).collect();

    let tx = conn.unchecked_transaction().map_err(map_db_error)?;

    {
        let mut stmt = tx
            .prepare(
                "INSERT INTO models (\
                    provider_id, model_id, display_name, target_format, \
                    discovered_at, expires_at, \
                    context_length, max_output_tokens, \
                    input_modalities_json, output_modalities_json, \
                    model_type, family, capabilities_json, model_id_normalized\
                 ) VALUES (\
                    ?, ?, ?, ?, datetime('now'), datetime('now', '+' || ? || ' seconds'), \
                    ?, ?, ?, ?, COALESCE(?, 'chat'), ?, ?, ?\
                 ) ON CONFLICT(provider_id, model_id) DO UPDATE SET \
                    display_name = excluded.display_name, \
                    target_format = excluded.target_format, \
                    context_length = COALESCE(excluded.context_length, context_length), \
                    max_output_tokens = COALESCE(excluded.max_output_tokens, max_output_tokens), \
                    input_modalities_json = COALESCE(excluded.input_modalities_json, input_modalities_json), \
                    output_modalities_json = COALESCE(excluded.output_modalities_json, output_modalities_json), \
                    model_type = COALESCE(excluded.model_type, model_type), \
                    family = COALESCE(excluded.family, family), \
                    capabilities_json = COALESCE(excluded.capabilities_json, capabilities_json), \
                    model_id_normalized = COALESCE(excluded.model_id_normalized, model_id_normalized)",
            )
            .map_err(map_db_error)?;

        for d in discovered {
            let caps_json = d.capabilities.as_ref().and_then(|c| c.to_json());
            let input_mods_json = d
                .input_modalities
                .as_ref()
                .and_then(|v| serde_json::to_string(v).ok());
            let output_mods_json = d
                .output_modalities
                .as_ref()
                .and_then(|v| serde_json::to_string(v).ok());

            let is_new = !existing.contains(d.model_id.as_str());
            if is_new {
                new_model_ids.push(d.model_id.clone());
                inserted_model_ids.push(d.model_id.as_str());
            }

            let normalized = normalize_model_id(d.model_id.as_str());

            let changed = stmt
                .execute(params![
                    provider.as_str(),
                    d.model_id.as_str(),
                    d.display_name,
                    d.target_format.as_str(),
                    ttl_secs,
                    d.context_length,
                    d.max_output_tokens,
                    input_mods_json,
                    output_mods_json,
                    d.model_type,
                    d.family,
                    caps_json,
                    &normalized,
                ])
                .map_err(map_db_error)?;
            total += changed;
        }
    }

    if discovered.is_empty() {
        tx.execute(
            "DELETE FROM models WHERE provider_id = ?1 AND custom = 0",
            params![provider.as_str()],
        )
        .map_err(map_db_error)?;
    } else {
        let discovered_ids: Vec<&str> = discovered.iter().map(|d| d.model_id.as_str()).collect();
        let discovered_json =
            serde_json::to_string(&discovered_ids).unwrap_or_else(|_| "[]".to_string());
        let sql = "DELETE FROM models \
             WHERE provider_id = ? AND custom = 0 \
               AND model_id NOT IN (SELECT value FROM json_each(?))";
        tx.execute(sql, params![provider.as_str(), discovered_json])
            .map_err(map_db_error)?;
    }

    if !inserted_model_ids.is_empty() {
        let inserted_json =
            serde_json::to_string(&inserted_model_ids).unwrap_or_else(|_| "[]".to_string());
        let sql = "SELECT id, model_id FROM models \
             WHERE provider_id = ? AND model_id IN (SELECT value FROM json_each(?))";
        let mut stmt = tx.prepare(sql).map_err(map_db_error)?;
        let rows = stmt
            .query_map(params![provider.as_str(), inserted_json], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(map_db_error)?;
        let mut new_rows: Vec<(i64, String)> = Vec::new();
        for row in rows {
            new_rows.push(row.map_err(map_db_error)?);
        }
        drop(stmt);

        let combo_targets_present: bool = tx
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master \
                 WHERE type = 'table' AND name = 'combo_targets'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n != 0)
            .unwrap_or(false);

        if combo_targets_present {
            for (new_id, upstream) in &new_rows {
                let _ = crate::combos::reconnect_orphan_targets(
                    &tx,
                    provider,
                    upstream,
                    ModelRowId(*new_id),
                )?;
            }
        }
    }

    tx.commit().map_err(map_db_error)?;

    Ok(UpsertResult {
        touched: total,
        new_model_ids,
    })
}

/// Model repository trait.
pub trait ModelRepository: Send + Sync {
    fn list_active(&self, provider: &ProviderId) -> Result<Vec<Model>>;
    fn list_active_all(&self) -> Result<Vec<Model>>;
    fn list_all(&self) -> Result<Vec<Model>>;
    fn get_by_row_id(&self, row_id: ModelRowId) -> Result<Option<Model>>;
    fn find_active_by_name(&self, model_id: &str) -> Result<Option<Model>>;
    fn find_active_by_provider_and_name(
        &self,
        provider: &ProviderId,
        model_id: &str,
    ) -> Result<Option<Model>>;
    fn set_active(&self, id: ModelRowId, active: bool) -> Result<()>;
    fn set_active_bulk(&self, provider: &ProviderId, active: bool) -> Result<u64>;
    fn set_test_status(&self, id: ModelRowId, status: i32) -> Result<()>;
    fn delete(&self, id: ModelRowId) -> Result<u64>;
    fn create_custom(
        &self,
        provider_id: &ProviderId,
        model_id: &ModelId,
        display_name: Option<&str>,
        target_format: TargetFormat,
        ttl_seconds: i64,
    ) -> Result<ModelRowId>;
    fn mark_expired(&self) -> Result<usize>;
    fn upsert_many(
        &self,
        provider: &ProviderId,
        discovered: &[DiscoveredModel],
        ttl: Duration,
    ) -> Result<UpsertResult>;
    fn apply_auto_activation(&self, provider: &ProviderId, keyword: Option<&str>) -> Result<u64>;
}

/// Concrete SQLite repository implementation.
pub struct SqliteModelRepository {
    pool: Arc<DbPool>,
}

impl SqliteModelRepository {
    pub fn new(pool: Arc<DbPool>) -> Self {
        Self { pool }
    }
}

impl ModelRepository for SqliteModelRepository {
    fn list_active(&self, provider: &ProviderId) -> Result<Vec<Model>> {
        let conn = self.pool.open_connection()?;
        list_active(&conn, provider)
    }

    fn list_active_all(&self) -> Result<Vec<Model>> {
        let conn = self.pool.open_connection()?;
        list_active_all(&conn)
    }

    fn list_all(&self) -> Result<Vec<Model>> {
        let conn = self.pool.open_connection()?;
        list_all(&conn)
    }

    fn get_by_row_id(&self, row_id: ModelRowId) -> Result<Option<Model>> {
        let conn = self.pool.open_connection()?;
        get_by_row_id(&conn, row_id)
    }

    fn find_active_by_name(&self, model_id: &str) -> Result<Option<Model>> {
        let conn = self.pool.open_connection()?;
        find_active_by_name(&conn, model_id)
    }

    fn find_active_by_provider_and_name(
        &self,
        provider: &ProviderId,
        model_id: &str,
    ) -> Result<Option<Model>> {
        let conn = self.pool.open_connection()?;
        find_active_by_provider_and_name(&conn, provider, model_id)
    }

    fn set_active(&self, id: ModelRowId, active: bool) -> Result<()> {
        let conn = self.pool.open_connection()?;
        set_active(&conn, id, active)
    }

    fn set_active_bulk(&self, provider: &ProviderId, active: bool) -> Result<u64> {
        let conn = self.pool.open_connection()?;
        set_active_bulk(&conn, provider, active)
    }

    fn set_test_status(&self, id: ModelRowId, status: i32) -> Result<()> {
        let conn = self.pool.open_connection()?;
        set_test_status(&conn, id, status)
    }

    fn delete(&self, id: ModelRowId) -> Result<u64> {
        let conn = self.pool.open_connection()?;
        delete(&conn, id)
    }

    fn create_custom(
        &self,
        provider_id: &ProviderId,
        model_id: &ModelId,
        display_name: Option<&str>,
        target_format: TargetFormat,
        ttl_seconds: i64,
    ) -> Result<ModelRowId> {
        let conn = self.pool.open_connection()?;
        create_custom(
            &conn,
            provider_id,
            model_id,
            display_name,
            target_format,
            ttl_seconds,
        )
    }

    fn mark_expired(&self) -> Result<usize> {
        let conn = self.pool.open_connection()?;
        mark_expired(&conn)
    }

    fn upsert_many(
        &self,
        provider: &ProviderId,
        discovered: &[DiscoveredModel],
        ttl: Duration,
    ) -> Result<UpsertResult> {
        let conn = self.pool.open_connection()?;
        upsert_many(&conn, provider, discovered, ttl)
    }

    fn apply_auto_activation(&self, provider: &ProviderId, keyword: Option<&str>) -> Result<u64> {
        let conn = self.pool.open_connection()?;
        apply_auto_activation(&conn, provider, keyword)
    }
}
