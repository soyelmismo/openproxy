//! Database access layer and repository for the `models` table.

use openproxy_types::{
    DiscoveredModel, Model, ModelId, ModelRowId, ProviderId, Result, TargetFormat, UpsertResult,
    normalize_model_id,
};
use rusqlite::{Connection, Row, params};
use std::sync::Arc;
use std::time::Duration;

use crate::conn::DbPool;
use crate::error::{map_db_error, map_db_error_ctx};

fn map_row(row: &Row<'_>) -> rusqlite::Result<Model> {
    crate::map_row_struct!(row, Model {
        row_id: @id(0, ModelRowId),
        provider_id: @id_str(1, ProviderId),
        model_id: @id_str(2, ModelId),
        display_name: @opt_box_str(3),
        target_format: @enum_parse(4, TargetFormat),
        discovered_at: @box_str(5),
        expires_at: @opt_box_str(6),
        timeout_overrides_json: @opt_box_str(7),
        active: @bool(8),
        last_test_status: 9,
        last_test_at: @opt_box_str(10),
        custom: @bool(11),
        context_length: 12,
        max_output_tokens: 13,
        capabilities_json: @opt_box_str(14),
        family: @opt_box_str(15),
        model_type: @box_str_default(16, "chat"),
        input_modalities_json: @opt_box_str(17),
        output_modalities_json: @opt_box_str(18),
        manually_disabled_at: @opt_box_str(19),
    })
}

crate::def_table_select!(
    model_select,
    "models",
    "id, provider_id, model_id, display_name, target_format, \
     discovered_at, expires_at, timeout_overrides_json, active, \
     last_test_status, last_test_at, custom, \
     context_length, max_output_tokens, capabilities_json, \
     family, model_type, input_modalities_json, \
     output_modalities_json, \
     manually_disabled_at"
);

crate::def_table_select!(model_auto_active_select, "models", "model_id, display_name");

crate::def_table_select!(
    model_existing_select,
    "models",
    "model_id, id, display_name"
);

crate::def_table_select!(model_inserted_select, "models", "id, model_id");

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

pub fn set_active(conn: &Connection, id: ModelRowId, active: bool) -> Result<()> {
    let bit = i64::from(active);
    conn.execute(
        "UPDATE models \
         SET active = ?1, \
             manually_disabled_at = CASE WHEN ?1 = 1 THEN NULL ELSE datetime('now') END \
         WHERE id = ?2",
        params![bit, id.0],
    )
    .map_err(map_db_error_ctx(format!(
        "update active for model {}",
        id.0
    )))?;
    Ok(())
}

