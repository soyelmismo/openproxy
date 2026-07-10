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
use crate::upstream::{CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest};
use rusqlite::Connection;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;

/// models.dev API source URL.
const MODELS_DEV_URL: &str = "https://models.dev/api.json";

/// Provider mapping: models.dev provider id → our internal IDs.
///
/// When a model's own provider is not listed here (e.g. `nous-research`,
/// `ollama-cloud`, `kilocode`, `antigravity`), the cross-provider fallback
/// in `enrich_models_from_sync()` and `pricing::lookup_with_db()` will
/// still find pricing/context by matching on model_id alone.
const PROVIDER_MAP: &[(&str, &[&str])] = &[
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
    ("opencode-go", &["opencode-zen"]),
    ("perplexity", &["openrouter"]),
    ("groq", &["openrouter"]),
    ("together", &["openrouter"]),
    ("fireworks", &["openrouter"]),
    ("deepinfra", &["openrouter"]),
    ("xai", &["openrouter"]),
];

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

/// Fetch models.dev data, map providers, upsert into DB.
/// The caller must supply the already-fetched API response bytes so
/// that `&Connection` is not held across async boundaries.
pub fn upsert_models_dev(body: &[u8], conn: &Connection) -> Result<usize> {
    // models.dev root is a flat dict: provider_id -> { id, name, models }
    let root: HashMap<String, serde_json::Value> = serde_json::from_slice(body)
        .map_err(|e| CoreError::Parse(format!("models.dev parse: {e}")))?;

    let mut total = 0usize;

    for (ext_id, provider_val) in &root {
        // We insert every models.dev provider — not just those in
        // `PROVIDER_MAP`. The cross-provider matching in
        // `enrich_models_from_sync` and `pricing::lookup_with_db`
        // matches by `model_id_normalized` across all providers, so
        // dropping the gate makes more data available (e.g. for
        // `nous-research`, `cerebras`, `hyperbolic`, etc.).
        //
        // For each models.dev provider we still insert under BOTH the
        // models.dev `ext_id` AND any mapped local provider ids. The
        // latter keeps `auto_create_combos` working (it joins on
        // `provider_id` and needs the local id).
        let ext_id_str: &str = ext_id.as_str();
        let mapped_ids: &[&str] = PROVIDER_MAP
            .iter()
            .find(|(ext, _)| *ext == ext_id_str)
            .map(|(_, ids)| *ids)
            .unwrap_or(&[]);

        let mut all_ids: Vec<&str> = Vec::with_capacity(1 + mapped_ids.len());
        all_ids.push(ext_id_str);
        all_ids.extend_from_slice(mapped_ids);

        // Get models dict for this provider.
        let Some(models_obj) = provider_val.get("models").and_then(|v| v.as_object()) else {
            continue;
        };

        let mut stmt = conn.prepare(
            "INSERT INTO model_capabilities_sync \
             (provider_id, model_id, context_length, max_output_tokens, \
              pricing_input_per_1m, pricing_output_per_1m, pricing_cached_per_1m, \
              tool_call, reasoning, vision, structured_output, \
              modalities_input, modalities_output, family, status, \
              model_id_normalized) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16) \
             ON CONFLICT(provider_id, model_id) DO UPDATE SET \
              context_length       = coalesce(excluded.context_length,       model_capabilities_sync.context_length),
              max_output_tokens    = coalesce(excluded.max_output_tokens,    model_capabilities_sync.max_output_tokens),
              pricing_input_per_1m = coalesce(excluded.pricing_input_per_1m, model_capabilities_sync.pricing_input_per_1m),
              pricing_output_per_1m= coalesce(excluded.pricing_output_per_1m,model_capabilities_sync.pricing_output_per_1m),
              pricing_cached_per_1m= coalesce(excluded.pricing_cached_per_1m,model_capabilities_sync.pricing_cached_per_1m),
              tool_call     = coalesce(excluded.tool_call,     model_capabilities_sync.tool_call),
              reasoning     = coalesce(excluded.reasoning,     model_capabilities_sync.reasoning),
              vision        = coalesce(excluded.vision,        model_capabilities_sync.vision),
              structured_output = coalesce(excluded.structured_output, model_capabilities_sync.structured_output),
              modalities_input  = coalesce(excluded.modalities_input,  model_capabilities_sync.modalities_input),
              modalities_output = coalesce(excluded.modalities_output, model_capabilities_sync.modalities_output),
              family        = coalesce(excluded.family,        model_capabilities_sync.family),
              status        = coalesce(excluded.status,        model_capabilities_sync.status),
              model_id_normalized = coalesce(excluded.model_id_normalized, model_capabilities_sync.model_id_normalized),
              fetched_at    = strftime('%Y-%m-%dT%H:%M:%SZ','now')"
        ).map_err(crate::error::map_db_error)?;

        for model_val in models_obj.values() {
            let model: ModelsDevModel = match serde_json::from_value(model_val.clone()) {
                Ok(m) => m,
                Err(_) => continue,
            };

            // Extract nested values.
            let context = model.limit.as_ref().and_then(|l| l.context);
            let max_output = model.limit.as_ref().and_then(|l| l.output);
            let input_price = model.cost.as_ref().and_then(|c| c.input);
            let output_price = model.cost.as_ref().and_then(|c| c.output);
            let cached_price = model.cost.as_ref().and_then(|c| c.cache_read);

            let mod_in = model
                .modalities
                .as_ref()
                .and_then(|m| m.input.as_ref())
                .map(|v| serde_json::to_string(v).unwrap_or_default());
            let mod_out = model
                .modalities
                .as_ref()
                .and_then(|m| m.output.as_ref())
                .map(|v| serde_json::to_string(v).unwrap_or_default());

            let normalized = crate::model_normalize::normalize_model_id(&model.id);

            for our_id in &all_ids {
                stmt.execute(rusqlite::params![
                    our_id,
                    &model.id,
                    context,
                    max_output,
                    input_price,
                    output_price,
                    cached_price,
                    model.tool_call.map(|b| b as i64),
                    model.reasoning.map(|b| b as i64),
                    None::<i64>, // vision not present in API
                    model.structured_output.map(|b| b as i64),
                    mod_in,
                    mod_out,
                    model.family.as_deref(),
                    model.status.as_deref(),
                    &normalized,
                ])
                .map_err(crate::error::map_db_error)?;
                total += 1;
            }
        }
    }

    Ok(total)
}

