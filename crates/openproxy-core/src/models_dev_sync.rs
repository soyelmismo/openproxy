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
//! triggered manually via `POST /v1/admin/models/sync-models-dev`.

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
    ("google", &["gemini", "gemini-cli"]),
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
pub fn upsert_models_dev(
    body: &[u8],
    conn: &Connection,
) -> Result<usize> {
    // models.dev root is a flat dict: provider_id -> { id, name, models }
    let root: HashMap<String, serde_json::Value> = serde_json::from_slice(body)
        .map_err(|e| CoreError::Parse(format!("models.dev parse: {e}")))?;

    let mut total = 0usize;

    for (ext_id, provider_val) in &root {
        // Map external provider id to our internal ids.
        let Some(our_ids) = PROVIDER_MAP.iter()
            .find(|(ext, _)| *ext == ext_id.as_str())
            .map(|(_, ids)| *ids)
        else {
            continue;
        };

        // Get models dict for this provider.
        let Some(models_obj) = provider_val.get("models").and_then(|v| v.as_object()) else {
            continue;
        };

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

            let mod_in = model.modalities.as_ref()
                .and_then(|m| m.input.as_ref())
                .map(|v| serde_json::to_string(v).unwrap_or_default());
            let mod_out = model.modalities.as_ref()
                .and_then(|m| m.output.as_ref())
                .map(|v| serde_json::to_string(v).unwrap_or_default());

            for our_id in our_ids {
                conn.execute(
                    "INSERT INTO model_capabilities_sync \
                     (provider_id, model_id, context_length, max_output_tokens, \
                      pricing_input_per_1m, pricing_output_per_1m, pricing_cached_per_1m, \
                      tool_call, reasoning, vision, structured_output, \
                      modalities_input, modalities_output, family, status) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15) \
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
                      fetched_at    = strftime('%Y-%m-%dT%H:%M:%SZ','now')",
                    rusqlite::params![
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
                    ],
                ).map_err(|e| CoreError::Database {
                    message: format!("models.dev upsert: {e}"),
                    source: Some(Box::new(e)),
                })?;
                total += 1;
            }
        }
    }

    Ok(total)
}

/// Fetch raw JSON bytes from models.dev.
async fn fetch_models_dev(upstream: &Arc<UpstreamClient>) -> Result<bytes::Bytes> {
    let req = UpstreamRequest::get(MODELS_DEV_URL);
    let cancel = CancellationToken::new();
    let response = upstream
        .call(req, TimeoutProfile::Quota, cancel)
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
        });
    }

    Ok(body)
}

// ── Enrichment helpers ──────────────────────────────────────────────