pub fn set_active_bulk(conn: &Connection, provider: &ProviderId, active: bool) -> Result<u64> {
    let bit = i64::from(active);
    let n = conn
        .execute(
            "UPDATE models \
             SET active = ?1, \
                 manually_disabled_at = CASE WHEN ?1 = 1 THEN NULL ELSE datetime('now') END \
             WHERE provider_id = ?2 AND custom = 0",
            params![bit, provider.as_str()],
        )
        .map_err(map_db_error_ctx(format!("set_active_bulk for {provider}")))?;
    Ok(n as u64)
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

fn query_newly_active_models(
    tx: &rusqlite::Transaction,
    provider: &ProviderId,
    keyword: Option<&str>,
) -> Result<Vec<(String, Option<String>)>> {
    match keyword {
        Some(k) => crate::db_query_all!(
            tx,
            model_auto_active_select!(
                "WHERE provider_id = ?1 AND custom = 0 \
                 AND discovered_at >= datetime('now', '-60 seconds') \
                 AND active = 0 \
                 AND manually_disabled_at IS NULL \
                 AND model_id LIKE '%' || ?2 || '%'"
            ),
            params![provider.as_str(), k],
            |r| crate::map_row_tuple!(r => (0, 1)),
            "query newly active models with keyword"
        ),
        None => crate::db_query_all!(
            tx,
            model_auto_active_select!(
                "WHERE provider_id = ?1 AND custom = 0 \
                 AND discovered_at >= datetime('now', '-60 seconds') \
                 AND active = 0 \
                 AND manually_disabled_at IS NULL"
            ),
            params![provider.as_str()],
            |r| crate::map_row_tuple!(r => (0, 1)),
            "query newly active models"
        ),
    }
}

fn update_models_active_status(
    tx: &rusqlite::Transaction,
    provider: &ProviderId,
    keyword: Option<&str>,
) -> Result<usize> {
    match keyword {
        Some(k) => tx.execute(
            "UPDATE models \
              SET active = CASE WHEN model_id LIKE '%' || ?1 || '%' THEN 1 ELSE 0 END \
              WHERE provider_id = ?2 \
                AND custom = 0 \
                AND discovered_at >= datetime('now', '-60 seconds') \
                AND manually_disabled_at IS NULL",
            params![k, provider.as_str()],
        ),
        None => tx.execute(
            "UPDATE models SET active = 1 \
              WHERE provider_id = ?1 \
                AND custom = 0 \
                AND discovered_at >= datetime('now', '-60 seconds') \
                AND manually_disabled_at IS NULL",
            params![provider.as_str()],
        ),
    }
    .map_err(map_db_error_ctx(format!(
        "apply_auto_activation for {provider}"
    )))
}

fn notify_auto_activated_models(
    tx: &rusqlite::Transaction,
    provider: &ProviderId,
    keyword: Option<&str>,
    newly_active: &[(String, Option<String>)],
) -> Result<()> {
    let notifications_present: bool = tx
        .query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = 'notifications'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .is_ok_and(|n| n != 0);

    if !notifications_present || newly_active.is_empty() {
        return Ok(());
    }

    let already_notified: std::collections::HashSet<String> = {
        let mut stmt = tx
            .prepare(
                "SELECT dedup_key FROM notifications \
                 WHERE kind = 'model_auto_activated' AND provider_id = ?1 AND dedup_key IS NOT NULL",
            )
            .map_err(map_db_error)?;
        let rows = stmt
            .query_map(params![provider.as_str()], |r| r.get::<_, String>(0))
            .map_err(map_db_error)?;
        rows.filter_map(std::result::Result::ok).collect()
    };

    let to_notify: Vec<_> = newly_active
        .iter()
        .filter(|(model_id, _)| {
            let dedup = format!("{}:{}:auto", provider.as_str(), model_id);
            !already_notified.contains(&dedup)
        })
        .collect();

    if !to_notify.is_empty() {
        let _ = crate::batch::batch_insert(
            tx,
            "INSERT OR IGNORE INTO",
            "notifications",
            &["kind", "payload_json", "dedup_key", "provider_id"],
            &to_notify,
            None,
            |(model_id, display_name), query_params| {
                let payload = serde_json::json!({
                    "provider_id": provider.as_str(),
                    "model_id": model_id,
                    "display_name": display_name,
                    "matched_keyword": keyword,
                });
                let dedup = format!("{}:{}:auto", provider.as_str(), model_id);

                query_params.push(rusqlite::types::Value::Text(
                    "model_auto_activated".to_string(),
                ));
                query_params.push(rusqlite::types::Value::Text(payload.to_string()));
                query_params.push(rusqlite::types::Value::Text(dedup));
                query_params.push(rusqlite::types::Value::Text(provider.as_str().to_string()));
            },
        );
    }
    Ok(())
}

pub fn apply_auto_activation(
    conn: &Connection,
    provider: &ProviderId,
    keyword: Option<&str>,
) -> Result<u64> {
    let tx = conn.unchecked_transaction().map_err(map_db_error)?;
    let newly_active = query_newly_active_models(&tx, provider, keyword)?;
    let updated = update_models_active_status(&tx, provider, keyword)?;
    notify_auto_activated_models(&tx, provider, keyword, &newly_active)?;
    tx.commit().map_err(map_db_error)?;
    Ok(updated as u64)
}

pub use crate::error::BUSY_RETRY_DELAYS;

/// `apply_auto_activation` with automatic retry on transient SQLite
/// BUSY/LOCKED via [`crate::error::with_busy_retry`].
pub fn apply_auto_activation_with_retry(
    conn: &Connection,
    provider: &ProviderId,
    keyword: Option<&str>,
) -> Result<u64> {
    crate::error::with_busy_retry("apply_auto_activation", || {
        apply_auto_activation(conn, provider, keyword)
    })
}

fn fetch_existing_model_ids(
    conn: &Connection,
    provider: &ProviderId,
) -> Result<std::collections::HashSet<String>> {
    let existing_rows: Vec<(String, i64, Option<String>)> = crate::db_query_all!(
        conn,
        model_existing_select!("WHERE provider_id = ?1"),
        params![provider.as_str()],
        |r| crate::map_row_tuple!(r => (0, 1, 2)),
        "query existing models"
    )?;

    Ok(existing_rows.into_iter().map(|(m, _, _)| m).collect())
}

fn upsert_discovered_models<'a>(
    tx: &rusqlite::Transaction,
    provider: &ProviderId,
    discovered: &'a [DiscoveredModel],
    ttl_secs: i64,
    existing: &std::collections::HashSet<String>,
    new_model_ids: &mut Vec<ModelId>,
    inserted_model_ids: &mut Vec<&'a str>,
) -> Result<usize> {
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
                model_type = CASE \
                    WHEN models.custom = 1 THEN COALESCE(models.model_type, excluded.model_type) \
                    WHEN models.model_type = 'audio' AND excluded.model_type = 'chat' THEN excluded.model_type \
                    WHEN models.model_type = 'chat' AND excluded.model_type != 'chat' THEN excluded.model_type \
                    WHEN models.model_type = 'embedding' AND excluded.model_type = 'rerank' THEN excluded.model_type \
                    ELSE COALESCE(models.model_type, excluded.model_type) \
                END, \
                family = COALESCE(excluded.family, family), \
                capabilities_json = COALESCE(excluded.capabilities_json, capabilities_json), \
                model_id_normalized = COALESCE(excluded.model_id_normalized, model_id_normalized)",
        )
        .map_err(map_db_error)?;

    let mut total = 0;
    for d in discovered {
        let caps_json = d
            .capabilities
            .as_ref()
            .and_then(openproxy_types::ModelCapabilities::to_json);
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
    Ok(total)
}

