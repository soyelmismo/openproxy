//! models.dev sync — fetch model pricing, context length, and
//! capabilities from https://models.dev/api.json and store them in
//! the `model_capabilities_sync` table. Also enriches `models` rows
//! with missing `context_length` and auto-creates combos for models
//! that are active in multiple providers.
//!
//! ## Provider mapping
//!
//! models.dev uses canonical provider IDs (e.g. `openai`, `anthropic`,
//! `google`). OpenProxy uses different IDs (e.g. `openrouter` for
//! OpenRouter-hosted OpenAI models). The `PROVIDER_MAP` below handles
//! this mapping: one models.dev provider → many OpenProxy provider IDs.
//!
//! ## Opt-in
//!
//! Set `MODELS_DEV_SYNC_ENABLED=true` in the environment to enable
//! the periodic background sync (default: off). The sync can also be
//! triggered manually via `POST /admin/models/sync-models-dev`.

use crate::error::{CoreError, Result};
use openproxy_adapters::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest,
};
use rusqlite::Connection;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

/// models.dev API source URL.
const MODELS_DEV_URL: &str = "https://models.dev/api.json";

/// Provider mapping: models.dev provider id → our internal IDs.
///
/// Static fallback/supplemental table for mapping models.dev provider IDs to
/// internal OpenProxy provider IDs when adapters are not directly registered
/// or for aliases (e.g. `minimax-cn`).
pub const PROVIDER_MAP: &[(&str, &[&str])] = &[
    ("openai", &["openrouter"]),
    ("anthropic", &["openrouter"]),
    ("google", &["gemini"]),
    ("meta", &["openrouter"]),
    ("mistral", &["openrouter"]),
    ("deepseek", &["openrouter"]),
    ("qwen", &["openrouter"]),
    ("nvidia", &["nvidia-nim"]),
    ("minimax", &["minimax", "minimax-cn"]),
    ("amazon", &["openrouter"]),
    ("cohere", &["openrouter"]),
    ("opencode", &["opencode-zen"]),
    ("opencode-go", &["opencode-go"]),
    ("perplexity", &["openrouter"]),
    ("groq", &["openrouter"]),
    ("together", &["openrouter"]),
    ("fireworks", &["openrouter"]),
    ("deepinfra", &["openrouter"]),
    ("xai", &["openrouter"]),
];

fn ingest_adapter_mappings(map: &mut HashMap<String, Vec<String>>) {
    for adapter in openproxy_adapters::adapters::builtin_adapters() {
        let internal_id = adapter.id().as_str().to_string();
        for &canon_id in adapter.models_dev_canonical_ids() {
            let entry = map.entry(canon_id.to_string()).or_default();
            if !entry.contains(&internal_id) {
                entry.push(internal_id.clone());
            }
        }
    }
}

fn ingest_static_provider_map(map: &mut HashMap<String, Vec<String>>) {
    for &(canon_id, internal_ids) in PROVIDER_MAP {
        let entry = map.entry(canon_id.to_string()).or_default();
        for &id in internal_ids {
            let id_str = id.to_string();
            if !entry.contains(&id_str) {
                entry.push(id_str);
            }
        }
    }
}

pub type ProviderTargetMap = HashMap<Box<str>, Box<[Box<str>]>>;

/// Builds the mapping of canonical models.dev provider ID -> OpenProxy internal provider IDs.
/// Leverages `models_dev_canonical_ids` metadata from registered built-in adapters,
/// with static `PROVIDER_MAP` fallback and alias augmentation.
pub fn build_provider_mapping() -> ProviderTargetMap {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    ingest_adapter_mappings(&mut map);
    ingest_static_provider_map(&mut map);
    map.into_iter()
        .map(|(k, v)| {
            let boxed_v: Box<[Box<str>]> = v.into_iter().map(String::into_boxed_str).collect();
            (k.into_boxed_str(), boxed_v)
        })
        .collect()
}

/// Pre-indexed provider map built from adapter metadata + static fallback table.
pub static RESOLVED_PROVIDER_MAP: LazyLock<ProviderTargetMap> =
    LazyLock::new(build_provider_mapping);

// ── API Response shapes ─────────────────────────────────────────────
//
// Root is a flat dict keyed by provider id:
//   { "openai": { "id": "openai", "name": "...", "models": { "gpt-4o": { ... } } } }
//
// Each provider's `models` is a dict keyed by model id, with prices
// nested under `cost`, context under `limit`, and capabilities at top level.

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

// ── Core sync function ──────────────────────────────────────────────