/// After a sync, update `models.context_length` from the capabilities
/// table for rows where `context_length IS NULL`. Also update
/// `capabilities_json` with synced capabilities.
///
/// Matching is done in two passes:
/// 1. Exact `(provider_id, model_id)` match.
/// 2. Cross-provider model_id match (stripping any `provider/` prefix)
///    for models whose own provider API doesn't return these values.
///
/// Returns the number of `models` rows updated.
pub fn enrich_models_from_sync(conn: &Connection) -> Result<usize> {
    // ── Pass 1: exact (provider_id, model_id) match ─────────────────
    let ctx_exact = conn.execute(
        "UPDATE models SET context_length = (
            SELECT context_length FROM model_capabilities_sync s
            WHERE s.provider_id = models.provider_id
              AND s.model_id   = models.model_id
              AND s.context_length IS NOT NULL
        ) WHERE context_length IS NULL
          AND EXISTS (
            SELECT 1 FROM model_capabilities_sync s
            WHERE s.provider_id = models.provider_id
              AND s.model_id   = models.model_id
              AND s.context_length IS NOT NULL
          )",
        [],
    ).map_err(|e| CoreError::Database {
        message: format!("enrich context_length: {e}"),
        source: Some(Box::new(e)),
    })?;

    let tok_exact = conn.execute(
        "UPDATE models SET max_output_tokens = (
            SELECT max_output_tokens FROM model_capabilities_sync s
            WHERE s.provider_id = models.provider_id
              AND s.model_id   = models.model_id
              AND s.max_output_tokens IS NOT NULL
        ) WHERE max_output_tokens IS NULL
          AND EXISTS (
            SELECT 1 FROM model_capabilities_sync s
            WHERE s.provider_id = models.provider_id
              AND s.model_id   = models.model_id
              AND s.max_output_tokens IS NOT NULL
          )",
        [],
    ).map_err(|e| CoreError::Database {
        message: format!("enrich max_output_tokens: {e}"),
        source: Some(Box::new(e)),
    })?;

    // ── Pass 2: cross-provider model_id match (provider prefix stripped) ──
    //
    // For models whose model_id looks like "provider/name", strip the
    // prefix and match on the base name. Also try matching the raw
    // model_id directly (handles models without a provider prefix).
    //
    // This catches models from providers that aren't in PROVIDER_MAP
    // or whose model IDs don't match exactly (e.g. OpenRouter returns
    // "openai/gpt-4o" but models.dev stores "gpt-4o").
    let ctx_fallback = conn.execute(
        "UPDATE models SET context_length = (
            SELECT s.context_length FROM model_capabilities_sync s
            WHERE s.model_id = (
                CASE WHEN instr(models.model_id, '/') > 0
                     THEN substr(models.model_id, instr(models.model_id, '/') + 1)
                     ELSE models.model_id
                END
            )
              AND s.context_length IS NOT NULL
            LIMIT 1
        ) WHERE context_length IS NULL
          AND EXISTS (
            SELECT 1 FROM model_capabilities_sync s
            WHERE s.model_id = (
                CASE WHEN instr(models.model_id, '/') > 0
                     THEN substr(models.model_id, instr(models.model_id, '/') + 1)
                     ELSE models.model_id
                END
            )
              AND s.context_length IS NOT NULL
        )",
        [],
    ).map_err(|e| CoreError::Database {
        message: format!("enrich context_length (cross-provider): {e}"),
        source: Some(Box::new(e)),
    })?;

    let tok_fallback = conn.execute(
        "UPDATE models SET max_output_tokens = (
            SELECT s.max_output_tokens FROM model_capabilities_sync s
            WHERE s.model_id = (
                CASE WHEN instr(models.model_id, '/') > 0
                     THEN substr(models.model_id, instr(models.model_id, '/') + 1)
                     ELSE models.model_id
                END
            )
              AND s.max_output_tokens IS NOT NULL
            LIMIT 1
        ) WHERE max_output_tokens IS NULL
          AND EXISTS (
            SELECT 1 FROM model_capabilities_sync s
            WHERE s.model_id = (
                CASE WHEN instr(models.model_id, '/') > 0
                     THEN substr(models.model_id, instr(models.model_id, '/') + 1)
                     ELSE models.model_id
                END
            )
              AND s.max_output_tokens IS NOT NULL
        )",
        [],
    ).map_err(|e| CoreError::Database {
        message: format!("enrich max_output_tokens (cross-provider): {e}"),
        source: Some(Box::new(e)),
    })?;

    // ── Pass 3: cross-provider, prefix stripped + free suffix stripped ──
    //
    // Many models have "-free", ":free", or "-free-trial" suffixes
    // (e.g. "openai/gpt-4o:free"). After Pass 2 strips the provider
    // prefix we get "gpt-4o:free" which still won't match the sync
    // table's "gpt-4o".  This pass strips those suffixes too.
    //
    // Uses SQLite replace() which is safe here because ":free" is
    // distinctive, "-free-trial" is specific, and "-free" in practice
    // only appears as a suffix on model IDs.

    /// Normalize a model_id by stripping provider prefix and free suffixes.
    /// Shared by the three Pass 3 queries below.
    macro_rules! normalized_id {
        () => {
            "replace(replace(replace( \
                CASE WHEN instr(models.model_id, '/') > 0 \
                     THEN substr(models.model_id, instr(models.model_id, '/') + 1) \
                     ELSE models.model_id \
                END, \
            ':free', ''), '-free-trial', ''), '-free', '')"
        };
    }

    let ctx_free = conn.execute(
        &format!(
            "UPDATE models SET context_length = (
                SELECT s.context_length FROM model_capabilities_sync s
                WHERE s.model_id = {}
                  AND s.context_length IS NOT NULL
                LIMIT 1
            ) WHERE context_length IS NULL
              AND {} != (
                CASE WHEN instr(models.model_id, '/') > 0
                     THEN substr(models.model_id, instr(models.model_id, '/') + 1)
                     ELSE models.model_id
                END
              )
              AND EXISTS (
                SELECT 1 FROM model_capabilities_sync s
                WHERE s.model_id = {}
                  AND s.context_length IS NOT NULL
            )",
            normalized_id!(),
            normalized_id!(),
            normalized_id!(),
        ),
        [],
    ).map_err(|e| CoreError::Database {
        message: format!("enrich context_length (free-suffix): {e}"),
        source: Some(Box::new(e)),
    })?;

    let tok_free = conn.execute(
        &format!(
            "UPDATE models SET max_output_tokens = (
                SELECT s.max_output_tokens FROM model_capabilities_sync s
                WHERE s.model_id = {}
                  AND s.max_output_tokens IS NOT NULL
                LIMIT 1
            ) WHERE max_output_tokens IS NULL
              AND {} != (
                CASE WHEN instr(models.model_id, '/') > 0
                     THEN substr(models.model_id, instr(models.model_id, '/') + 1)
                     ELSE models.model_id
                END
              )
              AND EXISTS (
                SELECT 1 FROM model_capabilities_sync s
                WHERE s.model_id = {}
                  AND s.max_output_tokens IS NOT NULL
            )",
            normalized_id!(),
            normalized_id!(),
            normalized_id!(),
        ),
        [],
    ).map_err(|e| CoreError::Database {
        message: format!("enrich max_output_tokens (free-suffix): {e}"),
        source: Some(Box::new(e)),
    })?;

    // ── Capabilities: exact match, then cross-provider, then free-suffix ──
    let cap_exact = conn.execute(
        "UPDATE models SET capabilities_json = (
            SELECT json_patch(
                coalesce(models.capabilities_json, '{}'),
                json_object(
                    'vision',              s.vision,
                    'tool_calling',        s.tool_call,
                    'reasoning',           s.reasoning,
                    'structured_output',   s.structured_output
                )
            )
            FROM model_capabilities_sync s
            WHERE s.provider_id = models.provider_id
              AND s.model_id   = models.model_id
              AND (s.vision IS NOT NULL OR s.tool_call IS NOT NULL
                   OR s.reasoning IS NOT NULL OR s.structured_output IS NOT NULL)
        ) WHERE EXISTS (
            SELECT 1 FROM model_capabilities_sync s
            WHERE s.provider_id = models.provider_id
              AND s.model_id   = models.model_id
              AND (s.vision IS NOT NULL OR s.tool_call IS NOT NULL
                   OR s.reasoning IS NOT NULL OR s.structured_output IS NOT NULL)
        )",
        [],
    ).map_err(|e| CoreError::Database {
        message: format!("enrich capabilities: {e}"),
        source: Some(Box::new(e)),
    })?;

    // Capabilities: cross-provider fallback (prefix stripped).
    let cap_fallback = conn.execute(
        "UPDATE models SET capabilities_json = (
            SELECT json_patch(
                coalesce(models.capabilities_json, '{}'),
                json_object(
                    'vision',              s.vision,
                    'tool_calling',        s.tool_call,
                    'reasoning',           s.reasoning,
                    'structured_output',   s.structured_output
                )
            )
            FROM model_capabilities_sync s
            WHERE s.model_id = (
                CASE WHEN instr(models.model_id, '/') > 0
                     THEN substr(models.model_id, instr(models.model_id, '/') + 1)
                     ELSE models.model_id
                END
            )
              AND (s.vision IS NOT NULL OR s.tool_call IS NOT NULL
                   OR s.reasoning IS NOT NULL OR s.structured_output IS NOT NULL)
            LIMIT 1
        ) WHERE NOT EXISTS (
            SELECT 1 FROM model_capabilities_sync s
            WHERE s.provider_id = models.provider_id
              AND s.model_id   = models.model_id
        )
          AND EXISTS (
            SELECT 1 FROM model_capabilities_sync s
            WHERE s.model_id = (
                CASE WHEN instr(models.model_id, '/') > 0
                     THEN substr(models.model_id, instr(models.model_id, '/') + 1)
                     ELSE models.model_id
                END
            )
              AND (s.vision IS NOT NULL OR s.tool_call IS NOT NULL
                   OR s.reasoning IS NOT NULL OR s.structured_output IS NOT NULL)
        )",
        [],
    ).map_err(|e| CoreError::Database {
        message: format!("enrich capabilities (cross-provider): {e}"),
        source: Some(Box::new(e)),
    })?;

    // Capabilities: free-suffix fallback (prefix + free suffix stripped).
    let cap_free = conn.execute(
        &format!(
            "UPDATE models SET capabilities_json = (
                SELECT json_patch(
                    coalesce(models.capabilities_json, '{{}}'),
                    json_object(
                        'vision',              s.vision,
                        'tool_calling',        s.tool_call,
                        'reasoning',           s.reasoning,
                        'structured_output',   s.structured_output
                    )
                )
                FROM model_capabilities_sync s
                WHERE s.model_id = {}
                  AND (s.vision IS NOT NULL OR s.tool_call IS NOT NULL
                       OR s.reasoning IS NOT NULL OR s.structured_output IS NOT NULL)
                LIMIT 1
            ) WHERE NOT EXISTS (
                SELECT 1 FROM model_capabilities_sync s
                WHERE s.provider_id = models.provider_id
                  AND s.model_id   = models.model_id
            )
              AND {} != (
                CASE WHEN instr(models.model_id, '/') > 0
                     THEN substr(models.model_id, instr(models.model_id, '/') + 1)
                     ELSE models.model_id
                END
              )
              AND EXISTS (
                SELECT 1 FROM model_capabilities_sync s
                WHERE s.model_id = {}
                  AND (s.vision IS NOT NULL OR s.tool_call IS NOT NULL
                       OR s.reasoning IS NOT NULL OR s.structured_output IS NOT NULL)
            )",
            normalized_id!(),
            normalized_id!(),
            normalized_id!(),
        ),
        [],
    ).map_err(|e| CoreError::Database {
        message: format!("enrich capabilities (free-suffix): {e}"),
        source: Some(Box::new(e)),
    })?;

    Ok(ctx_exact + tok_exact + ctx_fallback + tok_fallback
        + ctx_free + tok_free
        + cap_exact + cap_fallback + cap_free)
}