fn prune_obsolete_models(
    tx: &rusqlite::Transaction,
    provider: &ProviderId,
    discovered: &[DiscoveredModel],
) -> Result<()> {
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
    Ok(())
}

fn reconnect_inserted_combo_targets(
    tx: &rusqlite::Transaction,
    provider: &ProviderId,
    inserted_model_ids: &[&str],
) -> Result<()> {
    if inserted_model_ids.is_empty() {
        return Ok(());
    }

    let inserted_json =
        serde_json::to_string(inserted_model_ids).unwrap_or_else(|_| "[]".to_string());
    let new_rows: Vec<(i64, String)> = crate::db_query_all!(
        tx,
        model_inserted_select!(
            "WHERE provider_id = ?1 AND model_id IN (SELECT value FROM json_each(?2))"
        ),
        params![provider.as_str(), inserted_json],
        |r| crate::map_row_tuple!(r => (0, 1)),
        "query inserted models"
    )?;

    let combo_targets_present: bool = tx
        .query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master \
             WHERE type = 'table' AND name = 'combo_targets'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .is_ok_and(|n| n != 0);

    if combo_targets_present {
        for (new_id, upstream) in &new_rows {
            let _ = crate::combos::reconnect_orphan_targets(
                tx,
                provider,
                upstream,
                ModelRowId(*new_id),
            )?;
        }
    }
    Ok(())
}