/// Fetch raw JSON bytes from models.dev.
///
/// Wraps a single HTTP attempt in a retry loop: up to `MAX_RETRIES`
/// attempts with exponential backoff (2s, 4s, 8s). A single transient
/// network failure no longer means a 24h wait for the next scheduler
/// tick — we retry in place before bubbling the error up to the
/// scheduler, which would otherwise skip this tick and wait for the
/// next one.
async fn fetch_models_dev(upstream: &Arc<UpstreamClient>) -> Result<bytes::Bytes> {
    const MAX_RETRIES: u32 = 3;
    const INITIAL_BACKOFF: std::time::Duration = std::time::Duration::from_secs(2);

    let mut backoff = INITIAL_BACKOFF;
    for attempt in 1..=MAX_RETRIES {
        match fetch_models_dev_once(upstream).await {
            Ok(bytes) => {
                if attempt > 1 {
                    tracing::info!(attempt, "models.dev fetch succeeded after retry");
                }
                return Ok(bytes);
            }
            Err(e) => {
                if attempt == MAX_RETRIES {
                    tracing::warn!(
                        attempt,
                        error = %e,
                        "models.dev fetch failed after all retries"
                    );
                    return Err(e);
                }
                tracing::warn!(
                    attempt,
                    next_backoff_ms = backoff.as_millis() as u64,
                    error = %e,
                    "models.dev fetch failed; retrying"
                );
                tokio::time::sleep(backoff).await;
                backoff *= 2;
            }
        }
    }
    // Unreachable: the loop body returns on every iteration — `Ok`
    // immediately, `Err` either after sleeping (mid-loop) or after
    // logging + returning on the final attempt. Kept to satisfy the
    // function's return type without `unreachable!` panicking in
    // release builds if the const is ever changed to 0.
    Err(CoreError::UpstreamConnection(
        "models.dev fetch: retry loop exhausted".into(),
    ))
}