/// Auto-create combos for models that are active in ≥2 providers.
/// For each model_id that appears in multiple active providers with
/// healthy accounts, create a `priority` combo with one target per
/// provider+account combo (first account per provider).
/// Returns the number of combos created.
pub fn auto_create_combos(conn: &Connection) -> Result<usize> {
    // Find model_ids that are active in ≥2 different providers with
    // healthy accounts. We use the `models` table joined with accounts.
    let mut stmt = conn.prepare(
        "SELECT m.model_id
         FROM models m
         JOIN accounts a ON a.provider_id = m.provider_id
         WHERE m.active = 1
           AND a.health_status = 'healthy'
         GROUP BY m.model_id
         HAVING COUNT(DISTINCT m.provider_id) >= 2
         ORDER BY m.model_id",
    ).map_err(|e| CoreError::Database {
        message: format!("auto-combo query: {e}"),
        source: Some(Box::new(e)),
    })?;

    let model_ids: Vec<String> = {
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| CoreError::Database {
                message: format!("auto-combo query rows: {e}"),
                source: Some(Box::new(e)),
            })?;
        let mut ids = Vec::new();
        for row in rows {
            ids.push(row.map_err(|e| CoreError::Database {
                message: format!("auto-combo row: {e}"),
                source: Some(Box::new(e)),
            })?);
        }
        ids
    };

    let mut created = 0usize;

    for model_id in &model_ids {
        // Check if a combo for this model already exists.
        let exists: bool = conn.query_row(
            "SELECT COUNT(*) FROM combos WHERE name = ?1",
            rusqlite::params![format!("auto:{}", model_id)],
            |row| row.get::<_, i64>(0),
        ).map_err(|e| CoreError::Database {
            message: format!("auto-combo exists check: {e}"),
            source: Some(Box::new(e)),
        })? > 0;

        if exists {
            continue;
        }

        // Get all (provider_id, model_row_id, account_id) combos for this model.
        let mut target_stmt = conn.prepare(
            "SELECT m.rowid, m.provider_id, a.id
             FROM models m
             JOIN accounts a ON a.provider_id = m.provider_id AND a.health_status = 'healthy'
             WHERE m.model_id = ?1 AND m.active = 1
             ORDER BY m.provider_id",
        ).map_err(|e| CoreError::Database {
            message: format!("auto-combo targets prepare: {e}"),
            source: Some(Box::new(e)),
        })?;

        let targets: Vec<(i64, String, i64)> = {
            let rows = target_stmt.query_map(rusqlite::params![model_id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?))
            }).map_err(|e| CoreError::Database {
                message: format!("auto-combo targets rows: {e}"),
                source: Some(Box::new(e)),
            })?;
            let mut t = Vec::new();
            for row in rows {
                t.push(row.map_err(|e| CoreError::Database {
                    message: format!("auto-combo target row: {e}"),
                    source: Some(Box::new(e)),
                })?);
            }
            t
        };

        if targets.len() < 2 {
            continue;
        }

        // Create the combo. Use race_size = min(targets, 3) so
        // parallel race fires across available providers.
        let race_size = (targets.len() as u8).min(3);
        let combo_name = format!("auto:{}", model_id);
        conn.execute(
            "INSERT INTO combos (name, strategy, race_size) VALUES (?1, 'priority', ?2)",
            rusqlite::params![&combo_name, race_size],
        ).map_err(|e| CoreError::Database {
            message: format!("auto-combo insert: {e}"),
            source: Some(Box::new(e)),
        })?;

        let combo_id: i64 = conn.last_insert_rowid();

        // Insert targets.
        for (order, (row_id, provider_id, account_id)) in targets.iter().enumerate() {
            conn.execute(
                "INSERT INTO combo_targets (combo_id, provider_id, account_id, model_row_id, priority_order) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![combo_id, provider_id, account_id, row_id, order as i32],
            ).map_err(|e| CoreError::Database {
                message: format!("auto-combo target insert: {e}"),
                source: Some(Box::new(e)),
            })?;
        }

        created += 1;
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
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(check_interval_secs));
    tick.tick().await; // Skip first immediate tick.

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
        match { let conn = db_pool.writer(); enrich_models_from_sync(&conn) } {
            Ok(n) => tracing::info!("models.dev sync: enriched {} model rows", n),
            Err(e) => tracing::warn!(error = %e, "models.dev sync enrich failed"),
        }

        // Auto-create combos.
        match { let conn = db_pool.writer(); auto_create_combos(&conn) } {
            Ok(n) => {
                if n > 0 {
                    tracing::info!("models.dev sync: created {} auto-combos", n);
                }
            }
            Err(e) => tracing::warn!(error = %e, "models.dev sync auto-combo failed"),
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

    Ok(format!(
        "Synced {} models, enriched {} model rows, created {} auto-combos",
        count, enriched, combos
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
        // opencode → opencode-zen: 2 models × 1 internal id = 2 rows
        // google → gemini + gemini-cli: 1 model × 2 internal ids = 2 rows
        // total = 4
        assert_eq!(count, 4, "should upsert 4 rows (2 for opencode-zen, 2 for gemini/gemini-cli)");
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
        assert!(price.is_some(), "deepseek-v4-flash should have pricing via exact match");
        let p = price.unwrap();
        assert!((p.input_per_1m - 0.14).abs() < 1e-9, "paid model should be $0.14, got {}", p.input_per_1m);

        // User's model is "deepseek-v4-flash-free" (has -free suffix).
        // Exact match should find the free version.
        let price = crate::pricing::lookup_with_db(&conn, "opencode-zen", "deepseek-v4-flash-free");
        assert!(price.is_some());
        let p = price.unwrap();
        assert!((p.input_per_1m - 0.0).abs() < 1e-9, "free model should be $0, got {}", p.input_per_1m);

        // User's model is "deepseek-v4-flash-free-trial" — no exact match,
        // but fuzzy fallback strips -free-trial → matches "deepseek-v4-flash".
        let price = crate::pricing::lookup_with_db(&conn, "opencode-zen", "deepseek-v4-flash-free-trial");
        assert!(price.is_some(), "fuzzy fallback should strip -free-trial and match");
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
}