pub fn upsert_many(
    conn: &Connection,
    provider: &ProviderId,
    discovered: &[DiscoveredModel],
    ttl: Duration,
) -> Result<UpsertResult> {
    crate::error::with_busy_retry("models_upsert_many", || {
        let ttl_secs = ttl.as_secs() as i64;
        let existing = fetch_existing_model_ids(conn, provider)?;
        let tx = conn.unchecked_transaction().map_err(map_db_error)?;

        let mut new_model_ids: Vec<ModelId> = Vec::new();
        let mut inserted_model_ids: Vec<&str> = Vec::new();

        let total = upsert_discovered_models(
            &tx,
            provider,
            discovered,
            ttl_secs,
            &existing,
            &mut new_model_ids,
            &mut inserted_model_ids,
        )?;

        prune_obsolete_models(&tx, provider, discovered)?;
        reconnect_inserted_combo_targets(&tx, provider, &inserted_model_ids)?;

        tx.commit().map_err(map_db_error)?;

        Ok(UpsertResult {
            touched: total,
            new_model_ids: new_model_ids.into(),
        })
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
        model_type: Option<&str>,
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
        let conn = self.pool.reader();
        list_active(&conn, provider)
    }

    fn list_active_all(&self) -> Result<Vec<Model>> {
        let conn = self.pool.reader();
        list_active_all(&conn)
    }

    fn list_all(&self) -> Result<Vec<Model>> {
        let conn = self.pool.reader();
        list_all(&conn)
    }

    fn get_by_row_id(&self, row_id: ModelRowId) -> Result<Option<Model>> {
        let conn = self.pool.reader();
        get_by_row_id(&conn, row_id)
    }

    fn find_active_by_name(&self, model_id: &str) -> Result<Option<Model>> {
        let conn = self.pool.reader();
        find_active_by_name(&conn, model_id)
    }

    fn find_active_by_provider_and_name(
        &self,
        provider: &ProviderId,
        model_id: &str,
    ) -> Result<Option<Model>> {
        let conn = self.pool.reader();
        find_active_by_provider_and_name(&conn, provider, model_id)
    }

    fn set_active(&self, id: ModelRowId, active: bool) -> Result<()> {
        let conn = self.pool.writer();
        set_active(&conn, id, active)
    }

    fn set_active_bulk(&self, provider: &ProviderId, active: bool) -> Result<u64> {
        let conn = self.pool.writer();
        set_active_bulk(&conn, provider, active)
    }

    fn set_test_status(&self, id: ModelRowId, status: i32) -> Result<()> {
        let conn = self.pool.writer();
        set_test_status(&conn, id, status)
    }

    fn delete(&self, id: ModelRowId) -> Result<u64> {
        let conn = self.pool.writer();
        delete(&conn, id)
    }

    fn create_custom(
        &self,
        provider_id: &ProviderId,
        model_id: &ModelId,
        display_name: Option<&str>,
        target_format: TargetFormat,
        ttl_seconds: i64,
        model_type: Option<&str>,
    ) -> Result<ModelRowId> {
        let conn = self.pool.writer();
        create_custom(
            &conn,
            provider_id,
            model_id,
            display_name,
            target_format,
            ttl_seconds,
            model_type,
        )
    }

    fn mark_expired(&self) -> Result<usize> {
        let conn = self.pool.writer();
        mark_expired(&conn)
    }

    fn upsert_many(
        &self,
        provider: &ProviderId,
        discovered: &[DiscoveredModel],
        ttl: Duration,
    ) -> Result<UpsertResult> {
        let conn = self.pool.writer();
        upsert_many(&conn, provider, discovered, ttl)
    }

    fn apply_auto_activation(&self, provider: &ProviderId, keyword: Option<&str>) -> Result<u64> {
        let conn = self.pool.writer();
        apply_auto_activation(&conn, provider, keyword)
    }
}

impl crate::crud::FromRow for Model {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        map_row(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conn::DbPool;
    use crate::models;
    use crate::providers::{self, NewProvider};
    use openproxy_types::error::CoreError;
    use openproxy_types::{
        AuthType, DiscoveredModel, ModelId, ProviderFormat, ProviderId as CoreProviderId,
        RateLimitScope, TargetFormat,
    };
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;
    use std::time::{Duration, Instant};

    /// Mirror of `combos::tests::fresh_pool` — every test gets an
    /// isolated file-based DB so WAL locks between tests never bleed.
    fn fresh_pool() -> (DbPool, PathBuf) {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!("openproxy-models-test-{pid}-{nanos}-{n}"));
        std::fs::create_dir_all(&dir).expect("mkdir tempdir");
        let path = dir.join("models.db");
        let pool = DbPool::open(&path).expect("open pool");
        {
            let mut w = pool.writer();
            crate::migrations::run(&mut w).expect("migrations");
        }
        (pool, path)
    }

    fn seed_provider(conn: &Connection, provider: &CoreProviderId) {
        providers::create(
            conn,
            NewProvider {
                id: provider,
                name: provider.as_str(),
                base_url: "https://example.invalid",
                auth_type: AuthType::Bearer,
                format: ProviderFormat::Openai,
                extra_headers_json: None,
                auto_activate_keyword: None,
                rate_limit_scope: RateLimitScope::Account,
            },
        )
        .expect("seed provider");
    }

    fn seed_models(conn: &Connection, provider: &CoreProviderId, ids: &[&str]) {
        models::upsert_many(
            conn,
            provider,
            &ids.iter()
                .map(|id| DiscoveredModel {
                    model_id: ModelId::new(*id),
                    display_name: Some((*id).to_string()),
                    target_format: TargetFormat::Openai,
                    context_length: None,
                    max_output_tokens: None,
                    input_modalities: None,
                    output_modalities: None,
                    model_type: None,
                    family: None,
                    capabilities: None,
                })
                .collect::<Vec<_>>(),
            Duration::from_hours(1),
        )
        .expect("upsert_many seed");
    }

    /// AC-A: happy path — the wrapper succeeds on the first attempt
    /// when no contention is present. The wrapper must not introduce
    /// any retry-side delay in the success path.
    #[test]
    fn apply_auto_activation_with_retry_succeeds_on_first_attempt() {
        let (pool, _path) = fresh_pool();
        let conn = pool.open_connection().expect("open conn");
        let provider = CoreProviderId::new("acme_ok");

        seed_provider(&conn, &provider);
        seed_models(&conn, &provider, &["gpt-4", "claude-3", "llama-3"]);

        let started = Instant::now();
        let result = apply_auto_activation_with_retry(&conn, &provider, Some("gpt"));
        let elapsed = started.elapsed();

        assert!(result.is_ok(), "expected ok, got {result:?}");
        let updated = result.unwrap();
        assert!(updated >= 1, "gpt-4 row should have been updated");
        // No retry on success — well under the 50ms minimum backoff.
        assert!(
            elapsed < Duration::from_millis(40),
            "first-attempt success should be near-instant; took {elapsed:?}",
        );
    }

    /// AC-B: when a sibling connection holds a write lock, the
    /// wrapper waits through the backoff schedule and succeeds once
    /// the lock is released. We force a BUSY collision on a sibling
    /// connection and verify the retry recovers.
    ///
    /// The retry-wrapped `conn` is configured with
    /// `busy_timeout = 0` so it returns BUSY immediately on each
    /// attempt (instead of waiting the production 5s default). The
    /// blocker holds a write transaction for 120ms — long enough
    /// to fail attempts 1 and 2, but short enough that attempt 3
    /// (fired 150ms after the start) succeeds.
    #[test]
    fn apply_auto_activation_with_retry_succeeds_after_transient_busy() {
        let (pool, _path) = fresh_pool();
        // The connection we'll retry against. Override
        // `busy_timeout` to 0 so each attempt returns BUSY
        // immediately rather than waiting the production 5s.
        let conn = pool.open_connection().expect("open conn");
        conn.pragma_update(None, "busy_timeout", 0i64)
            .expect("busy_timeout=0 on retry conn");
        let provider = CoreProviderId::new("acme_busy");

        seed_provider(&conn, &provider);
        seed_models(&conn, &provider, &["gpt-4", "claude-3"]);

        // Sibling connection that will hold a write transaction
        // for 120ms. With `busy_timeout=0` on the retry `conn`,
        // the wrapper's first two attempts return BUSY
        // immediately, but by the third attempt (fired 150ms
        // after t=0) the blocker has released the lock and the
        // wrapper succeeds.
        let blocker_path = _path;
        let blocker = std::thread::spawn(move || {
            let blocker_conn = Connection::open(&blocker_path).expect("blocker open");
            // Make the blocker's first failed write return BUSY
            // immediately instead of waiting its own 5s
            // busy_timeout — that way the test isn't dominated
            // by the producer-side wait either.
            blocker_conn
                .pragma_update(None, "busy_timeout", 0i64)
                .expect("busy_timeout=0 on blocker");
            let tx = blocker_conn.unchecked_transaction().expect("blocker tx");
            tx.execute(
                "INSERT INTO providers (id, name, base_url, auth_type, format) \
                 VALUES ('blocker', 'b', 'https://x', 'bearer', 'openai')",
                [],
            )
            .expect("blocker insert");
            std::thread::sleep(Duration::from_millis(120));
            tx.commit().expect("blocker commit");
        });

        // Give the blocker a moment to acquire its write tx.
        std::thread::sleep(Duration::from_millis(30));

        let started = Instant::now();
        let result = apply_auto_activation_with_retry(&conn, &provider, Some("gpt"));
        let elapsed = started.elapsed();

        blocker.join().expect("blocker join");

        assert!(
            result.is_ok(),
            "wrapper should recover after blocker releases lock, got {result:?}",
        );
        // Total wall time ≥ first backoff (50ms) — i.e. the
        // retry was actually exercised before the wrapper
        // succeeded.
        assert!(
            elapsed >= Duration::from_millis(50),
            "wrapper should have slept through at least one backoff; took {elapsed:?}",
        );
        // And bounded above by the cumulative backoff budget
        // (50ms + 100ms = 150ms) plus a small scheduling slack
        // for the third attempt's instant return.
        assert!(
            elapsed < Duration::from_millis(400),
            "wrapper should have recovered well within the retry budget; took {elapsed:?}",
        );
    }

    /// AC-D: non-BUSY errors (e.g. `SQLITE_ERROR` for "no such
    /// table" against a freshly-opened in-memory connection) must
    /// propagate immediately without sleeping through the backoff
    /// schedule.
    #[test]
    fn apply_auto_activation_with_retry_does_not_retry_non_busy_errors() {
        // Bare in-memory connection with no migrations applied.
        // `apply_auto_activation` will hit `SQLITE_ERROR` (no such
        // table) on its first attempt. The wrapper must NOT retry
        // this — it must propagate immediately.
        let raw = Connection::open_in_memory().expect("in-memory");
        let provider = CoreProviderId::new("acme_broken");

        let started = Instant::now();
        let result = apply_auto_activation_with_retry(&raw, &provider, Some("gpt"));
        let elapsed = started.elapsed();

        assert!(result.is_err(), "expected err, got {result:?}");
        // Non-BUSY errors must propagate in well under the 50ms
        // minimum retry delay.
        assert!(
            elapsed < Duration::from_millis(40),
            "non-BUSY error should propagate immediately; took {elapsed:?}",
        );
        // And the returned error must be a Database variant, not a
        // retry-exhausted warning.
        match result {
            Err(CoreError::Database { .. }) => {}
            other => panic!("expected Database error, got {other:?}"),
        }
    }
}
