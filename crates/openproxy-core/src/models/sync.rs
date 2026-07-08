use crate::error::{Result};
use crate::ids::{ModelRowId, ProviderId};
use crate::models::{DiscoveredModel, UpsertResult};
use rusqlite::{Connection, params};
use std::time::Duration;

pub struct SyncDiff<'a> {
    pub discovered_set: std::collections::HashSet<&'a str>,
    pub new_models: Vec<&'a DiscoveredModel>,
    // Store the owned data here so we don't have to clone it into deleted_models
    pub existing_rows: Vec<(String, i64, Option<String>)>,
}

impl<'a> SyncDiff<'a> {
    pub fn deleted_models(&self) -> impl Iterator<Item = (&str, Option<&str>)> {
        self.existing_rows
            .iter()
            .filter(|(m, _, _)| !self.discovered_set.contains(m.as_str()))
            .map(|(m, _, dn)| (m.as_str(), dn.as_deref()))
    }
}

pub fn compute_diff<'a>(
    conn: &Connection,
    provider: &ProviderId,
    discovered: &'a [DiscoveredModel],
) -> Result<SyncDiff<'a>> {
    let existing_rows: Vec<(String, i64, Option<String>)> = {
        let mut stmt = conn
            .prepare("SELECT model_id, id, display_name FROM models WHERE provider_id = ?")
            .map_err(crate::error::map_db_error)?;
        let rows = stmt
            .query_map([provider.as_str()], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })
            .map_err(crate::error::map_db_error)?;
        let mut out = Vec::new();
        for id in rows {
            out.push(id.map_err(crate::error::map_db_error)?);
        }
        out
    };

    let existing: std::collections::HashSet<&str> =
        existing_rows.iter().map(|(m, _, _)| m.as_str()).collect();

    let discovered_set: std::collections::HashSet<&str> =
        discovered.iter().map(|d| d.model_id.as_str()).collect();

    let mut new_models = Vec::new();
    for d in discovered {
        if !existing.contains(d.model_id.as_str()) {
            new_models.push(d);
        }
    }

    Ok(SyncDiff {
        discovered_set,
        new_models,
        existing_rows,
    })
}

