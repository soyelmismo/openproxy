use crate::error::Result;
use crate::ids::ProviderId;
use crate::models::{DiscoveredModel, UpsertResult};
use openproxy_db::models::upsert::{
    DISCOVERED_MODEL_UPSERT_SQL, prune_obsolete_models, reconnect_inserted_combo_targets,
};
use rusqlite::{Connection, params};
use std::time::Duration;

pub struct SyncDiff<'a> {
    pub discovered_set: std::collections::HashSet<&'a str>,
    pub new_models: Vec<&'a DiscoveredModel>,
    // owned here so `deleted_models` needs no clone
    pub existing_rows: Vec<(String, i64, Option<String>)>,
}

impl SyncDiff<'_> {
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
            .map_err(openproxy_db::error::map_db_error)?;
        let rows = stmt
            .query_map([provider.as_str()], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })
            .map_err(openproxy_db::error::map_db_error)?;
        rows.map(|r| r.map_err(openproxy_db::error::map_db_error))
            .collect::<Result<Vec<_>>>()?
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

pub type SyncNotificationItem = (i64, &'static str, serde_json::Value);
pub type SyncTransactionResult = (UpsertResult, Vec<SyncNotificationItem>);

pub fn execute_sync_transaction(
    conn: &Connection,
    provider: &ProviderId,
    discovered: &[DiscoveredModel],
    diff: &SyncDiff,
    ttl: Duration,
) -> Result<SyncTransactionResult> {
    openproxy_db::error::with_busy_retry("execute_sync_transaction", || {
        let mut total = 0usize;
        let mut new_model_ids: Vec<crate::ids::ModelId> = Vec::new();
        let ttl_secs = ttl.as_secs() as i64;
        let mut inserted_model_ids: Vec<&str> = Vec::new();

        let tx = conn
            .unchecked_transaction()
            .map_err(openproxy_db::error::map_db_error)?;

        {
            let new_models_set: std::collections::HashSet<&str> = diff
                .new_models
                .iter()
                .map(|n| n.model_id.as_str())
                .collect();

            let mut stmt = tx
                .prepare(DISCOVERED_MODEL_UPSERT_SQL)
                .map_err(openproxy_db::error::map_db_error)?;

            for d in discovered {
                let model_id_str = d.model_id.as_str();
                let caps = d.capabilities.clone().unwrap_or_else(|| {
                    openproxy_types::capabilities::infer_capabilities(model_id_str)
                });
                let caps_json = caps.to_json();
                let input_mods_json = d
                    .input_modalities
                    .as_ref()
                    .and_then(|v| serde_json::to_string(v).ok())
                    .or_else(|| {
                        Some(openproxy_types::capabilities::infer_input_modalities_json(
                            model_id_str,
                        ))
                    });
                let output_mods_json = d
                    .output_modalities
                    .as_ref()
                    .and_then(|v| serde_json::to_string(v).ok())
                    .or_else(|| {
                        Some(openproxy_types::capabilities::infer_output_modalities_json(
                            model_id_str,
                        ))
                    });
                let inferred_type = d.model_type.clone().unwrap_or_else(|| {
                    openproxy_types::capabilities::infer_model_type(model_id_str).to_string()
                });
                let inferred_family = d
                    .family
                    .clone()
                    .or_else(|| openproxy_types::capabilities::infer_family(model_id_str));

                let is_new = new_models_set.contains(model_id_str);
                if is_new {
                    new_model_ids.push(d.model_id.clone());
                    inserted_model_ids.push(model_id_str);
                }

                let normalized = crate::model_normalize::normalize_model_id(model_id_str);

                let changed = stmt
                    .execute(params![
                        provider.as_str(),        // 1. provider_id
                        model_id_str,             // 2. model_id
                        d.display_name,           // 3. display_name
                        d.target_format.as_str(), // 4. target_format
                        ttl_secs,                 // 5. (used in the datetime '+? seconds' expr)
                        d.context_length,         // 6. context_length
                        d.max_output_tokens,      // 7. max_output_tokens
                        input_mods_json,          // 8. input_modalities_json
                        output_mods_json,         // 9. output_modalities_json
                        inferred_type,            // 10. model_type
                        inferred_family,          // 11. family
                        caps_json,                // 12. capabilities_json
                        &normalized,              // 13. model_id_normalized
                    ])
                    .map_err(openproxy_db::error::map_db_error)?;
                total += changed;
            }
        }

        prune_obsolete_models(&tx, provider, discovered)?;

        let events = generate_events(&tx, provider, diff)?;

        let updated = reconnect_inserted_combo_targets(
            &tx,
            provider,
            &inserted_model_ids,
            openproxy_db::error::map_db_error,
        )?;
        if updated > 0 {
            tracing::info!(
                target: "openproxy.core.models",
                provider = %provider,
                reconnected_targets = updated,
                "gate F1: reconnected orphan combo_targets to re-inserted model",
            );
        }

        tx.commit().map_err(openproxy_db::error::map_db_error)?;

        Ok((
            UpsertResult {
                touched: total,
                new_model_ids: new_model_ids.into(),
            },
            events,
        ))
    })
}

pub fn generate_events(
    tx: &rusqlite::Transaction,
    provider: &ProviderId,
    diff: &SyncDiff,
) -> Result<Vec<(i64, &'static str, serde_json::Value)>> {
    let mut events = Vec::new();

    let notifications_present: bool = tx
        .query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = 'notifications'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .is_ok_and(|n| n != 0);

    if !notifications_present {
        return Ok(events);
    }

    // gate off: write no notification rows inside this transaction and return no
    // events, so the caller broadcasts nothing
    if !crate::notifications::insert_many_enabled() {
        return Ok(events);
    }

    let already_notified: std::collections::HashSet<String> = {
        let mut stmt = tx
            .prepare(
                "SELECT dedup_key FROM notifications \
                 WHERE kind = ?1 AND provider_id = ?2 AND dedup_key IS NOT NULL",
            )
            .map_err(openproxy_db::error::map_db_error)?;
        let rows = stmt
            .query_map(
                rusqlite::params![crate::notifications::KIND_MODEL_NEW, provider.as_str()],
                |r| r.get::<_, String>(0),
            )
            .map_err(openproxy_db::error::map_db_error)?;
        rows.filter_map(std::result::Result::ok).collect()
    };

    let new_models_rows: Vec<_> = diff
        .new_models
        .iter()
        .filter_map(|d| {
            let dedup = format!("{}:{}", provider.as_str(), d.model_id.as_str());
            if already_notified.contains(&dedup) {
                return None;
            }
            let payload = serde_json::json!({
                "provider_id": provider.as_str(),
                "model_id": d.model_id.as_str(),
                "display_name": d.display_name,
                "target_format": d.target_format.as_str(),
                "context_length": d.context_length,
            });
            Some((payload, Some(dedup), Some(provider.as_str().to_string())))
        })
        .collect();

    if !new_models_rows.is_empty()
        && let Ok(results) = crate::notifications::insert_many_gated(
            tx,
            crate::notifications::KIND_MODEL_NEW,
            &new_models_rows,
        )
    {
        for (id, payload) in results {
            events.push((id, crate::notifications::KIND_MODEL_NEW, payload));
        }
    }

    let deleted_models_rows: Vec<_> = diff
        .deleted_models()
        .map(|(model_id, display_name)| {
            let payload = serde_json::json!({
                "provider_id": provider.as_str(),
                "model_id": model_id,
                "display_name": display_name,
            });
            let dedup = format!("{}:{}", provider.as_str(), model_id);
            (payload, Some(dedup), Some(provider.as_str().to_string()))
        })
        .collect();

    if let Ok(results) = crate::notifications::insert_many_gated(
        tx,
        crate::notifications::KIND_MODEL_GONE,
        &deleted_models_rows,
    ) {
        for (id, payload) in results {
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
