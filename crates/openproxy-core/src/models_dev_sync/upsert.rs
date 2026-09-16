//! Ingestion and database upsert for models.dev API payloads.

use super::provider_map::resolve_provider_target_ids;
use crate::error::{CoreError, Result};
use rusqlite::Connection;
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Deserialize)]
struct ModelsDevModel {
    id: String,
    tool_call: Option<bool>,
    reasoning: Option<bool>,
    structured_output: Option<bool>,
    limit: Option<ModelsDevLimit>,
    cost: Option<ModelsDevCost>,
    modalities: Option<ModelsDevModalities>,
    family: Option<String>,
    status: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ModelsDevLimit {
    context: Option<i64>,
    output: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct ModelsDevCost {
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct ModelsDevModalities {
    input: Option<Vec<String>>,
    output: Option<Vec<String>>,
}

fn upsert_single_model(
    stmt: &mut rusqlite::Statement,
    model_val: &serde_json::Value,
    all_ids: &[&str],
) -> Result<usize> {
    let model: ModelsDevModel = match serde::Deserialize::deserialize(model_val) {
        Ok(m) => m,
        Err(_) => return Ok(0),
    };

    let context = model.limit.as_ref().and_then(|l| l.context);
    let max_output = model.limit.as_ref().and_then(|l| l.output);
    let input_price = model.cost.as_ref().and_then(|c| c.input);
    let output_price = model.cost.as_ref().and_then(|c| c.output);
    let cached_price = model.cost.as_ref().and_then(|c| c.cache_read);

    let mod_in = model
        .modalities
        .as_ref()
        .and_then(|m| m.input.as_ref())
        .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "{}".to_string()));
    let mod_out = model
        .modalities
        .as_ref()
        .and_then(|m| m.output.as_ref())
        .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "{}".to_string()));
    let is_vision = model
        .modalities
        .as_ref()
        .and_then(|m| m.input.as_ref())
        .map(|inputs| inputs.iter().any(|s| s == "image"));

    let normalized = crate::model_normalize::normalize_model_id(&model.id);
    let mut count = 0;

    for our_id in all_ids {
        stmt.execute(rusqlite::params![
            our_id,
            &model.id,
            context,
            max_output,
            input_price,
            output_price,
            cached_price,
            model.tool_call.map(i64::from),
            model.reasoning.map(i64::from),
            is_vision.map(i64::from),
            model.structured_output.map(i64::from),
            mod_in,
            mod_out,
            model.family.as_deref(),
            model.status.as_deref(),
            &normalized,
        ])
        .map_err(openproxy_db::error::map_db_error)?;
        count += 1;
    }

    Ok(count)
}

fn upsert_provider_models(
    stmt: &mut rusqlite::Statement,
    ext_id: &str,
    provider_val: &serde_json::Value,
) -> Result<usize> {
    let Some(models_obj) = provider_val.get("models").and_then(|v| v.as_object()) else {
        return Ok(0);
    };

    let all_ids = resolve_provider_target_ids(ext_id);
    let mut count = 0;

    for model_val in models_obj.values() {
        count += upsert_single_model(stmt, model_val, &all_ids)?;
    }

    Ok(count)
}

fn prepare_upsert_capabilities_stmt(conn: &Connection) -> Result<rusqlite::Statement<'_>> {
    conn.prepare(
        "INSERT INTO model_capabilities_sync \
         (provider_id, model_id, context_length, max_output_tokens, \
          pricing_input_per_1m, pricing_output_per_1m, pricing_cached_per_1m, \
          tool_call, reasoning, vision, structured_output, \
          modalities_input, modalities_output, family, status, \
          model_id_normalized) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16) \
         ON CONFLICT(provider_id, model_id) DO UPDATE SET \
          context_length       = coalesce(excluded.context_length,       model_capabilities_sync.context_length),\
          max_output_tokens    = coalesce(excluded.max_output_tokens,    model_capabilities_sync.max_output_tokens),\
          pricing_input_per_1m = coalesce(excluded.pricing_input_per_1m, model_capabilities_sync.pricing_input_per_1m),\
          pricing_output_per_1m= coalesce(excluded.pricing_output_per_1m,model_capabilities_sync.pricing_output_per_1m),\
          pricing_cached_per_1m= coalesce(excluded.pricing_cached_per_1m,model_capabilities_sync.pricing_cached_per_1m),\
          tool_call     = coalesce(excluded.tool_call,     model_capabilities_sync.tool_call),\
          reasoning     = coalesce(excluded.reasoning,     model_capabilities_sync.reasoning),\
          vision        = coalesce(excluded.vision,        model_capabilities_sync.vision),\
          structured_output = coalesce(excluded.structured_output, model_capabilities_sync.structured_output),\
          modalities_input  = coalesce(excluded.modalities_input,  model_capabilities_sync.modalities_input),\
          modalities_output = coalesce(excluded.modalities_output, model_capabilities_sync.modalities_output),\
          family        = coalesce(excluded.family,        model_capabilities_sync.family),\
          status        = coalesce(excluded.status,        model_capabilities_sync.status),\
          model_id_normalized = coalesce(excluded.model_id_normalized, model_capabilities_sync.model_id_normalized),\
          fetched_at    = strftime('%Y-%m-%dT%H:%M:%SZ','now')"
    ).map_err(openproxy_db::error::map_db_error)
}

fn commit_or_rollback(conn: &Connection, is_in_tx: bool, result: Result<usize>) -> Result<usize> {
    match result {
        Ok(t) => {
            if !is_in_tx {
                conn.execute("COMMIT", ())
                    .map_err(openproxy_db::error::map_db_error)?;
            }
            Ok(t)
        }
        Err(e) => {
            if !is_in_tx {
                let _ = conn.execute("ROLLBACK", ());
            }
            Err(e)
        }
    }
}

/// Fetch models.dev data, map providers, upsert into DB.
/// The caller must supply the already-fetched API response bytes so
/// that `&Connection` is not held across async boundaries.
pub fn upsert_models_dev(body: &[u8], conn: &Connection) -> Result<usize> {
    let root: HashMap<String, serde_json::Value> = serde_json::from_slice(body)
        .map_err(|e| CoreError::Parse(format!("models.dev parse: {e}")))?;

    let is_in_tx = !conn.is_autocommit();
    if !is_in_tx {
        conn.execute("BEGIN", ())
            .map_err(openproxy_db::error::map_db_error)?;
    }

    let result = (|| -> Result<usize> {
        let mut total = 0usize;
        let mut stmt = prepare_upsert_capabilities_stmt(conn)?;
        for (ext_id, provider_val) in &root {
            total += upsert_provider_models(&mut stmt, ext_id, provider_val)?;
        }
        Ok(total)
    })();

    commit_or_rollback(conn, is_in_tx, result)
}