fn resolve_provider_target_ids(ext_id: &str) -> Vec<&str> {
    let mapped_ids = RESOLVED_PROVIDER_MAP
        .get(ext_id)
        .map_or(&[][..], |v| v.as_ref());

    let mut all_ids: Vec<&str> = Vec::with_capacity(1 + mapped_ids.len());
    all_ids.push(ext_id);
    for id in mapped_ids {
        let id_str: &str = id.as_ref();
        if !all_ids.contains(&id_str) {
            all_ids.push(id_str);
        }
    }
    all_ids
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

/// Fetch models.dev data, map providers, upsert into DB.
/// The caller must supply the already-fetched API response bytes so
/// that `&Connection` is not held across async boundaries.
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

async fn handle_fetch_attempt_error(
    attempt: u32,
    max_retries: u32,
    backoff: &mut std::time::Duration,
    e: CoreError,
) -> Result<()> {
    if attempt == max_retries {
        tracing::warn!(attempt, error = %e, "models.dev fetch failed after all retries");
        Err(e)
    } else {
        tracing::warn!(
            attempt,
            next_backoff_ms = backoff.as_millis() as u64,
            error = %e,
            "models.dev fetch failed; retrying"
        );
        tokio::time::sleep(*backoff).await;
        *backoff *= 2;
        Ok(())
    }
}

/// Fetch raw JSON bytes from models.dev.
async fn fetch_models_dev(upstream: &Arc<UpstreamClient>) -> Result<bytes::Bytes> {
    const MAX_RETRIES: u32 = 3;
    let mut backoff = std::time::Duration::from_secs(2);
    for attempt in 1..=MAX_RETRIES {
        match fetch_models_dev_once(upstream).await {
            Ok(bytes) => {
                if attempt > 1 {
                    tracing::info!(attempt, "models.dev fetch succeeded after retry");
                }
                return Ok(bytes);
            }
            Err(e) => {
                handle_fetch_attempt_error(attempt, MAX_RETRIES, &mut backoff, e).await?;
            }
        }
    }
    Err(CoreError::UpstreamConnection(
        "models.dev fetch: retry loop exhausted".into(),
    ))
}

fn map_fetch_upstream_error(
    e: openproxy_adapters::upstream::UpstreamError,
    ctx: &str,
) -> CoreError {
    if matches!(e, openproxy_adapters::upstream::UpstreamError::Cancel) {
        CoreError::Cancelled(openproxy_types::CancelReason::ClientDisconnected)
    } else {
        CoreError::UpstreamConnection(format!("{ctx}: {e}"))
    }
}

/// Single attempt to fetch raw JSON bytes from models.dev.
async fn fetch_models_dev_once(upstream: &Arc<UpstreamClient>) -> Result<bytes::Bytes> {
    let req = UpstreamRequest::get(MODELS_DEV_URL);
    let cancel = CancellationToken::new();
    let response = upstream
        .call(req, TimeoutProfile::ModelDiscovery, cancel)
        .await
        .map_err(|e| map_fetch_upstream_error(e, "models.dev fetch"))?;

    let status = response.status;
    let body = response
        .collect()
        .await
        .map_err(|e| map_fetch_upstream_error(e, "models.dev body read"))?;

    if !status.is_success() {
        let text = String::from_utf8_lossy(&body);
        return Err(CoreError::UpstreamError {
            status: status.as_u16(),
            provider: "models.dev".into(),
            model: "<sync>".into(),
            body: text.to_string(),
            is_proxy_rotated: false,
            class: openproxy_types::UpstreamErrorClass::Generic,
            is_hard_skip: false,
        });
    }

    Ok(body)
}

// ── Enrichment helpers ──────────────────────────────────────────────

/// Backfill `model_id_normalized` for existing rows in both `models` and
/// `model_capabilities_sync` that have NULL.
///
/// Migration 000033 added the `model_id_normalized` column to both tables
/// but left it NULL for all pre-existing rows. The enrichment queries in
/// [`enrich_models_from_sync`] match on `models.model_id_normalized` and
/// gate on `WHERE ... IS NOT NULL`, so without this backfill, existing
/// models (everything discovered before the migration deployed) would
/// never get context windows or pricing from models.dev.
///
/// This function loads every row where `model_id_normalized IS NULL`,
/// computes the normalized id in Rust via [`normalize_model_id`], and
/// UPDATEs the row. It is idempotent: rows that already have a non-NULL
/// value are skipped.
///
/// Returns the total number of rows backfilled across both tables.
fn fetch_unnormalized_rows(conn: &Connection, table: &str) -> Result<Vec<(String, String)>> {
    let sql = match table {
        "models" => "SELECT provider_id, model_id FROM models WHERE model_id_normalized IS NULL",
        "model_capabilities_sync" => {
            "SELECT provider_id, model_id FROM model_capabilities_sync WHERE model_id_normalized IS NULL"
        }
        _ => unreachable!("invalid table"),
    };
    let mut stmt = conn
        .prepare(sql)
        .map_err(openproxy_db::error::map_db_error)?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(openproxy_db::error::map_db_error)?;
    Ok(rows.filter_map(std::result::Result::ok).collect())
}

fn backfill_table_normalized(
    conn: &Connection,
    table: &str,
    rows: &[(String, String)],
) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    let mut total = 0;
    for chunk in rows.chunks(900 / 3) {
        let vals = openproxy_db::batch::values_placeholders(chunk.len(), 3);
        let mut sql = String::with_capacity(160 + vals.len() + table.len() * 3);
        sql.push_str("WITH updates(provider_id, model_id, normalized) AS (VALUES ");
        sql.push_str(&vals);
        sql.push_str(") UPDATE ");
        sql.push_str(table);
        sql.push_str(" SET model_id_normalized = updates.normalized FROM updates WHERE ");
        sql.push_str(table);
        sql.push_str(".provider_id = updates.provider_id AND ");
        sql.push_str(table);
        sql.push_str(".model_id = updates.model_id");

        let mut norm_strings = Vec::with_capacity(chunk.len());
        for (_, model_id) in chunk {
            norm_strings.push(crate::model_normalize::normalize_model_id(model_id));
        }
        let mut params: Vec<&str> = Vec::with_capacity(chunk.len() * 3);
        for ((provider_id, model_id), normalized) in chunk.iter().zip(&norm_strings) {
            params.push(provider_id.as_str());
            params.push(model_id.as_str());
            params.push(normalized.as_str());
        }
        let count = conn
            .execute(&sql, rusqlite::params_from_iter(params))
            .map_err(openproxy_db::error::map_db_error)?;
        total += count;
    }
    Ok(total)
}

/// Backfill `model_id_normalized` for existing rows in both `models` and
/// `model_capabilities_sync` that have NULL.
pub fn backfill_model_id_normalized(conn: &Connection) -> Result<usize> {
    let model_rows = fetch_unnormalized_rows(conn, "models")?;
    let sync_rows = fetch_unnormalized_rows(conn, "model_capabilities_sync")?;

    let total = backfill_table_normalized(conn, "models", &model_rows)?
        + backfill_table_normalized(conn, "model_capabilities_sync", &sync_rows)?;

    if total > 0 {
        tracing::info!(
            total,
            models_backfilled = model_rows.len(),
            sync_backfilled = sync_rows.len(),
            "backfilled model_id_normalized for existing rows"
        );
    }

    Ok(total)
}

/// Recompute `cost_usd` for usage rows that have `cost_usd = 0` AND
/// `prompt_tokens > 0` (i.e. they consumed tokens but had no pricing
/// at record time).
type UsageRow = (i64, String, String, Option<u32>, Option<u32>);

fn resolve_recompute_price(
    conn: &Connection,
    provider_id: &str,
    model_id: &str,
) -> Option<crate::pricing::Price> {
    let price = crate::pricing::lookup_with_db(conn, provider_id, model_id);
    match price {
        Some(p) if p.input_per_1m == 0.0 && p.output_per_1m == 0.0 => {
            let base_model = crate::model_normalize::normalize_model_id(model_id);
            let paid = crate::pricing::lookup_by_normalized(conn, &base_model)
                .filter(|p| p.input_per_1m > 0.0 || p.output_per_1m > 0.0);
            paid.or(Some(p))
        }
        other => other,
    }
}

fn compute_usage_row_cost(
    price: Option<crate::pricing::Price>,
    prompt_tokens: Option<u32>,
    completion_tokens: Option<u32>,
) -> Option<f64> {
    let p = price?;
    let prompt = f64::from(prompt_tokens.unwrap_or(0));
    let completion = f64::from(completion_tokens.unwrap_or(0));
    let cost = p.input_per_1m * prompt / 1_000_000.0 + p.output_per_1m * completion / 1_000_000.0;
    if cost > 0.0 { Some(cost) } else { None }
}