#[allow(clippy::type_complexity)]
pub fn execute_sync_transaction(
    conn: &Connection,
    provider: &ProviderId,
    discovered: &[DiscoveredModel],
    diff: &SyncDiff,
    ttl: Duration,
) -> Result<(UpsertResult, Vec<(i64, &'static str, serde_json::Value)>)> {
    let mut total = 0usize;
    let mut new_model_ids: Vec<crate::ids::ModelId> = Vec::new();
    let ttl_secs = ttl.as_secs() as i64;
    let mut inserted_model_ids: Vec<&str> = Vec::new();

    let tx = conn
        .unchecked_transaction()
        .map_err(crate::error::map_db_error)?;

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
            .map_err(crate::error::map_db_error)?;

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

            let is_new = diff.new_models.iter().any(|n| n.model_id == d.model_id);
            if is_new {
                new_model_ids.push(d.model_id.clone());
                inserted_model_ids.push(d.model_id.as_str());
            }

            let normalized = crate::model_normalize::normalize_model_id(d.model_id.as_str());

            let changed = stmt
                .execute(params![
                    provider.as_str(),        // 1. provider_id
                    d.model_id.as_str(),      // 2. model_id
                    d.display_name,           // 3. display_name
                    d.target_format.as_str(), // 4. target_format
                    ttl_secs,                 // 5. (used in the datetime '+? seconds' expr)
                    d.context_length,         // 6. context_length
                    d.max_output_tokens,      // 7. max_output_tokens
                    input_mods_json,          // 8. input_modalities_json
                    output_mods_json,         // 9. output_modalities_json
                    d.model_type,             // 10. model_type
                    d.family,                 // 11. family
                    caps_json,                // 12. capabilities_json
                    &normalized,              // 13. model_id_normalized
                ])
                .map_err(crate::error::map_db_error)?;
            total += changed;
        }
    }

    if discovered.is_empty() {
        tx.execute(
            "DELETE FROM models WHERE provider_id = ?1 AND custom = 0",
            params![provider.as_str()],
        )
        .map_err(crate::error::map_db_error)?;
    } else {
        let discovered_ids: Vec<&str> = discovered.iter().map(|d| d.model_id.as_str()).collect();
        let discovered_json =
            serde_json::to_string(&discovered_ids).unwrap_or_else(|_| "[]".to_string());
        let sql = "DELETE FROM models \
             WHERE provider_id = ? AND custom = 0 \
               AND model_id NOT IN (SELECT value FROM json_each(?))";
        tx.execute(sql, params![provider.as_str(), discovered_json])
            .map_err(crate::error::map_db_error)?;
    }

    let events = generate_events(&tx, provider, diff)?;

    if !inserted_model_ids.is_empty() {
        let inserted_json =
            serde_json::to_string(&inserted_model_ids).unwrap_or_else(|_| "[]".to_string());
        let sql = "SELECT id, model_id FROM models \
             WHERE provider_id = ? AND model_id IN (SELECT value FROM json_each(?))";
        let mut stmt = tx.prepare(sql).map_err(crate::error::map_db_error)?;
        let rows = stmt
            .query_map(params![provider.as_str(), inserted_json], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(crate::error::map_db_error)?;
        let mut new_rows: Vec<(i64, String)> = Vec::new();
        for row in rows {
            new_rows.push(row.map_err(crate::error::map_db_error)?);
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
                let updated = crate::combos::reconnect_orphan_targets(
                    &tx,
                    provider,
                    upstream,
                    ModelRowId(*new_id),
                )?;
                if updated > 0 {
                    tracing::info!(
                        target: "openproxy.core.models",
                        provider = %provider,
                        upstream_model_id = %upstream,
                        new_model_row_id = new_id,
                        reconnected_targets = updated,
                        "gate F1: reconnected orphan combo_targets to re-inserted model",
                    );
                }
            }
        }
    }

    tx.commit().map_err(crate::error::map_db_error)?;

    Ok((
        UpsertResult {
            touched: total,
            new_model_ids,
        },
        events,
    ))
}

pub fn generate_events(
    tx: &rusqlite::Transaction,
    provider: &ProviderId,
    diff: &SyncDiff,
) -> Result<Vec<(i64, &'static str, serde_json::Value)>> {
    let mut events = Vec::new();

    for d in &diff.new_models {
        let payload = serde_json::json!({
            "provider_id": provider.as_str(),
            "model_id": d.model_id.as_str(),
            "display_name": d.display_name,
            "target_format": d.target_format.as_str(),
            "context_length": d.context_length,
        });
        let dedup = format!("{}:{}", provider.as_str(), d.model_id.as_str());
        if let Some(id) = crate::notifications::insert(
            tx,
            crate::notifications::KIND_MODEL_NEW,
            &payload,
            Some(&dedup),
            Some(provider.as_str()),
        )
        .ok()
        .flatten()
        {
            events.push((id, crate::notifications::KIND_MODEL_NEW, payload));
        }
    }

    for (model_id, display_name) in diff.deleted_models() {
        let payload = serde_json::json!({
            "provider_id": provider.as_str(),
            "model_id": model_id,
            "display_name": display_name,
        });
        let dedup = format!("{}:{}", provider.as_str(), model_id);
        if let Some(id) = crate::notifications::insert(
            tx,
            crate::notifications::KIND_MODEL_GONE,
            &payload,
            Some(&dedup),
            Some(provider.as_str()),
        )
        .ok()
        .flatten()
        {
            events.push((id, crate::notifications::KIND_MODEL_GONE, payload));
        }
    }

    Ok(events)
}

pub fn broadcast_notifications(
    conn: &Connection,
    events: &[(i64, &'static str, serde_json::Value)],
) {
    for (id, kind, payload) in events {
        let _ = crate::notifications::broadcast_one(conn, *id, kind, payload);
    }
}
