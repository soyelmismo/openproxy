use openproxy_types::{
    DiscoveredModel, ModelId, ModelRowId, ProviderId, Result, UpsertResult, normalize_model_id,
};
use rusqlite::{Connection, params};
use std::time::Duration;

use super::{model_existing_select, model_inserted_select};
use crate::error::map_db_error;

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

    let sync_routing_overrides: std::collections::HashMap<String, openproxy_types::TargetFormat> =
        if provider.as_str().starts_with("opencode") {
            let mut overrides_stmt = tx
                .prepare(
                    "SELECT model_id, routing_format FROM model_capabilities_sync \
                     WHERE provider_id = ?1 AND routing_format IS NOT NULL",
                )
                .map_err(map_db_error)?;
            let rows = overrides_stmt
                .query_map([provider.as_str()], |row| {
                    let m_id: String = row.get(0)?;
                    let fmt_str: String = row.get(1)?;
                    Ok((m_id, fmt_str))
                })
                .map_err(map_db_error)?;
            let mut map = std::collections::HashMap::new();
            for r in rows {
                if let Ok((m_id, fmt_str)) = r
                    && let Ok(fmt) = fmt_str.parse::<openproxy_types::TargetFormat>()
                {
                    map.insert(m_id, fmt);
                }
            }
            map
        } else {
            std::collections::HashMap::new()
        };

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
        let target_format = sync_routing_overrides
            .get(d.model_id.as_str())
            .copied()
            .unwrap_or(d.target_format);

        let changed = stmt
            .execute(params![
                provider.as_str(),
                d.model_id.as_str(),
                d.display_name,
                target_format.as_str(),
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