pub fn recompute_costs(conn: &Connection) -> Result<usize> {
    let rows: Vec<UsageRow> = {
        let mut stmt = conn
            .prepare(
                "SELECT id, provider_id, upstream_model_id, prompt_tokens, completion_tokens \
                 FROM usage \
                 WHERE cost_usd = 0.0 \
                   AND (prompt_tokens > 0 OR completion_tokens > 0)",
            )
            .map_err(openproxy_db::error::map_db_error)?;
        let result = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<u32>>(3)?,
                    row.get::<_, Option<u32>>(4)?,
                ))
            })
            .map_err(openproxy_db::error::map_db_error)?;
        result.filter_map(std::result::Result::ok).collect()
    };

    let mut updated = 0usize;
    {
        let mut stmt = conn
            .prepare("UPDATE usage SET cost_usd = ?1 WHERE id = ?2")
            .map_err(openproxy_db::error::map_db_error)?;
        for (id, provider_id, model_id, prompt_tokens, completion_tokens) in &rows {
            let price = resolve_recompute_price(conn, provider_id, model_id);
            if let Some(cost) = compute_usage_row_cost(price, *prompt_tokens, *completion_tokens) {
                stmt.execute(rusqlite::params![cost, id])
                    .map_err(openproxy_db::error::map_db_error)?;
                updated += 1;
            }
        }
    }

    if updated > 0 {
        tracing::info!(
            updated,
            total_candidates = rows.len(),
            "recomputed cost_usd for previously-unpriced usage rows"
        );
    }

    Ok(updated)
}

fn enrich_context_length(conn: &Connection) -> Result<usize> {
    conn.execute(
        "UPDATE models SET context_length = COALESCE(
        (SELECT s.context_length FROM model_capabilities_sync s
         WHERE s.model_id_normalized = models.model_id_normalized
           AND s.context_length IS NOT NULL
         LIMIT 1),
        models.context_length
     )
     WHERE models.custom = 0
       AND models.model_id_normalized IS NOT NULL
       AND EXISTS (
         SELECT 1 FROM model_capabilities_sync s
         WHERE s.model_id_normalized = models.model_id_normalized
           AND s.context_length IS NOT NULL
       )",
        [],
    )
    .map_err(openproxy_db::error::map_db_error)
}

fn enrich_max_output_tokens(conn: &Connection) -> Result<usize> {
    conn.execute(
        "UPDATE models SET max_output_tokens = COALESCE(
        (SELECT s.max_output_tokens FROM model_capabilities_sync s
         WHERE s.model_id_normalized = models.model_id_normalized
           AND s.max_output_tokens IS NOT NULL
         LIMIT 1),
        models.max_output_tokens
     )
     WHERE models.custom = 0
       AND models.model_id_normalized IS NOT NULL
       AND EXISTS (
         SELECT 1 FROM model_capabilities_sync s
         WHERE s.model_id_normalized = models.model_id_normalized
           AND s.max_output_tokens IS NOT NULL
       )",
        [],
    )
    .map_err(openproxy_db::error::map_db_error)
}

fn enrich_capabilities(conn: &Connection) -> Result<usize> {
    conn.execute(
        "UPDATE models SET capabilities_json = (
        SELECT json_patch(
            coalesce(models.capabilities_json, '{}'),
            json_object(
                'vision',            s.vision,
                'tool_calling',      s.tool_call,
                'reasoning',         s.reasoning,
                'structured_output', s.structured_output
            )
        )
        FROM model_capabilities_sync s
        WHERE s.model_id_normalized = models.model_id_normalized
          AND (s.vision IS NOT NULL OR s.tool_call IS NOT NULL
               OR s.reasoning IS NOT NULL OR s.structured_output IS NOT NULL)
        LIMIT 1
     )
     WHERE models.custom = 0
       AND models.model_id_normalized IS NOT NULL
       AND EXISTS (
         SELECT 1 FROM model_capabilities_sync s
         WHERE s.model_id_normalized = models.model_id_normalized
           AND (s.vision IS NOT NULL OR s.tool_call IS NOT NULL
                OR s.reasoning IS NOT NULL OR s.structured_output IS NOT NULL)
       )",
        [],
    )
    .map_err(openproxy_db::error::map_db_error)
}

fn enrich_metadata(conn: &Connection) -> Result<usize> {
    conn.execute(
        "UPDATE models SET
            family = COALESCE(
                (SELECT s.family FROM model_capabilities_sync s
                 WHERE s.model_id_normalized = models.model_id_normalized
                   AND s.family IS NOT NULL LIMIT 1),
                models.family
            ),
            input_modalities_json = COALESCE(
                (SELECT s.modalities_input FROM model_capabilities_sync s
                 WHERE s.model_id_normalized = models.model_id_normalized
                   AND s.modalities_input IS NOT NULL LIMIT 1),
                models.input_modalities_json
            ),
            output_modalities_json = CASE
                WHEN models.model_id_normalized LIKE '%embed%' OR models.model_id_normalized LIKE '%bge-%'
                    THEN '[\"embedding\"]'
                ELSE COALESCE(
                    (SELECT s.modalities_output FROM model_capabilities_sync s
                     WHERE s.model_id_normalized = models.model_id_normalized
                       AND s.modalities_output IS NOT NULL LIMIT 1),
                    models.output_modalities_json
                )
            END,
            model_type = CASE
                WHEN models.model_id_normalized LIKE '%embed%'
                  OR models.model_id_normalized LIKE '%bge-%'
                  OR EXISTS (
                    SELECT 1 FROM model_capabilities_sync s
                    WHERE s.model_id_normalized = models.model_id_normalized
                      AND s.modalities_output = '[\"embedding\"]'
                  ) THEN 'embedding'
                WHEN (models.model_id_normalized LIKE '%dall-e%'
                  OR models.model_id_normalized LIKE '%sdxl%'
                  OR models.model_id_normalized LIKE '%stable-diffusion%'
                  OR models.model_id_normalized LIKE '%midjourney%'
                  OR EXISTS (
                    SELECT 1 FROM model_capabilities_sync s
                    WHERE s.model_id_normalized = models.model_id_normalized
                      AND s.modalities_output = '[\"image\"]'
                  ))
                  AND NOT (models.model_id_normalized LIKE '%gemini%'
                           OR models.model_id_normalized LIKE '%gpt-%'
                           OR models.model_id_normalized LIKE '%claude%'
                           OR models.model_id_normalized LIKE '%diffusiongemma%')
                  THEN 'image'
                WHEN (models.model_id_normalized LIKE '%whisper%'
                  OR models.model_id_normalized LIKE '%tts%'
                  OR models.model_id_normalized LIKE '%elevenlabs%'
                  OR models.model_id_normalized LIKE '%melotts%'
                  OR EXISTS (
                    SELECT 1 FROM model_capabilities_sync s
                    WHERE s.model_id_normalized = models.model_id_normalized
                      AND s.modalities_output = '[\"audio\"]'
                      AND s.modalities_input NOT LIKE '%\"text\"%'
                  ))
                  AND NOT (models.model_id_normalized LIKE '%gemini%'
                           OR models.model_id_normalized LIKE '%gpt-%'
                           OR models.model_id_normalized LIKE '%claude%'
                           OR models.model_id_normalized LIKE '%qwen%')
                  THEN 'audio'
                WHEN (models.model_id_normalized LIKE '%gemini%'
                  OR models.model_id_normalized LIKE '%gpt-%'
                  OR models.model_id_normalized LIKE '%claude%'
                  OR models.model_id_normalized LIKE '%qwen%'
                  OR models.model_id_normalized LIKE '%llama%'
                  OR models.model_id_normalized LIKE '%mistral%'
                  OR models.model_id_normalized LIKE '%deepseek%'
                  OR models.model_id_normalized LIKE '%gemma%')
                  AND NOT (models.model_id_normalized LIKE '%whisper%'
                           OR models.model_id_normalized LIKE '%tts%'
                           OR models.model_id_normalized LIKE '%dall-e%'
                           OR models.model_id_normalized LIKE '%imagen%')
                  THEN 'chat'
                ELSE models.model_type
            END
         WHERE models.custom = 0
           AND models.model_id_normalized IS NOT NULL
           AND EXISTS (
             SELECT 1 FROM model_capabilities_sync s
             WHERE s.model_id_normalized = models.model_id_normalized
           )",
        [],
    )
    .map_err(openproxy_db::error::map_db_error)
}