/// Single attempt to fetch raw JSON bytes from models.dev. Used by
/// `fetch_models_dev`'s retry loop.
async fn fetch_models_dev_once(upstream: &Arc<UpstreamClient>) -> Result<bytes::Bytes> {
    let req = UpstreamRequest::get(MODELS_DEV_URL);
    let cancel = CancellationToken::new();
    let response = upstream
        .call(req, TimeoutProfile::ModelDiscovery, cancel)
        .await
        .map_err(|e| match e {
            crate::upstream::UpstreamError::Cancel => CoreError::ClientDisconnected,
            other => CoreError::UpstreamConnection(format!("models.dev fetch: {other}")),
        })?;

    let status = response.status;
    let body = response.collect().await.map_err(|e| match e {
        crate::upstream::UpstreamError::Cancel => CoreError::ClientDisconnected,
        other => CoreError::UpstreamConnection(format!("models.dev body read: {other}")),
    })?;

    if !status.is_success() {
        let text = String::from_utf8_lossy(&body);
        return Err(CoreError::UpstreamError {
            status: status.as_u16(),
            provider: "models.dev".into(),
            model: "<sync>".into(),
            body: text.to_string(),
            is_proxy_rotated: false,
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
pub fn backfill_model_id_normalized(conn: &Connection) -> Result<usize> {
    let mut total = 0usize;

    // ── Backfill `models` table ──────────────────────────────────────
    let model_rows: Vec<(String, String)> = {
        let mut stmt = conn
            .prepare("SELECT provider_id, model_id FROM models WHERE model_id_normalized IS NULL")
            .map_err(crate::error::map_db_error)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(crate::error::map_db_error)?;
        rows.filter_map(|r| r.ok()).collect()
    };
    {
        let mut stmt = conn
            .prepare(
                "UPDATE models SET model_id_normalized = ?1 \
                 WHERE provider_id = ?2 AND model_id = ?3",
            )
            .map_err(crate::error::map_db_error)?;
        for (provider_id, model_id) in &model_rows {
            let normalized = crate::model_normalize::normalize_model_id(model_id);
            stmt.execute(rusqlite::params![&normalized, provider_id, model_id])
                .map_err(crate::error::map_db_error)?;
            total += 1;
        }
    }

    // ── Backfill `model_capabilities_sync` table ─────────────────────
    // The sync table's rows are inserted by `upsert_models_dev` which
    // already computes `model_id_normalized`. But rows inserted before
    // migration 000033 deployed (or rows from a failed partial sync)
    // would have NULL. Backfill them the same way.
    let sync_rows: Vec<(String, String)> = {
        let mut stmt = conn
            .prepare(
                "SELECT provider_id, model_id FROM model_capabilities_sync \
                 WHERE model_id_normalized IS NULL",
            )
            .map_err(crate::error::map_db_error)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(crate::error::map_db_error)?;
        rows.filter_map(|r| r.ok()).collect()
    };
    {
        let mut stmt = conn
            .prepare(
                "UPDATE model_capabilities_sync SET model_id_normalized = ?1 \
                 WHERE provider_id = ?2 AND model_id = ?3",
            )
            .map_err(crate::error::map_db_error)?;
        for (provider_id, model_id) in &sync_rows {
            let normalized = crate::model_normalize::normalize_model_id(model_id);
            stmt.execute(rusqlite::params![&normalized, provider_id, model_id])
                .map_err(crate::error::map_db_error)?;
            total += 1;
        }
    }

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
/// at record time). After a models.dev sync populates pricing, this
/// function re-applies [`pricing::lookup_with_db`] to those rows and
/// updates their `cost_usd`.
///
/// Returns the number of rows re-priced.
type UsageRow = (i64, String, String, Option<u32>, Option<u32>);

pub fn recompute_costs(conn: &Connection) -> Result<usize> {
    // Load rows that need re-pricing.
    let rows: Vec<UsageRow> = {
        let mut stmt = conn
            .prepare(
                "SELECT id, provider_id, upstream_model_id, prompt_tokens, completion_tokens \
                 FROM usage \
                 WHERE cost_usd = 0.0 \
                   AND (prompt_tokens > 0 OR completion_tokens > 0)",
            )
            .map_err(crate::error::map_db_error)?;
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
            .map_err(crate::error::map_db_error)?;
        result.filter_map(|r| r.ok()).collect()
    };

    let mut updated = 0usize;
    {
        let mut stmt = conn
            .prepare("UPDATE usage SET cost_usd = ?1 WHERE id = ?2")
            .map_err(crate::error::map_db_error)?;
        for (id, provider_id, model_id, prompt_tokens, completion_tokens) in &rows {
            let price = crate::pricing::lookup_with_db(conn, provider_id, model_id);
            // If the model is a "free" variant (suffix `:free`, `-free`,
            // `-free-trial`) and the sync table returned a $0 price (the
            // free tier), try to find the PAID model's price instead. This
            // makes the analytics show the "cost saved" — what the request
            // WOULD have cost if the operator weren't using the free tier.
            // The operator can still see it's a free model by the model_id
            // in the row.
            let price = match price {
                Some(p) if p.input_per_1m == 0.0 && p.output_per_1m == 0.0 => {
                    // Free-tier price found; try the paid variant.
                    let base_model = crate::model_normalize::normalize_model_id(model_id);
                    let paid = crate::pricing::lookup_by_normalized(conn, &base_model)
                        .filter(|p| p.input_per_1m > 0.0 || p.output_per_1m > 0.0);
                    paid.or(Some(p))
                }
                other => other,
            };
            if let Some(p) = price {
                let prompt = prompt_tokens.unwrap_or(0) as f64;
                let completion = completion_tokens.unwrap_or(0) as f64;
                let cost = p.input_per_1m * prompt / 1_000_000.0
                    + p.output_per_1m * completion / 1_000_000.0;
                if cost > 0.0 {
                    stmt.execute(rusqlite::params![cost, id])
                        .map_err(crate::error::map_db_error)?;
                    updated += 1;
                }
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

/// After a sync, refresh `models.context_length`, `max_output_tokens`,
/// and `capabilities_json` from the `model_capabilities_sync` table.
///
/// Matching is done in a SINGLE pass on the `model_id_normalized`
/// column — a precomputed, application-side normalization of the
/// model id that strips provider prefixes, date suffixes (`-20241022`,
/// `-2024-04-09`), version suffixes (`-v1`, `-2407`), free suffixes
/// (`:free`, `-free-trial`, `-free`), and normalizes family naming
/// (`gemini-2_5-pro` → `gemini-2.5-pro`). This lets
/// `anthropic/claude-3-5-sonnet-20241022` match models.dev's
/// `claude-3-5-sonnet`.
///
/// ## Refresh behavior (no longer "sticky heuristic")
///
/// The previous implementation gated updates on `WHERE context_length
/// IS NULL`, which meant a heuristic-set value at boot would never be
/// replaced by models.dev's (more accurate) value. Now we **always
/// refresh** non-custom rows: the `COALESCE(sync_value, models.<col>)`
/// form prefers the sync value and falls back to the existing one,
/// while `WHERE models.custom = 0` skips operator-curated rows so an
/// admin's hand-set context window survives a sync.
///
/// Returns the number of `models` rows touched across all three
/// enriched columns.
pub fn enrich_models_from_sync(conn: &Connection) -> Result<usize> {
    // ── Backfill model_id_normalized for existing rows ───────────────
    // Migration 000033 added the column but left it NULL for pre-existing
    // rows. Without this backfill, the enrichment queries below (which
    // gate on `WHERE models.model_id_normalized IS NOT NULL`) would skip
    // every model discovered before the migration deployed.
    backfill_model_id_normalized(conn)?;

    // ── context_length: refresh from sync on normalized match ───────
    let ctx = conn
        .execute(
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
        .map_err(crate::error::map_db_error)?;

    // ── max_output_tokens: refresh from sync on normalized match ────
    let tok = conn
        .execute(
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
        .map_err(crate::error::map_db_error)?;

    // ── capabilities_json: refresh from sync on normalized match ────
    //
    // We use `json_patch` to merge the sync row's capability flags
    // (vision, tool_calling, reasoning, structured_output) into the
    // existing `capabilities_json` so any operator-set fields are
    // preserved. The sync row is only applied when at least one flag
    // is non-NULL — otherwise the SELECT would emit an all-null patch
    // that wipes the existing JSON.
    let cap = conn
        .execute(
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
        .map_err(crate::error::map_db_error)?;

    Ok(ctx + tok + cap)
}

/// Auto-create combos for models that are active in ≥2 providers.
/// For each model_id that appears in multiple active providers with
/// healthy accounts, create a `priority` combo with one target per
/// provider+account combo (first account per provider).
/// Returns the number of combos created.
pub fn auto_create_combos(conn: &Connection) -> Result<usize> {
    // Find model_id_normalized values that are active in ≥2 different providers with
    // healthy accounts. We use the `models` table joined with accounts.
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
        .map_err(crate::error::map_db_error)?;

    let normalized_ids: Vec<String> = {
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(crate::error::map_db_error)?;
        let mut ids = Vec::new();
        for row in rows {
            ids.push(row.map_err(crate::error::map_db_error)?);
        }
        ids
    };

    // Pre-group all active targets by model_id_normalized for the target normalized_ids to avoid N queries inside the loop.
    let targets_by_norm_id: std::collections::HashMap<String, Vec<(i64, String, i64)>> = {
        if normalized_ids.is_empty() {
            std::collections::HashMap::new()
        } else {
            // Because SQLite variables max out at 999 typically, we query in chunks if needed.
            // But since normalized_ids is already filtered by `HAVING COUNT >= 2`, it's rarely > 999 at once.
            // Even if it is, doing a full table scan is acceptable because this sync runs infrequently
            // in a background task.
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
                .map_err(crate::error::map_db_error)?;

            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<i64>>(3)?.unwrap_or(-1),
                    ))
                })
                .map_err(crate::error::map_db_error)?;

            let mut map: std::collections::HashMap<String, Vec<(i64, String, i64)>> =
                std::collections::HashMap::new();
            // We can optionally filter here, but we iterate over `normalized_ids` later anyway.
            for row in rows {
                let (norm_id, row_id, provider_id, account_id) =
                    row.map_err(crate::error::map_db_error)?;
                map.entry(norm_id)
                    .or_default()
                    .push((row_id, provider_id, account_id));
            }
            map
        }
    };

    // Filter combos to just those related to our normalized_ids
    let combo_names: Vec<String> = normalized_ids
        .iter()
        .map(|id| format!("auto:{}", id))
        .collect();

    let existing_combos: std::collections::HashMap<String, i64> = {
        if combo_names.is_empty() {
            std::collections::HashMap::new()
        } else {
            let mut stmt = conn
                .prepare("SELECT name, id FROM combos")
                .map_err(crate::error::map_db_error)?;
            stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(crate::error::map_db_error)?
            .filter_map(|r| r.ok())
            .filter(|(name, _)| combo_names.contains(name)) // only load what we care about
            .collect()
        }
    };

    let combo_ids: Vec<i64> = existing_combos.values().copied().collect();
    let existing_targets: std::collections::HashSet<(i64, i64, i64)> = {
        if combo_ids.is_empty() {
            std::collections::HashSet::new()
        } else {
            // We use IN (?) bindings where possible, but if there's > 999 we can chunk or just fetch what we need
            // Here, fetching only for known combos is much smaller than the full table.
            // In a background job context, a manual filter like this is safe if chunking isn't strictly required
            // but let's implement chunks just to be fully SQLite-safe.
            let mut set = std::collections::HashSet::new();
            for chunk in combo_ids.chunks(900) {
                let placeholders = vec!["?"; chunk.len()].join(",");
                let query = format!(
                    "SELECT combo_id, account_id, model_row_id FROM combo_targets WHERE combo_id IN ({})",
                    placeholders
                );
                let mut stmt = conn.prepare(&query).map_err(crate::error::map_db_error)?;
                let params = rusqlite::params_from_iter(chunk.iter());
                let rows = stmt
                    .query_map(params, |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, Option<i64>>(1)?.unwrap_or(-1),
                            row.get::<_, i64>(2)?,
                        ))
                    })
                    .map_err(crate::error::map_db_error)?;
                for row in rows.filter_map(|r| r.ok()) {
                    set.insert(row);
                }
            }
            set
        }
    };

    let mut max_orders: std::collections::HashMap<i64, i32> = {
        if combo_ids.is_empty() {
            std::collections::HashMap::new()
        } else {
            let mut map = std::collections::HashMap::new();
            for chunk in combo_ids.chunks(900) {
                let placeholders = vec!["?"; chunk.len()].join(",");
                let query = format!(
                    "SELECT combo_id, MAX(priority_order) FROM combo_targets WHERE combo_id IN ({}) GROUP BY combo_id",
                    placeholders
                );
                let mut stmt = conn.prepare(&query).map_err(crate::error::map_db_error)?;
                let params = rusqlite::params_from_iter(chunk.iter());
                let rows = stmt
                    .query_map(params, |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, i32>(1)?))
                    })
                    .map_err(crate::error::map_db_error)?;
                for (id, max_order) in rows.filter_map(|r| r.ok()) {
                    map.insert(id, max_order);
                }
            }
            map
        }
    };

    // Check if we are already in a transaction. If not, open one explicitly
    // so the multiple INSERTS don't trigger implicit per-statement fsyncs.
    let is_in_tx = !conn.is_autocommit();
    if !is_in_tx {
        conn.execute("BEGIN", [])
            .map_err(crate::error::map_db_error)?;
    }

    let mut created = 0usize;

    // Prepare statements outside the loop for insertions.
    let mut insert_combo_stmt = conn
        .prepare("INSERT INTO combos (name, strategy, race_size) VALUES (?1, 'priority', ?2)")
        .map_err(crate::error::map_db_error)?;

    let mut insert_target_stmt = conn
        .prepare(
            "INSERT INTO combo_targets (combo_id, provider_id, account_id, model_row_id, priority_order) VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .map_err(crate::error::map_db_error)?;

    for norm_id in &normalized_ids {
        let combo_name = format!("auto:{}", norm_id);

        let combo_id = existing_combos.get(&combo_name).copied();

        let empty_targets = Vec::new();
        let targets = targets_by_norm_id.get(norm_id).unwrap_or(&empty_targets);

        if targets.len() < 2 {
            continue;
        }

        let combo_id = match combo_id {
            Some(id) => id,
            None => {
                // Create the combo. Use race_size = min(targets, 3) so
                // parallel race fires across available providers.
                let race_size = (targets.len() as u8).min(3);
                insert_combo_stmt
                    .execute(rusqlite::params![&combo_name, race_size])
                    .map_err(crate::error::map_db_error)?;

                created += 1;
                conn.last_insert_rowid()
            }
        };

        // Insert new targets (append-only logic).
        for &(row_id, ref provider_id, account_id) in targets {
            let target_exists =
                existing_targets.contains(&(combo_id, account_id, row_id));

            if !target_exists {
                let current_max = max_orders.get(&combo_id).copied().unwrap_or(-1);
                let next_order = current_max + 1;
                max_orders.insert(combo_id, next_order);

                insert_target_stmt
                    .execute(rusqlite::params![
                        combo_id,
                        provider_id,
                        account_id,
                        row_id,
                        next_order
                    ])
                    .map_err(crate::error::map_db_error)?;
            }
        }
    }

    if !is_in_tx {
        conn.execute("COMMIT", [])
            .map_err(crate::error::map_db_error)?;
    }

    Ok(created)
}

/// Background sync task: periodically fetch models.dev, enrich, and
/// auto-create combos. Runs at the configured interval.
pub async fn start_sync_scheduler(
    db_pool: std::sync::Arc<crate::db::DbPool>,
    upstream_client: Arc<UpstreamClient>,
    check_interval_secs: u64,
) {
    // `tokio::time::interval` fires immediately on the first call to
    // `tick()`, so the first sync runs at boot instead of waiting
    // `check_interval_secs`. The previous code skipped the first
    // immediate tick (`tick.tick().await` before the loop), which meant
    // a fresh boot with `MODELS_DEV_SYNC_ENABLED=true` didn't sync
    // until `check_interval_secs` (24h by default) after startup.
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(check_interval_secs));

    loop {
        tick.tick().await;

        tracing::info!("models.dev sync: starting");

        // Fetch models.dev FIRST, without holding any connection.
        let body = match fetch_models_dev(&upstream_client).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "models.dev sync fetch failed");
                continue;
            }
        };

        // Then upsert under the connection lock.
        let count = {
            let conn = db_pool.writer();
            match upsert_models_dev(&body, &conn) {
                Ok(n) => {
                    tracing::info!("models.dev sync: {} rows upserted", n);
                    n
                }
                Err(e) => {
                    tracing::warn!(error = %e, "models.dev sync upsert failed");
                    continue;
                }
            }
        };

        if count == 0 {
            continue;
        }

        // Enrich models table.
        {
            let conn = db_pool.writer();
            match enrich_models_from_sync(&conn) {
                Ok(n) => tracing::info!("models.dev sync: enriched {} model rows", n),
                Err(e) => tracing::warn!(error = %e, "models.dev sync enrich failed"),
            }
        }

        // Auto-create combos.
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

        tracing::info!("models.dev sync: complete");
    }
}