/// After a sync, refresh `models.context_length`, `max_output_tokens`,
/// and `capabilities_json` from the `model_capabilities_sync` table.
pub fn enrich_models_from_sync(conn: &Connection) -> Result<usize> {
    backfill_model_id_normalized(conn)?;
    let ctx = enrich_context_length(conn)?;
    let tok = enrich_max_output_tokens(conn)?;
    let cap = enrich_capabilities(conn)?;
    let meta = enrich_metadata(conn)?;
    Ok(ctx + tok + cap + meta)
}

fn fetch_candidate_normalized_ids(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare(
            "SELECT m.model_id_normalized
             FROM models m
             JOIN accounts a ON a.provider_id = m.provider_id
             WHERE m.active = 1
               AND a.health_status = 'healthy'
               AND m.model_id_normalized IS NOT NULL
               AND m.model_id_normalized != ''
             GROUP BY m.model_id_normalized
             HAVING COUNT(DISTINCT m.provider_id) >= 2
             ORDER BY m.model_id_normalized",
        )
        .map_err(openproxy_db::error::map_db_error)?;

    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(openproxy_db::error::map_db_error)?;
    rows.map(|row| row.map_err(openproxy_db::error::map_db_error))
        .collect::<std::result::Result<Vec<_>, crate::error::CoreError>>()
}

type TargetDescriptor = (i64, String, i64);
type TargetsByNormIdMap = HashMap<String, Vec<TargetDescriptor>>;
type ComboTargetKey = (i64, i64, i64);
type ExistingTargetsAndMaxOrders = (std::collections::HashSet<ComboTargetKey>, HashMap<i64, i32>);

fn fetch_targets_by_norm_id(
    conn: &Connection,
    normalized_ids: &[String],
) -> Result<TargetsByNormIdMap> {
    if normalized_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let mut stmt = conn
        .prepare(
            "SELECT m.model_id_normalized, m.rowid, m.provider_id, a.id
             FROM models m
             JOIN accounts a ON a.provider_id = m.provider_id AND a.health_status = 'healthy'
             WHERE m.active = 1
               AND m.model_id_normalized IS NOT NULL
               AND m.model_id_normalized != ''
             ORDER BY m.model_id_normalized, m.provider_id",
        )
        .map_err(openproxy_db::error::map_db_error)?;

    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<i64>>(3)?.unwrap_or(-1),
            ))
        })
        .map_err(openproxy_db::error::map_db_error)?;

    let mut map: TargetsByNormIdMap = HashMap::new();
    for row in rows {
        let (norm_id, row_id, provider_id, account_id) =
            row.map_err(openproxy_db::error::map_db_error)?;
        map.entry(norm_id)
            .or_default()
            .push((row_id, provider_id, account_id));
    }
    Ok(map)
}

fn fetch_existing_combos(
    conn: &Connection,
    combo_names: &std::collections::HashSet<String>,
) -> Result<HashMap<String, i64>> {
    if combo_names.is_empty() {
        return Ok(HashMap::new());
    }
    let mut stmt = conn
        .prepare("SELECT name, id FROM combos")
        .map_err(openproxy_db::error::map_db_error)?;
    let result = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(openproxy_db::error::map_db_error)?
        .filter_map(std::result::Result::ok)
        .filter(|(name, _)| combo_names.contains(name))
        .collect();
    Ok(result)
}

fn fetch_existing_targets_and_max_orders(
    conn: &Connection,
    combo_ids: &[i64],
) -> Result<ExistingTargetsAndMaxOrders> {
    if combo_ids.is_empty() {
        return Ok((std::collections::HashSet::new(), HashMap::new()));
    }
    let rows = openproxy_db::batch::query_in_chunks(
        conn,
        "SELECT combo_id, account_id, model_row_id FROM combo_targets WHERE combo_id IN ({})",
        combo_ids,
        openproxy_db::batch::DEFAULT_CHUNK_SIZE,
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<i64>>(1)?.unwrap_or(-1),
                row.get::<_, i64>(2)?,
            ))
        },
    )
    .map_err(openproxy_db::error::map_db_error)?;
    let existing_targets = rows.into_iter().collect();

    let order_rows = openproxy_db::batch::query_in_chunks(
        conn,
        "SELECT combo_id, MAX(priority_order) FROM combo_targets WHERE combo_id IN ({}) GROUP BY combo_id",
        combo_ids,
        openproxy_db::batch::DEFAULT_CHUNK_SIZE,
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i32>(1)?)),
    )
    .map_err(openproxy_db::error::map_db_error)?;
    let max_orders = order_rows.into_iter().collect();

    Ok((existing_targets, max_orders))
}

fn append_combo_targets(
    insert_target_stmt: &mut rusqlite::Statement,
    combo_id: i64,
    targets: &[(i64, String, i64)],
    existing_targets: &std::collections::HashSet<(i64, i64, i64)>,
    max_orders: &mut std::collections::HashMap<i64, i32>,
) -> Result<()> {
    for &(row_id, ref provider_id, account_id) in targets {
        if !existing_targets.contains(&(combo_id, account_id, row_id)) {
            let next_order = max_orders.get(&combo_id).copied().unwrap_or(-1) + 1;
            max_orders.insert(combo_id, next_order);
            insert_target_stmt
                .execute(rusqlite::params![
                    combo_id,
                    provider_id,
                    account_id,
                    row_id,
                    next_order
                ])
                .map_err(openproxy_db::error::map_db_error)?;
        }
    }
    Ok(())
}

fn ensure_combo_id(
    conn: &Connection,
    insert_combo_stmt: &mut rusqlite::Statement,
    combo_name: &str,
    target_count: usize,
    existing_id: Option<i64>,
    created_count: &mut usize,
) -> Result<i64> {
    if let Some(id) = existing_id {
        return Ok(id);
    }
    let race_size = (target_count as u8).min(3);
    insert_combo_stmt
        .execute(rusqlite::params![combo_name, race_size])
        .map_err(openproxy_db::error::map_db_error)?;
    *created_count += 1;
    Ok(conn.last_insert_rowid())
}

struct ComboSyncState<'a, 'stmt> {
    existing_combos: &'a std::collections::HashMap<String, i64>,
    existing_targets: &'a std::collections::HashSet<(i64, i64, i64)>,
    max_orders: &'a mut std::collections::HashMap<i64, i32>,
    insert_combo_stmt: &'a mut rusqlite::Statement<'stmt>,
    insert_target_stmt: &'a mut rusqlite::Statement<'stmt>,
    created: &'a mut usize,
}

fn sync_single_candidate_combo(
    conn: &Connection,
    norm_id: &str,
    targets: &[(i64, String, i64)],
    state: &mut ComboSyncState<'_, '_>,
) -> Result<()> {
    if targets.len() < 2 {
        return Ok(());
    }

    let combo_name = format!("auto:{norm_id}");
    let combo_id = ensure_combo_id(
        conn,
        state.insert_combo_stmt,
        &combo_name,
        targets.len(),
        state.existing_combos.get(&combo_name).copied(),
        state.created,
    )?;

    append_combo_targets(
        state.insert_target_stmt,
        combo_id,
        targets,
        state.existing_targets,
        state.max_orders,
    )
}

/// Auto-create combos for models that are active in ≥2 providers.
pub fn auto_create_combos(conn: &Connection) -> Result<usize> {
    let normalized_ids = fetch_candidate_normalized_ids(conn)?;
    let targets_by_norm_id = fetch_targets_by_norm_id(conn, &normalized_ids)?;

    let combo_names: std::collections::HashSet<String> = normalized_ids
        .iter()
        .map(|id| format!("auto:{id}"))
        .collect();

    let existing_combos = fetch_existing_combos(conn, &combo_names)?;
    let combo_ids: Vec<i64> = existing_combos.values().copied().collect();
    let (existing_targets, mut max_orders) =
        fetch_existing_targets_and_max_orders(conn, &combo_ids)?;

    let is_in_tx = !conn.is_autocommit();
    if !is_in_tx {
        conn.execute("BEGIN", ())
            .map_err(openproxy_db::error::map_db_error)?;
    }

    let mut created = 0usize;
    let mut insert_combo_stmt = conn
        .prepare("INSERT INTO combos (name, strategy, race_size) VALUES (?1, 'priority', ?2)")
        .map_err(openproxy_db::error::map_db_error)?;

    let mut insert_target_stmt = conn
        .prepare(
            "INSERT INTO combo_targets (combo_id, provider_id, account_id, model_row_id, priority_order) VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .map_err(openproxy_db::error::map_db_error)?;

    let mut state = ComboSyncState {
        existing_combos: &existing_combos,
        existing_targets: &existing_targets,
        max_orders: &mut max_orders,
        insert_combo_stmt: &mut insert_combo_stmt,
        insert_target_stmt: &mut insert_target_stmt,
        created: &mut created,
    };

    let empty_targets = Vec::new();
    for norm_id in &normalized_ids {
        let targets = targets_by_norm_id.get(norm_id).unwrap_or(&empty_targets);
        sync_single_candidate_combo(conn, norm_id, targets, &mut state)?;
    }

    if !is_in_tx {
        conn.execute("COMMIT", ())
            .map_err(openproxy_db::error::map_db_error)?;
    }

    Ok(created)
}

/// Background sync task: periodically fetch models.dev, enrich, and
/// auto-create combos. Runs at the configured interval.
///
/// Background sync task using a `ServiceContainer` for dependency injection.
pub async fn start_sync_scheduler_with_container(
    services: &crate::di::ServiceContainer,
    check_interval_secs: u64,
) -> Result<()> {
    let db_pool = services.db_pool()?;
    let upstream_client = services.upstream_client()?;
    start_sync_scheduler(db_pool, upstream_client, check_interval_secs).await;
    Ok(())
}

fn process_models_dev_sync_payload(db_pool: &openproxy_db::DbPool, body: &[u8]) -> usize {
    let count = {
        let conn = db_pool.writer();
        match upsert_models_dev(body, &conn) {
            Ok(n) => {
                tracing::info!("models.dev sync: {} rows upserted", n);
                n
            }
            Err(e) => {
                tracing::warn!(error = %e, "models.dev sync upsert failed");
                return 0;
            }
        }
    };

    if count > 0 {
        {
            let conn = db_pool.writer();
            match enrich_models_from_sync(&conn) {
                Ok(n) => tracing::info!("models.dev sync: enriched {} model rows", n),
                Err(e) => tracing::warn!(error = %e, "models.dev sync enrich failed"),
            }
        }
        {
            let conn = db_pool.writer();
            match auto_create_combos(&conn) {
                Ok(n) => {
                    if n > 0 {
                        tracing::info!("models.dev sync: created {} auto-combos", n);
                    }
                }
                Err(e) => tracing::warn!(error = %e, "models.dev sync auto-combo failed"),
            }
        }
    }
    count
}

async fn run_single_sync_iteration(
    db_pool: &Arc<openproxy_db::DbPool>,
    upstream_client: &Arc<UpstreamClient>,
) {
    tracing::info!("models.dev sync: starting");

    let body = match fetch_models_dev(upstream_client).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "models.dev sync fetch failed");
            return;
        }
    };

    let db_pool_clone = Arc::clone(db_pool);
    let count =
        tokio::task::spawn_blocking(move || process_models_dev_sync_payload(&db_pool_clone, &body))
            .await
            .unwrap_or(0);

    if count > 0 {
        tracing::info!("models.dev sync: complete");
    }
}