// ── Manual trigger helper ───────────────────────────────────────────

/// One-shot sync + enrich + auto-combo, called from the admin handler.
pub async fn run_one_shot(
    db_pool: std::sync::Arc<crate::db::DbPool>,
    upstream_client: Arc<UpstreamClient>,
) -> Result<String> {
    let body = fetch_models_dev(&upstream_client).await?;

    let count = {
        let conn = db_pool.writer();
        upsert_models_dev(&body, &conn)?
    };
    if count == 0 {
        return Ok("No new models.dev data".into());
    }

    let enriched = {
        let conn = db_pool.writer();
        enrich_models_from_sync(&conn)?
    };

    let combos = {
        let conn = db_pool.writer();
        auto_create_combos(&conn)?
    };

    // Re-price historical usage rows that had no pricing at record time.
    // After the sync populates pricing in model_capabilities_sync, this
    // walks every usage row with cost_usd = 0 AND tokens > 0 and
    // re-applies pricing::lookup_with_db. This fixes the "3147 rows had
    // no pricing data" warning after the operator runs a sync.
    let repriced = {
        let conn = db_pool.writer();
        recompute_costs(&conn)?
    };

    Ok(format!(
        "Synced {} models, enriched {} model rows, created {} auto-combos, re-priced {} usage rows",
        count, enriched, combos, repriced
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
            ctx, 200000,
            "context_length should be refreshed to models.dev's 200000, got {}",
            ctx
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
}