pub async fn start_sync_scheduler(
    db_pool: std::sync::Arc<openproxy_db::DbPool>,
    upstream_client: Arc<UpstreamClient>,
    check_interval_secs: u64,
) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(check_interval_secs));

    loop {
        tick.tick().await;
        run_single_sync_iteration(&db_pool, &upstream_client).await;
    }
}

// ── Manual trigger helper ───────────────────────────────────────────

/// One-shot sync + enrich + auto-combo, called from the admin handler.
pub async fn run_one_shot(
    db_pool: std::sync::Arc<openproxy_db::DbPool>,
    upstream_client: Arc<UpstreamClient>,
) -> Result<String> {
    let body = fetch_models_dev(&upstream_client).await?;

    let res = tokio::task::spawn_blocking(move || -> Result<(usize, usize, usize, usize)> {
        let count = {
            let conn = db_pool.writer();
            upsert_models_dev(&body, &conn)?
        };
        if count == 0 {
            return Ok((0, 0, 0, 0));
        }

        let enriched = {
            let conn = db_pool.writer();
            enrich_models_from_sync(&conn)?
        };

        let combos = {
            let conn = db_pool.writer();
            auto_create_combos(&conn)?
        };

        let repriced = {
            let conn = db_pool.writer();
            recompute_costs(&conn)?
        };

        Ok((count, enriched, combos, repriced))
    })
    .await
    .map_err(|e| openproxy_types::error::CoreError::Internal(e.to_string()))??;

    let (count, enriched, combos, repriced) = res;
    if count == 0 {
        return Ok("No new models.dev data".into());
    }

    Ok(format!(
        "Synced {count} models, enriched {enriched} model rows, created {combos} auto-combos, re-priced {repriced} usage rows"
    ))
}

// ── Tests ───────────────────────────────────────────────────────────-

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn create_sync_table(conn: &Connection) {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS model_capabilities_sync (
                provider_id       TEXT NOT NULL,
                model_id          TEXT NOT NULL,
                context_length    INTEGER,
                max_output_tokens INTEGER,
                pricing_input_per_1m  REAL,
                pricing_output_per_1m REAL,
                pricing_cached_per_1m REAL,
                tool_call         INTEGER,
                reasoning         INTEGER,
                vision            INTEGER,
                structured_output INTEGER,
                modalities_input  TEXT,
                modalities_output TEXT,
                family            TEXT,
                status            TEXT,
                fetched_at        TEXT,
                model_id_normalized TEXT,
                PRIMARY KEY (provider_id, model_id)
            )",
        )
        .unwrap();
    }

    /// Simulate a models.dev response with nested cost/limit and
    /// models-as-dict, covering both opencode and openai providers.
    const TEST_JSON: &str = r#"{
      "opencode": {
        "id": "opencode",
        "models": {
          "deepseek-v4-flash": {
            "id": "deepseek-v4-flash",
            "tool_call": true,
            "reasoning": false,
            "structured_output": true,
            "limit": { "context": 1000000, "output": 384000 },
            "cost": { "input": 0.14, "output": 0.28, "cache_read": 0.028 },
            "family": "deepseek-v4",
            "status": "active"
          },
          "deepseek-v4-flash-free": {
            "id": "deepseek-v4-flash-free",
            "tool_call": true,
            "reasoning": false,
            "structured_output": true,
            "limit": { "context": 200000, "output": 128000 },
            "cost": { "input": 0, "output": 0, "cache_read": 0 },
            "family": "deepseek-v4",
            "status": "active"
          }
        }
      },
      "google": {
        "id": "google",
        "models": {
          "gemini-2.5-pro": {
            "id": "gemini-2.5-pro",
            "tool_call": true,
            "reasoning": true,
            "structured_output": true,
            "limit": { "context": 1048576, "output": 65536 },
            "cost": { "input": 1.25, "output": 10.0, "cache_read": 0.25 },
            "family": "gemini-2.5",
            "status": "active"
          }
        }
      }
    }"#;

    #[test]
    fn upsert_parses_nested_format_and_stores_pricing() {
        let conn = Connection::open_in_memory().unwrap();
        create_sync_table(&conn);

        let count = upsert_models_dev(TEST_JSON.as_bytes(), &conn).unwrap();
        // opencode → mapped to opencode-zen locally + inserted under
        //   its own ext_id "opencode": 2 models × 2 ids = 4 rows.
        // google → mapped to gemini locally + inserted
        //   under its own ext_id "google": 1 model × 2 ids = 2 rows.
        // total = 6
        assert_eq!(
            count, 6,
            "should upsert 6 rows (4 opencode+opencode-zen, 2 google+gemini)"
        );
    }

    #[test]
    fn lookup_with_db_exact_match_returns_pricing() {
        let conn = Connection::open_in_memory().unwrap();
        create_sync_table(&conn);

        upsert_models_dev(TEST_JSON.as_bytes(), &conn).unwrap();

        // Exact match — gemini/gemini-cli pricing.
        let price = crate::pricing::lookup_with_db(&conn, "gemini", "gemini-2.5-pro");
        assert!(price.is_some(), "glm should have pricing");
        let p = price.unwrap();
        assert!((p.input_per_1m - 1.25).abs() < 1e-9);
        assert!((p.output_per_1m - 10.0).abs() < 1e-9);
    }

    #[test]
    fn lookup_with_db_fuzzy_free_suffix_fallback() {
        let conn = Connection::open_in_memory().unwrap();
        create_sync_table(&conn);

        upsert_models_dev(TEST_JSON.as_bytes(), &conn).unwrap();

        // User's model is "deepseek-v4-flash" (no -free suffix).
        // The sync table has both "deepseek-v4-flash" (paid) and "deepseek-v4-flash-free" (free).
        // Exact match should find the paid one.
        let price = crate::pricing::lookup_with_db(&conn, "opencode-zen", "deepseek-v4-flash");
        assert!(
            price.is_some(),
            "deepseek-v4-flash should have pricing via exact match"
        );
        let p = price.unwrap();
        assert!(
            (p.input_per_1m - 0.14).abs() < 1e-9,
            "paid model should be $0.14, got {}",
            p.input_per_1m
        );

        // User's model is "deepseek-v4-flash-free" (has -free suffix).
        // Exact match should find the free version.
        let price = crate::pricing::lookup_with_db(&conn, "opencode-zen", "deepseek-v4-flash-free");
        assert!(price.is_some());
        let p = price.unwrap();
        assert!(
            (p.input_per_1m - 0.0).abs() < 1e-9,
            "free model should be $0, got {}",
            p.input_per_1m
        );

        // User's model is "deepseek-v4-flash-free-trial" — no exact match,
        // but fuzzy fallback strips -free-trial → matches "deepseek-v4-flash".
        let price =
            crate::pricing::lookup_with_db(&conn, "opencode-zen", "deepseek-v4-flash-free-trial");
        assert!(
            price.is_some(),
            "fuzzy fallback should strip -free-trial and match"
        );
        let p = price.unwrap();
        assert!((p.input_per_1m - 0.14).abs() < 1e-9);

        // Unknown model — no match at all, falls back to static table which also has nothing.
        assert!(crate::pricing::lookup_with_db(&conn, "opencode-zen", "no-such-model").is_none());
    }

    #[test]
    fn lookup_with_db_falls_back_to_static_table() {
        let conn = Connection::open_in_memory().unwrap();
        create_sync_table(&conn);

        // Empty sync table — should fall back to static.
        let price = crate::pricing::lookup_with_db(&conn, "openrouter", "openai/gpt-4o");
        assert!(price.is_some(), "should fall back to static table");
        let p = price.unwrap();
        assert!((p.input_per_1m - 2.5).abs() < 1e-9);
    }

    /// End-to-end: a models.dev canonical entry (`claude-3-5-sonnet`)
    /// should be matched by an OpenRouter-style model id that carries
    /// a date suffix (`anthropic/claude-3-5-sonnet-20241022`). This
    /// is the headline bug fixed by `model_id_normalized` — without
    /// normalization, the date suffix means the sync table's exact
    /// `model_id` doesn't match the request's model id.
    #[test]
    fn lookup_with_db_normalized_matches_date_suffix() {
        let conn = Connection::open_in_memory().unwrap();
        create_sync_table(&conn);

        // Seed the sync table with a canonical models.dev entry —
        // note no date suffix, no provider prefix.
        let json = r#"{
            "anthropic": {
                "id": "anthropic",
                "models": {
                    "claude-3-5-sonnet": {
                        "id": "claude-3-5-sonnet",
                        "tool_call": true,
                        "reasoning": false,
                        "structured_output": true,
                        "limit": { "context": 200000, "output": 8192 },
                        "cost": { "input": 3.0, "output": 15.0, "cache_read": 0.3 },
                        "family": "claude-3.5",
                        "status": "active"
                    }
                }
            }
        }"#;
        upsert_models_dev(json.as_bytes(), &conn).unwrap();

        // Request a model id with the OpenRouter-style prefix and a
        // date suffix. The exact match fails; the normalized lookup
        // strips the prefix and the date and finds `claude-3-5-sonnet`.
        let price = crate::pricing::lookup_with_db(
            &conn,
            "openrouter",
            "anthropic/claude-3-5-sonnet-20241022",
        );
        assert!(
            price.is_some(),
            "normalized lookup should match the date-suffixed model"
        );
        let p = price.unwrap();
        assert!(
            (p.input_per_1m - 3.0).abs() < 1e-9,
            "expected $3.0/1M from claude-3-5-sonnet, got {}",
            p.input_per_1m
        );
        assert!((p.output_per_1m - 15.0).abs() < 1e-9);
    }

    /// End-to-end: the enrichment path should refresh a non-custom
    /// model's `context_length` from models.dev via the normalized
    /// match, even when the request model id has a date suffix that
    /// the sync table's `model_id` doesn't carry.
    #[test]
    fn enrich_via_normalized_matches_date_suffix() {
        let conn = Connection::open_in_memory().unwrap();
        // Need a minimal `models` table for `enrich_models_from_sync`
        // to UPDATE. The production schema lives in migration 000014,
        // but for the test we only need the columns the enrichment
        // touches: `model_id`, `provider_id`, `context_length`,
        // `max_output_tokens`, `custom`, and `model_id_normalized`.
        conn.execute_batch(
            "CREATE TABLE models (
                 provider_id         TEXT NOT NULL,
                 model_id            TEXT NOT NULL,
                 context_length      INTEGER,
                 max_output_tokens   INTEGER,
                 capabilities_json   TEXT,
                 family              TEXT,
                 input_modalities_json TEXT,
                 output_modalities_json TEXT,
                 model_type          TEXT NOT NULL DEFAULT 'chat',
                 custom              INTEGER NOT NULL DEFAULT 0,
                 model_id_normalized TEXT,
                 UNIQUE(provider_id, model_id)
             );",
        )
        .unwrap();
        create_sync_table(&conn);

        // Seed the sync table with a canonical entry.
        let json = r#"{
            "anthropic": {
                "id": "anthropic",
                "models": {
                    "claude-3-5-sonnet": {
                        "id": "claude-3-5-sonnet",
                        "tool_call": true,
                        "reasoning": false,
                        "structured_output": true,
                        "limit": { "context": 200000, "output": 8192 },
                        "cost": { "input": 3.0, "output": 15.0, "cache_read": 0.3 },
                        "family": "claude-3.5",
                        "status": "active"
                    }
                }
            }
        }"#;
        upsert_models_dev(json.as_bytes(), &conn).unwrap();

        // Pre-populate the models table with a row whose model_id
        // carries a date suffix and whose normalized form should
        // match the sync table. The heuristic context_length is set
        // to a deliberately wrong value so we can prove the
        // enrichment overwrites it (refresh behavior).
        let normalized =
            crate::model_normalize::normalize_model_id("anthropic/claude-3-5-sonnet-20241022");
        conn.execute(
            "INSERT INTO models (provider_id, model_id, context_length, custom, model_id_normalized) \
             VALUES ('openrouter', 'anthropic/claude-3-5-sonnet-20241022', 128000, 0, ?1)",
            rusqlite::params![&normalized],
        )
        .unwrap();

        // Run the enrichment.
        let touched = enrich_models_from_sync(&conn).unwrap();
        assert!(touched >= 1, "enrichment should touch at least one row");

        // The refresh should have overwritten 128000 with 200000.
        let ctx: i64 = conn
            .query_row(
                "SELECT context_length FROM models \
                 WHERE provider_id = 'openrouter' \
                   AND model_id = 'anthropic/claude-3-5-sonnet-20241022'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            ctx, 200_000,
            "context_length should be refreshed to models.dev's 200000, got {ctx}"
        );
    }

    #[test]
    fn auto_create_combos_appends_new_targets() {
        let conn = Connection::open_in_memory().unwrap();

        // Create models, combos, combo_targets, and accounts tables.
        conn.execute_batch(
            "CREATE TABLE models (
                 id                  INTEGER PRIMARY KEY AUTOINCREMENT,
                 provider_id         TEXT NOT NULL,
                 model_id            TEXT NOT NULL,
                 context_length      INTEGER,
                 active              INTEGER NOT NULL DEFAULT 1,
                 custom              INTEGER NOT NULL DEFAULT 0,
                 model_id_normalized TEXT,
                 UNIQUE(provider_id, model_id)
             );
             CREATE TABLE accounts (
                 id                  INTEGER PRIMARY KEY AUTOINCREMENT,
                 provider_id         TEXT NOT NULL,
                 health_status       TEXT NOT NULL DEFAULT 'healthy'
             );
             CREATE TABLE combos (
                 id                  INTEGER PRIMARY KEY AUTOINCREMENT,
                 name                TEXT NOT NULL UNIQUE,
                 strategy            TEXT NOT NULL,
                 race_size           INTEGER NOT NULL
             );
             CREATE TABLE combo_targets (
                 id                  INTEGER PRIMARY KEY AUTOINCREMENT,
                 combo_id            INTEGER NOT NULL REFERENCES combos(id) ON DELETE CASCADE,
                 provider_id         TEXT NOT NULL,
                 account_id          INTEGER REFERENCES accounts(id),
                 model_row_id        INTEGER REFERENCES models(id) ON DELETE CASCADE,
                 priority_order      INTEGER NOT NULL,
                 UNIQUE(combo_id, account_id, model_row_id)
             );",
        )
        .unwrap();

        // 1. Insert models with different naming conventions that normalize to "gpt-oss-120b"
        conn.execute(
            "INSERT INTO models (provider_id, model_id, model_id_normalized) VALUES ('nvidia-nim', 'openai/gpt-oss-120b', 'gpt-oss-120b')",
            []
        ).unwrap();
        conn.execute(
            "INSERT INTO models (provider_id, model_id, model_id_normalized) VALUES ('groq', 'openai/gpt-oss-120b', 'gpt-oss-120b')",
            []
        ).unwrap();
        conn.execute(
            "INSERT INTO models (provider_id, model_id, model_id_normalized) VALUES ('ollama-cloud', 'gpt-oss:120b', 'gpt-oss-120b')",
            []
        ).unwrap();

        // Insert accounts for these providers (to make them healthy/active)
        conn.execute("INSERT INTO accounts (id, provider_id, health_status) VALUES (1, 'nvidia-nim', 'healthy')", []).unwrap();
        conn.execute(
            "INSERT INTO accounts (id, provider_id, health_status) VALUES (2, 'groq', 'healthy')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO accounts (id, provider_id, health_status) VALUES (3, 'ollama-cloud', 'healthy')", []).unwrap();

        // 2. Run auto_create_combos.
        // It should group all three and create one combo "auto:gpt-oss-120b"
        let count = auto_create_combos(&conn).unwrap();
        assert_eq!(count, 1, "Should create 1 auto combo");

        // Verify the combo name and targets
        let combo_id: i64 = conn
            .query_row(
                "SELECT id FROM combos WHERE name = 'auto:gpt-oss-120b'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let targets_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM combo_targets WHERE combo_id = ?1",
                rusqlite::params![combo_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(targets_count, 3, "Should have 3 targets in the combo");

        // Verify priority orders are 0, 1, 2
        let orders: Vec<i32> = {
            let mut stmt = conn.prepare("SELECT priority_order FROM combo_targets WHERE combo_id = ?1 ORDER BY priority_order").unwrap();
            let rows = stmt
                .query_map(rusqlite::params![combo_id], |r| r.get::<_, i32>(0))
                .unwrap();
            rows.map(|r| r.unwrap()).collect()
        };
        assert_eq!(orders, vec![0, 1, 2]);

        // 3. Insert another model that normalizes to "gpt-oss-120b"
        conn.execute(
            "INSERT INTO models (provider_id, model_id, model_id_normalized) VALUES ('cerebras', 'gpt-oss-120b', 'gpt-oss-120b')",
            []
        ).unwrap();
        conn.execute("INSERT INTO accounts (id, provider_id, health_status) VALUES (4, 'cerebras', 'healthy')", []).unwrap();

        // 4. Run auto_create_combos again.
        // Since the combo already exists, it should not be "created" again (count = 0).
        // But it should append the new cerebras target to the existing combo.
        let count2 = auto_create_combos(&conn).unwrap();
        assert_eq!(count2, 0, "No new combos should be created");

        let targets_count2: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM combo_targets WHERE combo_id = ?1",
                rusqlite::params![combo_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(targets_count2, 4, "Should now have 4 targets in the combo");

        // Verify the new target has priority_order = 3
        let orders2: Vec<i32> = {
            let mut stmt = conn.prepare("SELECT priority_order FROM combo_targets WHERE combo_id = ?1 ORDER BY priority_order").unwrap();
            let rows = stmt
                .query_map(rusqlite::params![combo_id], |r| r.get::<_, i32>(0))
                .unwrap();
            rows.map(|r| r.unwrap()).collect()
        };
        assert_eq!(orders2, vec![0, 1, 2, 3]);
    }

    #[test]
    fn test_resolved_provider_map_uses_adapter_metadata() {
        let map = &*RESOLVED_PROVIDER_MAP;

        // Gemini adapter provides canonical id "google"
        let google_mapped = map.get("google").expect("google must be mapped");
        assert!(google_mapped.iter().any(|s| &**s == "gemini"));

        // MiniMax adapter provides canonical id "minimax"
        let minimax_mapped = map.get("minimax").expect("minimax must be mapped");
        assert!(minimax_mapped.iter().any(|s| &**s == "minimax"));
        assert!(minimax_mapped.iter().any(|s| &**s == "minimax-cn"));

        // OpenRouter adapter provides canonical ids "openai", "anthropic", "meta"
        let openai_mapped = map.get("openai").expect("openai must be mapped");
        assert!(openai_mapped.iter().any(|s| &**s == "openrouter"));

        let anthropic_mapped = map.get("anthropic").expect("anthropic must be mapped");
        assert!(anthropic_mapped.iter().any(|s| &**s == "openrouter"));

        let meta_mapped = map.get("meta").expect("meta must be mapped");
        assert!(meta_mapped.iter().any(|s| &**s == "openrouter"));

        // NVIDIA NIM adapter provides canonical id "nvidia"
        let nvidia_mapped = map.get("nvidia").expect("nvidia must be mapped");
        assert!(nvidia_mapped.iter().any(|s| &**s == "nvidia-nim"));

        // OpenCode Zen adapter provides canonical id "opencode"
        let opencode_mapped = map.get("opencode").expect("opencode must be mapped");
        assert!(opencode_mapped.iter().any(|s| &**s == "opencode-zen"));

        // OpenCode Go adapter provides canonical id "opencode-go"
        let opencode_go_mapped = map.get("opencode-go").expect("opencode-go must be mapped");
        assert!(opencode_go_mapped.iter().any(|s| &**s == "opencode-go"));
    }
}
