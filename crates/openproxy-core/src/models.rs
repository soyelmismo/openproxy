//! Persistent model registry. Models are discovered from providers' /models endpoint.
//!
//! This module owns the `models` table (see mvp-spec §8) and the operations
//! needed by the discovery loop, the `/v1/models` admin endpoint, and the
//! request-routing pipeline.
//!
//! Note: this is *not* where OpenAI/Anthropic serde structs live — those are
//! in `crate::translation`. The two namespaces are kept separate on purpose.

use crate::error::{CoreError, Result};
use crate::ids::{ModelId, ModelRowId, ProviderId};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Output wire format the upstream model natively speaks.
///
/// Persisted in `models.target_format`; the CHECK constraint allows only
/// `"openai"`, `"anthropic"`, or `"gemini"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TargetFormat {
    Openai,
    Anthropic,
    Gemini,
}

impl TargetFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            TargetFormat::Openai => "openai",
            TargetFormat::Anthropic => "anthropic",
            TargetFormat::Gemini => "gemini",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "openai" => Ok(TargetFormat::Openai),
            "anthropic" => Ok(TargetFormat::Anthropic),
            "gemini" => Ok(TargetFormat::Gemini),
            other => Err(CoreError::Validation(format!("invalid target_format: {}", other))),
        }
    }
}

/// A row in the `models` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    pub row_id: ModelRowId,
    pub provider_id: ProviderId,
    pub model_id: ModelId,
    pub display_name: Option<String>,
    pub target_format: TargetFormat,
    pub discovered_at: String,
    pub expires_at: Option<String>,
    pub timeout_overrides_json: Option<String>,
    /// Soft-disable bit. `true` means the row participates in routing;
    /// `false` hides it from [`list_active`] but keeps it in the table so
    /// the admin can re-enable it without losing any data. The schema
    /// stamps new rows with `active = 1` via the column default.
    pub active: bool,
    /// Most recent HTTP status code from the model-test endpoint.
    /// `None` means the model has never been tested; `0` is reserved
    /// for "request never reached the upstream" (DNS / connect / TLS
    /// errors).
    pub last_test_status: Option<i32>,
    /// Wall-clock timestamp the most recent test result was stamped
    /// at, in sqlite `datetime('now')` UTC form. `None` when the model
    /// has never been tested.
    pub last_test_at: Option<String>,
    /// `true` for rows hand-created via [`create_custom`] (not produced
    /// by an adapter's `/models` discovery). The auto-activation path
    /// skips these so an operator's hand-picked entries survive a
    /// refresh.
    pub custom: bool,
    /// Upstream context window in tokens (input + output). `None` when
    /// neither the operator nor a discovery backfill has filled it in;
    /// the public `GET /v1/models` handler falls back to a heuristic
    /// derived from `model_id` in that case. Stored as a string in the
    /// DB to keep the migration's `ALTER TABLE` to a plain `ADD COLUMN`.
    pub context_length: Option<i64>,
    /// Upstream max output tokens. Same fallback story as
    /// `context_length`.
    pub max_output_tokens: Option<i64>,
    /// Serialized [`crate::capabilities::ModelCapabilities`]. The
    /// endpoint also accepts the `null` JSON value and falls back to
    /// a heuristic. Stored as a string for the same migration reason
    /// as `context_length`.
    pub capabilities_json: Option<String>,
    /// Logical model family used by client UIs (e.g. Cursor's picker)
    /// to group related entries. `None` for unknown families.
    pub family: Option<String>,
    /// High-level model kind: `"chat"`, `"embedding"`, `"image"`,
    /// `"audio"`, or `"rerank"`. The DB default is `"chat"`.
    pub model_type: String,
    /// JSON array of input modalities (e.g. `["text", "image"]`).
    pub input_modalities_json: Option<String>,
    /// JSON array of output modalities (e.g. `["text"]`).
    pub output_modalities_json: Option<String>,
}

/// Input shape for [`upsert_many`]: what a provider adapter reports.
///
/// `row_id`, `discovered_at`, and `expires_at` are not supplied by the
/// adapter — they are filled in by the storage layer.
///
/// The optional metadata fields (`context_length`, `max_output_tokens`,
/// `input_modalities`, `output_modalities`, `model_type`, `family`,
/// `capabilities`) come straight from the upstream `/models` response
/// (e.g. OpenRouter's `context_length`, `architecture.*_modalities`,
/// `top_provider.max_completion_tokens`, `supported_parameters`). A
/// provider adapter that doesn't surface those fields leaves them
/// `None` and the runtime fallback at the `GET /v1/models` handler
/// takes over.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredModel {
    pub model_id: ModelId,
    pub display_name: Option<String>,
    pub target_format: TargetFormat,
    /// Context window in tokens (from OpenRouter's `context_length`).
    pub context_length: Option<i64>,
    /// Max output tokens (from OpenRouter's
    /// `top_provider.max_completion_tokens`).
    pub max_output_tokens: Option<i64>,
    /// Input modalities (from OpenRouter's
    /// `architecture.input_modalities`).
    pub input_modalities: Option<Vec<String>>,
    /// Output modalities (from OpenRouter's
    /// `architecture.output_modalities`).
    pub output_modalities: Option<Vec<String>>,
    /// Model type: `"chat"`, `"embedding"`, `"image"`, `"audio"`,
    /// `"rerank"`.
    pub model_type: Option<String>,
    /// Family (e.g. `"Qwen3"`, `"Llama-3.3"`, `"Claude-Sonnet-4"`).
    pub family: Option<String>,
    /// Capabilities (vision, tool_calling, reasoning, structured_output,
    /// temperature). Derived from `supported_parameters` by the
    /// OpenRouter adapter.
    pub capabilities: Option<crate::capabilities::ModelCapabilities>,
}

fn map_row(row: &Row<'_>) -> rusqlite::Result<Model> {
    let target_format_str: String = row.get("target_format")?;
    let target_format = match target_format_str.as_str() {
        "openai" => TargetFormat::Openai,
        "anthropic" => TargetFormat::Anthropic,
        "gemini" => TargetFormat::Gemini,
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

    // The schema's CHECK constraint guarantees `active` is 0 or 1; map
    // it to a bool at the boundary so the rest of the code stays type
    // safe.
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
        // model_type is NOT NULL with a DEFAULT of 'chat' in the schema,
        // so the column is guaranteed to come back as a string.
        model_type: row
            .get::<_, Option<String>>("model_type")?
            .unwrap_or_else(|| "chat".to_string()),
        input_modalities_json: row.get::<_, Option<String>>("input_modalities_json")?,
        output_modalities_json: row.get::<_, Option<String>>("output_modalities_json")?,
    })
}

/// Insert or update models reported by a provider's `/models` endpoint.
///
/// For each entry in `discovered`:
/// - if `(provider_id, model_id)` does not exist, insert a new row with
///   `discovered_at = now` and `expires_at = now + ttl`;
/// - otherwise refresh only the mutable metadata (`display_name` and
///   `target_format`). `discovered_at` and `expires_at` are **preserved**
///   so that:
///   - The 60-second recency window used by [`apply_auto_activation`]
///     only flags truly *new* rows. If we refreshed `discovered_at` on
///     re-upsert, a hand-disabled model that the provider keeps listing
///     would be considered "new" on every refresh and have its
///     `active` bit clobbered.
///   - The TTL-based expiry/purge cycle keeps working: a model that
///     stops being listed by the provider will eventually hit
///     `expires_at`, get purged by [`mark_expired`], and be re-inserted
///     with a fresh `discovered_at` on the next refresh that lists it
///     again.
///
/// Returns the number of rows touched (inserts + updates). The operation
/// runs inside a single transaction so partial failures roll back.
pub fn upsert_many(
    conn: &Connection,
    provider: &ProviderId,
    discovered: &[DiscoveredModel],
    ttl: Duration,
) -> Result<usize> {
    let mut total = 0usize;
    let ttl_secs = ttl.as_secs() as i64;

    let tx = conn.unchecked_transaction().map_err(|e| CoreError::Database {
        message: format!("begin upsert_many tx: {}", e),
        source: Some(Box::new(e)),
    })?;

    {
        let mut stmt = tx
            .prepare(
                "INSERT INTO models (\
                    provider_id, model_id, display_name, target_format, \
                    discovered_at, expires_at, \
                    context_length, max_output_tokens, \
                    input_modalities_json, output_modalities_json, \
                    model_type, family, capabilities_json\
                 ) VALUES (\
                    ?, ?, ?, ?, datetime('now'), datetime('now', '+' || ? || ' seconds'), \
                    ?, ?, ?, ?, COALESCE(?, 'chat'), ?, ?\
                 ) ON CONFLICT(provider_id, model_id) DO UPDATE SET \
                    display_name = excluded.display_name, \
                    target_format = excluded.target_format, \
                    context_length = COALESCE(excluded.context_length, context_length), \
                    max_output_tokens = COALESCE(excluded.max_output_tokens, max_output_tokens), \
                    input_modalities_json = COALESCE(excluded.input_modalities_json, input_modalities_json), \
                    output_modalities_json = COALESCE(excluded.output_modalities_json, output_modalities_json), \
                    model_type = COALESCE(excluded.model_type, model_type), \
                    family = COALESCE(excluded.family, family), \
                    capabilities_json = COALESCE(excluded.capabilities_json, capabilities_json)",
            )
            .map_err(|e| CoreError::Database {
                message: format!("prepare upsert_many: {}", e),
                source: Some(Box::new(e)),
            })?;

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

            let changed = stmt
                .execute(params![
                    provider.as_str(),            // 1. provider_id
                    d.model_id.as_str(),          // 2. model_id
                    d.display_name,               // 3. display_name
                    d.target_format.as_str(),     // 4. target_format
                    ttl_secs,                     // 5. (used in the datetime '+? seconds' expr)
                    d.context_length,             // 6. context_length
                    d.max_output_tokens,          // 7. max_output_tokens
                    input_mods_json,              // 8. input_modalities_json
                    output_mods_json,             // 9. output_modalities_json
                    d.model_type,                 // 10. model_type
                    d.family,                     // 11. family
                    caps_json,                    // 12. capabilities_json
                ])
                .map_err(|e| CoreError::Database {
                    message: format!("execute upsert_many: {}", e),
                    source: Some(Box::new(e)),
                })?;
            total += changed;
        }
    }

    tx.commit().map_err(|e| CoreError::Database {
        message: format!("commit upsert_many: {}", e),
        source: Some(Box::new(e)),
    })?;

    Ok(total)
}

/// List all non-expired, non-disabled models for a given provider.
///
/// A row is considered active when both:
/// - `active = 1` (the soft-disable bit set by [`set_active`]), and
/// - `expires_at IS NULL` or `expires_at > datetime('now')` (UTC).
///
/// An admin can flip a model out of routing without deleting the row by
/// calling [`set_active`] with `false`; the row stays in the table for
/// audit / re-enable.
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
               AND active = 1 \
               AND (expires_at IS NULL OR expires_at > datetime('now'))",
        )
        .map_err(|e| CoreError::Database {
            message: format!("prepare list_active: {}", e),
            source: Some(Box::new(e)),
        })?;

    let rows = stmt
        .query_map([provider.as_str()], map_row)
        .map_err(|e| CoreError::Database {
            message: format!("query list_active: {}", e),
            source: Some(Box::new(e)),
        })?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| CoreError::Database {
            message: format!("row list_active: {}", e),
            source: Some(Box::new(e)),
        })?);
    }
    Ok(out)
}

/// List all active (non-disabled, non-expired) models across every provider.
///
/// A row is considered active when both:
/// - `active = 1` (the soft-disable bit set by [`set_active`]), and
/// - `expires_at IS NULL` or `expires_at > datetime('now')` (UTC).
///
/// This is the cross-provider variant of [`list_active`]. The public
/// `GET /v1/models` endpoint consumes it so SDKs/CLIs pointing at
/// openproxy never see rows the admin has disabled (which is the
/// whole point of having a soft-disable bit at all). The single-
/// provider [`list_active`] is still used by the routing pipeline,
/// which only cares about the models it can send to a specific
/// upstream.
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
             WHERE active = 1 \
               AND (expires_at IS NULL OR expires_at > datetime('now'))",
        )
        .map_err(|e| CoreError::Database {
            message: format!("prepare list_active_all: {}", e),
            source: Some(Box::new(e)),
        })?;

    let rows = stmt
        .query_map([], map_row)
        .map_err(|e| CoreError::Database {
            message: format!("query list_active_all: {}", e),
            source: Some(Box::new(e)),
        })?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| CoreError::Database {
            message: format!("row list_active_all: {}", e),
            source: Some(Box::new(e)),
        })?);
    }
    Ok(out)
}

/// List every row in `models`. Used by the `/v1/admin/models` admin
/// endpoint which surfaces both live and recently-expired entries (the
/// `active` field tells the UI which ones are currently routable).
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
        .map_err(|e| CoreError::Database {
            message: format!("prepare list_all: {}", e),
            source: Some(Box::new(e)),
        })?;

    let rows = stmt.query_map([], map_row).map_err(|e| CoreError::Database {
        message: format!("query list_all: {}", e),
        source: Some(Box::new(e)),
    })?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| CoreError::Database {
            message: format!("row list_all: {}", e),
            source: Some(Box::new(e)),
        })?);
    }
    Ok(out)
}

/// Delete all expired models. Returns the number of rows removed.
///
/// "Expired" means `expires_at` is set and strictly less than `datetime('now')`.
/// This is intended to be called periodically (e.g. by a sweep job) — leaving
/// expired rows around is harmless; they simply don't show up in [`list_active`].
pub fn mark_expired(conn: &Connection) -> Result<usize> {
    let n = conn
        .execute(
            "DELETE FROM models \
             WHERE expires_at IS NOT NULL AND expires_at < datetime('now')",
            [],
        )
        .map_err(|e| CoreError::Database {
            message: format!("execute mark_expired: {}", e),
            source: Some(Box::new(e)),
        })?;
    Ok(n)
}

/// Set the soft-disable flag on a model. Returns `Ok(())` whether or not a
/// row was affected — toggling a non-existent id is a no-op, mirroring the
/// idempotent style of the other admin mutations.
///
/// `active = true` makes the row visible to [`list_active`]; `false` hides
/// it from routing. The row is preserved either way, so a future
/// re-enable doesn't lose any data.
pub fn set_active(conn: &Connection, id: ModelRowId, active: bool) -> Result<()> {
    let bit = if active { 1i64 } else { 0i64 };
    conn.execute(
        "UPDATE models SET active = ?1 WHERE id = ?2",
        params![bit, id.0],
    )
    .map_err(|e| CoreError::Database {
        message: format!("update active for model {}: {}", id.0, e),
        source: Some(Box::new(e)),
    })?;
    Ok(())
}

/// Bulk set active for all non-custom models of a provider.
/// Returns the number of rows updated.
///
/// Honors `custom = 1` rows (does NOT touch them), matching the
/// behavior of `apply_auto_activation`.
pub fn set_active_bulk(
    conn: &Connection,
    provider: &ProviderId,
    active: bool,
) -> Result<u64> {
    let bit = if active { 1i64 } else { 0i64 };
    let n = conn
        .execute(
            "UPDATE models SET active = ?1 WHERE provider_id = ?2 AND custom = 0",
            params![bit, provider.as_str()],
        )
        .map_err(|e| CoreError::Database {
            message: format!("set_active_bulk for {}: {}", provider, e),
            source: Some(Box::new(e)),
        })?;
    Ok(n as u64)
}

/// Fetch a single model by its primary key. Returns `None` if not found.
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
        .map_err(|e| CoreError::Database {
            message: format!("query get_by_row_id: {}", e),
            source: Some(Box::new(e)),
        })?;
    Ok(res)
}

/// Find an active model by its (exact, case-sensitive) `model_id`.
///
/// "Active" means `active = 1` AND not expired. The match is exact
/// because upstream model ids are looked up verbatim by adapters and
/// downstream tools; a fuzzy/prefix match here would silently alias
/// models that share a namespace and lead to surprising routing
/// decisions.
///
/// The function returns at most one row. If two providers happened to
/// both register the same `model_id` (the schema's `UNIQUE` constraint
/// is per-`(provider_id, model_id)`, so this is allowed), the
/// tie-breaker is `id ASC` — a deterministic but otherwise arbitrary
/// choice. The caller is expected to treat the result as
/// "one candidate" and decide via the routing layer which provider to
/// use.
///
/// Used by the model-first routing layer in
/// [`crate::routing::resolve`]: a chat request whose `model` field
/// matches a row in the `models` table is dispatched to that provider.
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
               AND (expires_at IS NULL OR expires_at > datetime('now')) \
             ORDER BY id ASC \
             LIMIT 1",
        )
        .map_err(|e| CoreError::Database {
            message: format!("prepare find_active_by_name: {}", e),
            source: Some(Box::new(e)),
        })?;
    let mut rows = stmt
        .query_map([model_id], map_row)
        .map_err(|e| CoreError::Database {
            message: format!("query find_active_by_name: {}", e),
            source: Some(Box::new(e)),
        })?;
    match rows.next() {
        Some(row) => Ok(Some(row.map_err(|e| CoreError::Database {
            message: format!("read find_active_by_name row: {}", e),
            source: Some(Box::new(e)),
        })?)),
        None => Ok(None),
    }
}

/// Find a single active model by provider id and model name.
///
/// Like [`find_active_by_name`] but scoped to a specific provider.
/// Used by the routing layer when the client sends a
/// `<provider>/<model_id>` prefix — we look for the model under that
/// exact provider rather than picking whichever was seeded first.
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
               AND (expires_at IS NULL OR expires_at > datetime('now')) \
             ORDER BY id ASC \
             LIMIT 1",
        )
        .map_err(|e| CoreError::Database {
            message: format!("prepare find_active_by_provider_and_name: {}", e),
            source: Some(Box::new(e)),
        })?;
    let mut rows = stmt
        .query_map(rusqlite::params![provider_id.as_str(), model_id], map_row)
        .map_err(|e| CoreError::Database {
            message: format!("query find_active_by_provider_and_name: {}", e),
            source: Some(Box::new(e)),
        })?;
    match rows.next() {
        Some(row) => Ok(Some(row.map_err(|e| CoreError::Database {
            message: format!("read find_active_by_provider_and_name row: {}", e),
            source: Some(Box::new(e)),
        })?)),
        None => Ok(None),
    }
}

/// Stamp the test-status columns on a model row. `status` is the raw
/// HTTP status code from the most recent `POST /v1/admin/models/:id/test`
/// call; pass `0` to mean "request never reached the upstream"
/// (DNS / connect / TLS errors are folded into this single value).
///
/// The companion `last_test_at` column is set to `datetime('now')` in
/// the same statement so the two columns always move together.
pub fn set_test_status(conn: &Connection, id: ModelRowId, status: i32) -> Result<()> {
    conn.execute(
        "UPDATE models \
         SET last_test_status = ?1, last_test_at = datetime('now') \
         WHERE id = ?2",
        params![status, id.0],
    )
    .map_err(|e| CoreError::Database {
        message: format!("update test status for model {}: {}", id.0, e),
        source: Some(Box::new(e)),
    })?;
    Ok(())
}

/// Hard-delete a model row. References in `combo_targets` are removed
/// first to satisfy the FK constraint (the schema declares the FK with
/// `ON DELETE CASCADE` for `provider_id` but the combo-target FK targets
/// `model_row_id` and is not cascading — see the migrations).
///
/// A missing id is a silent no-op (0 rows affected), matching the
/// idempotent style of the other admin deletions.
///
/// Returns the number of rows removed from `models`.
pub fn delete(conn: &Connection, id: ModelRowId) -> Result<u64> {
    let tx = conn.unchecked_transaction().map_err(|e| CoreError::Database {
        message: format!("begin delete model tx: {}", e),
        source: Some(Box::new(e)),
    })?;

    // combo_targets.model_row_id has no ON DELETE CASCADE in the
    // current schema; clean those rows up first so the FK check on
    // the model delete doesn't fire. We do it explicitly rather than
    // relying on cascade so the SQL stays self-documenting at the
    // call site.
    tx.execute(
        "DELETE FROM combo_targets WHERE model_row_id = ?1",
        params![id.0],
    )
    .map_err(|e| CoreError::Database {
        message: format!("delete combo_targets for model {}: {}", id.0, e),
        source: Some(Box::new(e)),
    })?;

    let removed = tx
        .execute("DELETE FROM models WHERE id = ?1", params![id.0])
        .map_err(|e| CoreError::Database {
            message: format!("delete model {}: {}", id.0, e),
            source: Some(Box::new(e)),
        })?;

    tx.commit().map_err(|e| CoreError::Database {
        message: format!("commit delete model tx: {}", e),
        source: Some(Box::new(e)),
    })?;

    Ok(removed as u64)
}

/// Insert a hand-crafted model row (or refresh an existing one).
///
/// Distinct from the adapter-driven [`upsert_many`] path: this sets
/// `custom = 1` and `active = 1` unconditionally so the operator's
/// intent ("this is a real model I want to call") is preserved across
/// refreshes that may not list the same id from the upstream.
///
/// `ttl_seconds` is the cache lifetime of the row. `0` means "never
/// expire" (sets `expires_at = NULL`); any other value is interpreted
/// as a delta from `datetime('now')`.
///
/// On conflict on `(provider_id, model_id)` the existing row is
/// preserved (its `row_id` is returned) and only the mutable fields
/// are refreshed; `custom` is forced back to `1` to defend against a
/// previously-discovered row being later "promoted" to custom without
/// the operator's consent — they asked for a custom row, so it stays
/// custom.
pub fn create_custom(
    conn: &Connection,
    provider_id: &ProviderId,
    model_id: &ModelId,
    display_name: Option<&str>,
    target_format: TargetFormat,
    ttl_seconds: i64,
) -> Result<ModelRowId> {
    // The `expires_at` value depends on `ttl_seconds`. We compose it
    // as a tiny SQL fragment because the same expression needs to
    // appear in both the INSERT VALUES list (a raw expression) and
    // the ON CONFLICT DO UPDATE clause (an `= <expression>` pair).
    let expires_expr = if ttl_seconds <= 0 {
        "NULL".to_string()
    } else {
        format!(
            "datetime('now', '+' || {} || ' seconds')",
            ttl_seconds
        )
    };

    // The `RETURNING id` clause gives us the rowid regardless of
    // whether the row was inserted or updated, so the caller can
    // chain a `set_test_status` or future operation off of it.
    let sql = format!(
        "INSERT INTO models \
            (provider_id, model_id, display_name, target_format, \
             discovered_at, expires_at, active, custom) \
         VALUES (?1, ?2, ?3, ?4, datetime('now'), {expires_expr}, 1, 1) \
         ON CONFLICT(provider_id, model_id) DO UPDATE SET \
            display_name = excluded.display_name, \
            target_format = excluded.target_format, \
            discovered_at = datetime('now'), \
            expires_at = {expires_expr}, \
            active = 1, \
            custom = 1 \
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
            ],
            |r| r.get(0),
        )
        .map_err(|e| {
            // FK violation → unknown provider. Same idiom as
            // `accounts::create` keeps error messages consistent.
            let msg = e.to_string();
            if msg.contains("FOREIGN KEY") {
                CoreError::Validation(format!(
                    "provider_id does not exist: {}",
                    provider_id
                ))
            } else {
                CoreError::Database {
                    message: format!("create_custom model for {}: {}", provider_id, e),
                    source: Some(Box::new(e)),
                }
            }
        })?;

    Ok(ModelRowId(row_id))
}

/// Recompute `active` for **newly discovered** non-custom models of a
/// provider, based on whether the model's `model_id` contains `keyword`.
///
/// "Newly discovered" means `discovered_at >= datetime('now', '-60 seconds')`
/// — i.e. rows that were **inserted** (not just refreshed) in the
/// current discovery cycle. Pre-existing rows whose `active` bit the
/// operator toggled by hand are preserved across refreshes because
/// [`upsert_many`] does not touch `discovered_at` on re-upsert.
///
/// - `Some(k)` — `active = 1` if `model_id LIKE '%k%'`, `0` otherwise.
/// - `None`    — `active = 1` for every newly discovered non-custom row.
///
/// Custom rows (`custom = 1`) are skipped entirely so the operator's
/// hand-picked entries survive a refresh. Returns the number of rows
/// whose `active` bit was changed.
pub fn apply_auto_activation(
    conn: &Connection,
    provider: &ProviderId,
    keyword: Option<&str>,
) -> Result<u64> {
    // The 60-second window is the key bit. The refresh flow is:
    //   1. `upsert_many` inserts new rows with `discovered_at = now`
    //      and preserves `discovered_at` on already-present rows.
    //   2. `apply_auto_activation` runs immediately after, in the same
    //      request handler.
    // So any row whose `discovered_at` is within the last 60s was a
    // *new insert* by this refresh. Rows older than that were already
    // present before the refresh and must keep their `active` bit.
    let updated = match keyword {
        Some(k) => conn.execute(
            "UPDATE models \
             SET active = CASE WHEN model_id LIKE '%' || ?1 || '%' THEN 1 ELSE 0 END \
             WHERE provider_id = ?2 \
               AND custom = 0 \
               AND discovered_at >= datetime('now', '-60 seconds')",
            params![k, provider.as_str()],
        ),
        None => conn.execute(
            "UPDATE models SET active = 1 \
             WHERE provider_id = ?1 \
               AND custom = 0 \
               AND discovered_at >= datetime('now', '-60 seconds')",
            params![provider.as_str()],
        ),
    }
    .map_err(|e| CoreError::Database {
        message: format!("apply_auto_activation for {}: {}", provider, e),
        source: Some(Box::new(e)),
    })?;
    Ok(updated as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    /// Set up an in-memory DB with the same DDL the production migrations
    /// produce for the `models` table (plus a row in `providers` to satisfy
    /// the FK — we only need the parent row to exist).
    fn fresh_db() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory");
        conn.execute_batch(
            "CREATE TABLE providers (
                 id            TEXT PRIMARY KEY,
                 display_name  TEXT NOT NULL,
                 base_url      TEXT NOT NULL,
                 auth_kind     TEXT NOT NULL,
                 health_status TEXT NOT NULL DEFAULT 'healthy',
                 created_at    TEXT NOT NULL DEFAULT (datetime('now')),
                 CHECK (health_status IN ('healthy', 'degraded', 'unhealthy'))
             );
             CREATE TABLE models (
                 id                     INTEGER PRIMARY KEY AUTOINCREMENT,
                 provider_id            TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
                 model_id               TEXT NOT NULL,
                 display_name           TEXT,
                 target_format          TEXT NOT NULL,
                 discovered_at          TEXT NOT NULL DEFAULT (datetime('now')),
                 expires_at             TEXT,
                 timeout_overrides_json TEXT,
                 active                 INTEGER NOT NULL DEFAULT 1
                                          CHECK (active IN (0, 1)),
                 last_test_status       INTEGER,
                 last_test_at           TEXT,
                 custom                 INTEGER NOT NULL DEFAULT 0
                                          CHECK (custom IN (0, 1)),
                 context_length         INTEGER,
                 max_output_tokens      INTEGER,
                 capabilities_json      TEXT,
                 family                 TEXT,
                 model_type             TEXT NOT NULL DEFAULT 'chat',
                 input_modalities_json  TEXT,
                 output_modalities_json TEXT,
                 UNIQUE(provider_id, model_id),
                 CHECK (target_format IN ('openai', 'anthropic', 'gemini'))
             );
             INSERT INTO providers (id, display_name, base_url, auth_kind)
             VALUES ('provA', 'Provider A', 'https://example.test', 'none');",
        )
        .expect("schema");
        conn
    }

    fn discovered(id: &str, fmt: TargetFormat) -> DiscoveredModel {
        DiscoveredModel {
            model_id: ModelId::new(id),
            display_name: Some(format!("Display {}", id)),
            target_format: fmt,
            context_length: None,
            max_output_tokens: None,
            input_modalities: None,
            output_modalities: None,
            model_type: None,
            family: None,
            capabilities: None,
        }
    }

    /// Minimal `DiscoveredModel` with all metadata fields unset, used
    /// as a base for struct-update syntax (`..minimal()`) in the
    /// metadata-persistence tests below.
    fn minimal(id: &str) -> DiscoveredModel {
        DiscoveredModel {
            model_id: ModelId::new(id),
            display_name: Some(id.to_string()),
            target_format: TargetFormat::Openai,
            context_length: None,
            max_output_tokens: None,
            input_modalities: None,
            output_modalities: None,
            model_type: None,
            family: None,
            capabilities: None,
        }
    }

    #[test]
    fn target_format_parse_roundtrip() {
        for (s, expected) in [
            ("openai", TargetFormat::Openai),
            ("anthropic", TargetFormat::Anthropic),
            ("gemini", TargetFormat::Gemini),
        ] {
            assert_eq!(TargetFormat::parse(s).unwrap(), expected);
            assert_eq!(expected.as_str(), s);
        }

        // Serde roundtrip: lowercase strings on the wire.
        let openai_json = serde_json::to_string(&TargetFormat::Openai).unwrap();
        assert_eq!(openai_json, "\"openai\"");
        let back: TargetFormat = serde_json::from_str(&openai_json).unwrap();
        assert_eq!(back, TargetFormat::Openai);

        // Invalid input is a Validation error, not a panic.
        let err = TargetFormat::parse("xml").unwrap_err();
        assert!(matches!(err, CoreError::Validation(_)), "got {:?}", err);
    }

    #[test]
    fn upsert_inserts_new() {
        let conn = fresh_db();
        let provider = ProviderId::new("provA");

        let n = upsert_many(
            &conn,
            &provider,
            &[discovered("m1", TargetFormat::Openai), discovered("m2", TargetFormat::Anthropic)],
            Duration::from_secs(3600),
        )
        .expect("upsert_many");

        // One row inserted per call, both fresh -> 2 changes reported.
        assert_eq!(n, 2);

        let all = list_all(&conn).expect("list_all");
        assert_eq!(all.len(), 2);
        let ids: Vec<&str> = all.iter().map(|m| m.model_id.as_str()).collect();
        assert!(ids.contains(&"m1"));
        assert!(ids.contains(&"m2"));

        let m1 = all.iter().find(|m| m.model_id.as_str() == "m1").unwrap();
        assert_eq!(m1.target_format, TargetFormat::Openai);
        assert!(m1.expires_at.is_some());
    }

    #[test]
    fn upsert_updates_existing() {
        let conn = fresh_db();
        let provider = ProviderId::new("provA");

        // First discovery: openai, name "old".
        upsert_many(
            &conn,
            &provider,
            &[DiscoveredModel {
                model_id: ModelId::new("m1"),
                display_name: Some("old".into()),
                target_format: TargetFormat::Openai,
                context_length: None,
                max_output_tokens: None,
                input_modalities: None,
                output_modalities: None,
                model_type: None,
                family: None,
                capabilities: None,
            }],
            Duration::from_secs(60),
        )
        .expect("first upsert");

        let original = list_all(&conn).unwrap();
        assert_eq!(original.len(), 1);
        let original_row_id = original[0].row_id;
        let original_discovered = original[0].discovered_at.clone();
        let original_expires = original[0].expires_at.clone();

        // Second discovery: same model, now anthropic + new name.
        let n = upsert_many(
            &conn,
            &provider,
            &[DiscoveredModel {
                model_id: ModelId::new("m1"),
                display_name: Some("new".into()),
                target_format: TargetFormat::Anthropic,
                context_length: None,
                max_output_tokens: None,
                input_modalities: None,
                output_modalities: None,
                model_type: None,
                family: None,
                capabilities: None,
            }],
            Duration::from_secs(7200),
        )
        .expect("second upsert");
        assert_eq!(n, 1, "update should report 1 changed row");

        let all = list_all(&conn).unwrap();
        assert_eq!(all.len(), 1, "no new row, just update");
        let m = &all[0];
        assert_eq!(m.row_id, original_row_id, "row id stable across update");
        assert_eq!(m.target_format, TargetFormat::Anthropic);
        assert_eq!(m.display_name.as_deref(), Some("new"));

        // discovered_at + expires_at are preserved on re-upsert: the
        // recency window used by `apply_auto_activation` only catches
        // truly-new rows, and a hand-toggled `active` bit must survive
        // subsequent provider refreshes. A re-upsert is a metadata
        // refresh, not a re-discovery.
        assert_eq!(
            m.discovered_at, original_discovered,
            "discovered_at must be preserved on re-upsert"
        );
        assert_eq!(
            m.expires_at, original_expires,
            "expires_at must be preserved on re-upsert"
        );
    }

    /// Regression test for the bug where the OpenRouter adapter
    /// dropped ~18 fields from the upstream `/models` response and
    /// every model ended up with default metadata. The fix expands
    /// `DiscoveredModel` to carry context_length, max_output_tokens,
    /// modalities, model_type, family, and capabilities_json through
    /// to the SQL `INSERT` / `UPDATE`. This test pins down the
    /// happy-path: all metadata fields land in the row.
    #[test]
    fn upsert_persists_openrouter_metadata() {
        use crate::capabilities::ModelCapabilities;

        let conn = fresh_db();
        let provider = ProviderId::new("provA");

        let models = vec![DiscoveredModel {
            model_id: ModelId::new("qwen/qwen3-coder:free"),
            display_name: Some("Qwen: Qwen3 Coder 480B A35B (free)".into()),
            target_format: TargetFormat::Openai,
            context_length: Some(1_048_576),
            max_output_tokens: Some(262_000),
            input_modalities: Some(vec!["text".into()]),
            output_modalities: Some(vec!["text".into()]),
            model_type: Some("chat".into()),
            family: Some("Qwen3".into()),
            capabilities: Some(ModelCapabilities {
                tool_calling: Some(true),
                temperature: Some(true),
                structured_output: Some(true),
                ..ModelCapabilities::empty()
            }),
        }];

        upsert_many(&conn, &provider, &models, Duration::from_secs(3600)).unwrap();

        let row = list_all(&conn).unwrap().pop().unwrap();
        assert_eq!(row.context_length, Some(1_048_576));
        assert_eq!(row.max_output_tokens, Some(262_000));
        assert_eq!(row.family.as_deref(), Some("Qwen3"));
        assert_eq!(row.model_type, "chat");
        // input/output modalities land as JSON arrays; parsing round-trips.
        let input_mods: Vec<String> =
            serde_json::from_str(row.input_modalities_json.as_deref().unwrap()).unwrap();
        assert_eq!(input_mods, vec!["text"]);
        let output_mods: Vec<String> =
            serde_json::from_str(row.output_modalities_json.as_deref().unwrap()).unwrap();
        assert_eq!(output_mods, vec!["text"]);
        // Capabilities JSON contains the bits we set, and `vision`
        // is omitted (None) thanks to `skip_serializing_if`.
        let caps_json = row.capabilities_json.as_deref().unwrap();
        assert!(caps_json.contains("\"tool_calling\":true"));
        assert!(caps_json.contains("\"temperature\":true"));
        assert!(caps_json.contains("\"structured_output\":true"));
        assert!(!caps_json.contains("vision"));
    }

    /// A re-upsert that brings fresh metadata must overwrite the
    /// previous values (this is what makes the OpenRouter refresh
    /// path correct when the upstream changes a model's
    /// `context_length`, for example).
    #[test]
    fn upsert_refreshes_metadata_on_re_upsert() {
        let conn = fresh_db();
        let provider = ProviderId::new("provA");

        // First insert: context_length = 128_000.
        upsert_many(
            &conn,
            &provider,
            &[DiscoveredModel {
                context_length: Some(128_000),
                max_output_tokens: Some(4_096),
                ..minimal("test/model")
            }],
            Duration::from_secs(3600),
        )
        .unwrap();

        // Re-upsert: context_length = 200_000 (upstream change).
        upsert_many(
            &conn,
            &provider,
            &[DiscoveredModel {
                context_length: Some(200_000),
                max_output_tokens: Some(8_192),
                ..minimal("test/model")
            }],
            Duration::from_secs(3600),
        )
        .unwrap();

        let row = list_all(&conn).unwrap().pop().unwrap();
        assert_eq!(row.context_length, Some(200_000), "context_length refreshed");
        assert_eq!(
            row.max_output_tokens,
            Some(8_192),
            "max_output_tokens refreshed"
        );
    }

    /// COALESCE behavior: if the upstream sends `None` for a field on
    /// re-upsert, the existing value is preserved. This matches the
    /// backfill-style fallback the public endpoint relies on.
    #[test]
    fn upsert_preserves_metadata_when_excluded_is_null() {
        let conn = fresh_db();
        let provider = ProviderId::new("provA");

        // First insert: context_length = 128_000.
        upsert_many(
            &conn,
            &provider,
            &[DiscoveredModel {
                context_length: Some(128_000),
                ..minimal("test/model")
            }],
            Duration::from_secs(3600),
        )
        .unwrap();

        // Re-upsert: context_length = None. The COALESCE in the ON
        // CONFLICT clause keeps the previous value.
        upsert_many(
            &conn,
            &provider,
            &[DiscoveredModel {
                context_length: None,
                ..minimal("test/model")
            }],
            Duration::from_secs(3600),
        )
        .unwrap();

        let row = list_all(&conn).unwrap().pop().unwrap();
        assert_eq!(
            row.context_length,
            Some(128_000),
            "context_length must be preserved when upstream sends None"
        );
    }

    /// Re-upserting a model must not bump `discovered_at` or `expires_at`,
    /// even when the metadata (`display_name`, `target_format`) does
    /// change. This is the regression test for the bug where a hand-
    /// disabled model that the provider keeps listing was being
    /// re-classified as "new" on every refresh and had its `active`
    /// bit clobbered by `apply_auto_activation`.
    #[test]
    fn upsert_preserves_discovered_at_on_re_upsert() {
        let conn = fresh_db();
        let provider = ProviderId::new("provA");

        // Initial insert.
        let n = upsert_many(
            &conn,
            &provider,
            &[DiscoveredModel {
                model_id: ModelId::new("anthropic/claude-sonnet-4"),
                display_name: Some("Claude Sonnet 4".into()),
                target_format: TargetFormat::Openai,
                context_length: None,
                max_output_tokens: None,
                input_modalities: None,
                output_modalities: None,
                model_type: None,
                family: None,
                capabilities: None,
            }],
            Duration::from_secs(3600),
        )
        .expect("first upsert");
        assert_eq!(n, 1);

        // Capture the original timestamps.
        let original = list_all(&conn).unwrap().pop().unwrap();
        let original_discovered = original.discovered_at.clone();
        let original_expires = original.expires_at.clone();

        // Sleep just long enough for `datetime('now')` to tick to a
        // different value (sqlite's datetime() has 1-second
        // resolution).
        std::thread::sleep(std::time::Duration::from_millis(1100));

        // Re-upsert with a different display_name.
        let n2 = upsert_many(
            &conn,
            &provider,
            &[DiscoveredModel {
                model_id: ModelId::new("anthropic/claude-sonnet-4"),
                display_name: Some("Claude Sonnet 4 (renamed)".into()),
                target_format: TargetFormat::Openai,
                context_length: None,
                max_output_tokens: None,
                input_modalities: None,
                output_modalities: None,
                model_type: None,
                family: None,
                capabilities: None,
            }],
            Duration::from_secs(3600),
        )
        .expect("second upsert");
        assert_eq!(n2, 1, "re-upsert should report 1 changed row");

        let updated = list_all(&conn).unwrap().pop().unwrap();
        assert_eq!(
            updated.discovered_at, original_discovered,
            "discovered_at must not change on re-upsert (got {} vs {})",
            updated.discovered_at, original_discovered,
        );
        assert_eq!(
            updated.expires_at, original_expires,
            "expires_at must not change on re-upsert (got {:?} vs {:?})",
            updated.expires_at, original_expires,
        );
        assert_eq!(
            updated.display_name.as_deref(),
            Some("Claude Sonnet 4 (renamed)"),
            "display_name should be refreshed"
        );
    }

    /// End-to-end: a model the user disabled by hand, after the
    /// provider lists it again on a refresh, must stay disabled. The
    /// chain that defends this is:
    ///   1. `upsert_many` does NOT bump `discovered_at` on re-upsert.
    ///   2. `apply_auto_activation` only touches rows within the
    ///      60-second recency window, so a row that was discovered
    ///      > 60s ago is left alone and keeps its `active` bit.
    #[test]
    fn apply_auto_activation_does_not_affect_old_re_upserted_model() {
        let conn = fresh_db();
        let provider = ProviderId::new("provA");

        // Initial discovery of the model.
        let n = upsert_many(
            &conn,
            &provider,
            &[DiscoveredModel {
                model_id: ModelId::new("anthropic/claude-sonnet-4"),
                display_name: None,
                target_format: TargetFormat::Openai,
                context_length: None,
                max_output_tokens: None,
                input_modalities: None,
                output_modalities: None,
                model_type: None,
                family: None,
                capabilities: None,
            }],
            Duration::from_secs(3600),
        )
        .expect("first upsert");
        assert_eq!(n, 1);

        // Operator hand-toggles the model OFF.
        let m = list_all(&conn).unwrap().pop().unwrap();
        set_active(&conn, m.row_id, false).expect("set_active false");
        let m = list_all(&conn).unwrap().pop().unwrap();
        assert!(!m.active, "pre-condition: model is hand-disabled");

        // Move discovered_at into the past so the next refresh's
        // recency window will exclude this row. We don't actually
        // wait 60s — that would make the test slow and flaky.
        conn.execute(
            "UPDATE models SET discovered_at = datetime('now', '-2 minutes') WHERE id = ?1",
            [m.row_id.0],
        )
        .expect("backdate discovered_at");
        let m_after_backdate = list_all(&conn).unwrap().pop().unwrap();
        let pre_upsert_discovered = m_after_backdate.discovered_at.clone();
        assert!(
            pre_upsert_discovered
                != conn
                    .query_row("SELECT datetime('now')", [], |r| r.get::<_, String>(0))
                    .unwrap(),
            "pre-condition: backdate must move discovered_at into the past",
        );

        // Refresh: the provider lists the same model again.
        let n2 = upsert_many(
            &conn,
            &provider,
            &[DiscoveredModel {
                model_id: ModelId::new("anthropic/claude-sonnet-4"),
                display_name: None,
                target_format: TargetFormat::Openai,
                context_length: None,
                max_output_tokens: None,
                input_modalities: None,
                output_modalities: None,
                model_type: None,
                family: None,
                capabilities: None,
            }],
            Duration::from_secs(3600),
        )
        .expect("refresh upsert");
        assert_eq!(n2, 1, "refresh should report 1 changed row");

        // discovered_at must still be the backdated value — the
        // refresh didn't bump it back to "now".
        let m = list_all(&conn).unwrap().pop().unwrap();
        assert_eq!(
            m.discovered_at, pre_upsert_discovered,
            "discovered_at should still be the backdated value after refresh: \
             got {}, expected {}",
            m.discovered_at, pre_upsert_discovered,
        );

        // apply_auto_activation with a keyword that would normally
        // activate the model must NOT touch it, because the
        // discovered_at is outside the 60s window.
        let updated =
            apply_auto_activation(&conn, &provider, Some("claude")).expect("apply keyword");
        assert_eq!(
            updated, 0,
            "should affect 0 rows because the model is not new"
        );

        // ...and the same with no keyword (would otherwise enable
        // every "new" non-custom row).
        let updated = apply_auto_activation(&conn, &provider, None).expect("apply none");
        assert_eq!(updated, 0, "should affect 0 rows because the model is not new");

        // active is still the operator's hand-set value.
        let m = list_all(&conn).unwrap().pop().unwrap();
        assert!(
            !m.active,
            "active must remain false after a refresh + auto-activation cycle"
        );
    }

    #[test]
    fn list_active_excludes_expired() {
        let conn = fresh_db();
        let provider = ProviderId::new("provA");

        // Manually insert one already-expired row and one with a long TTL.
        conn.execute(
            "INSERT INTO models (provider_id, model_id, display_name, target_format, expires_at) \
             VALUES ('provA', 'live', 'Live Model', 'openai', datetime('now', '+1 hour'))",
            [],
        )
        .expect("insert live");
        conn.execute(
            "INSERT INTO models (provider_id, model_id, display_name, target_format, expires_at) \
             VALUES ('provA', 'stale', 'Stale Model', 'openai', datetime('now', '-1 hour'))",
            [],
        )
        .expect("insert stale");
        conn.execute(
            "INSERT INTO models (provider_id, model_id, display_name, target_format, expires_at) \
             VALUES ('provA', 'null_expiry', 'Null Expiry', 'openai', NULL)",
            [],
        )
        .expect("insert null_expiry");

        let active = list_active(&conn, &provider).expect("list_active");
        let ids: Vec<&str> = active.iter().map(|m| m.model_id.as_str()).collect();
        assert!(ids.contains(&"live"));
        assert!(ids.contains(&"null_expiry"), "NULL expires_at counts as active");
        assert!(!ids.contains(&"stale"), "past expires_at excluded");
        assert_eq!(active.len(), 2);

        // list_all sees all three.
        assert_eq!(list_all(&conn).unwrap().len(), 3);
    }

    /// Cross-provider variant of the list_active contract. The public
    /// `GET /v1/models` endpoint feeds off this and must never leak
    /// disabled rows; this test guards the filter so a future refactor
    /// can't quietly regress to `list_all`.
    #[test]
    fn list_active_all_excludes_disabled_and_expired_across_providers() {
        let conn = fresh_db();
        // Add a second provider so we can prove the query doesn't
        // implicitly scope to a single provider.
        conn.execute(
            "INSERT INTO providers (id, display_name, base_url, auth_kind) \
             VALUES ('provB', 'Provider B', 'https://b.test', 'none')",
            [],
        )
        .expect("insert provB");

        // provA: one live + one disabled.
        conn.execute(
            "INSERT INTO models (provider_id, model_id, display_name, target_format, expires_at, active) \
             VALUES ('provA', 'a-live', 'A live', 'openai', datetime('now', '+1 hour'), 1)",
            [],
        )
        .expect("a-live");
        conn.execute(
            "INSERT INTO models (provider_id, model_id, display_name, target_format, expires_at, active) \
             VALUES ('provA', 'a-off', 'A off', 'openai', datetime('now', '+1 hour'), 0)",
            [],
        )
        .expect("a-off");

        // provB: one live + one expired.
        conn.execute(
            "INSERT INTO models (provider_id, model_id, display_name, target_format, expires_at, active) \
             VALUES ('provB', 'b-live', 'B live', 'openai', datetime('now', '+1 hour'), 1)",
            [],
        )
        .expect("b-live");
        conn.execute(
            "INSERT INTO models (provider_id, model_id, display_name, target_format, expires_at, active) \
             VALUES ('provB', 'b-stale', 'B stale', 'openai', datetime('now', '-1 hour'), 1)",
            [],
        )
        .expect("b-stale");

        // Sanity: list_all sees all four rows.
        assert_eq!(list_all(&conn).unwrap().len(), 4);

        // list_active_all returns only the two live-and-active rows,
        // spanning both providers.
        let active = list_active_all(&conn).expect("list_active_all");
        let ids: Vec<&str> = active.iter().map(|m| m.model_id.as_str()).collect();
        assert!(ids.contains(&"a-live"), "provA live row included");
        assert!(ids.contains(&"b-live"), "provB live row included");
        assert!(!ids.contains(&"a-off"), "soft-disabled row excluded");
        assert!(!ids.contains(&"b-stale"), "expired row excluded");
        assert_eq!(active.len(), 2);
    }

    #[test]
    fn mark_expired_deletes_old() {
        let conn = fresh_db();

        conn.execute(
            "INSERT INTO models (provider_id, model_id, display_name, target_format, expires_at) \
             VALUES ('provA', 'live', 'L', 'openai', datetime('now', '+1 hour'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO models (provider_id, model_id, display_name, target_format, expires_at) \
             VALUES ('provA', 'old1', 'O1', 'openai', datetime('now', '-1 hour'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO models (provider_id, model_id, display_name, target_format, expires_at) \
             VALUES ('provA', 'old2', 'O2', 'openai', datetime('now', '-10 minutes'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO models (provider_id, model_id, display_name, target_format, expires_at) \
             VALUES ('provA', 'forever', 'F', 'openai', NULL)",
            [],
        )
        .unwrap();

        let n = mark_expired(&conn).expect("mark_expired");
        assert_eq!(n, 2, "only the two past-dated rows deleted");

        let remaining = list_all(&conn).unwrap();
        let ids: Vec<&str> = remaining.iter().map(|m| m.model_id.as_str()).collect();
        assert_eq!(remaining.len(), 2);
        assert!(ids.contains(&"live"));
        assert!(ids.contains(&"forever"), "NULL expires_at never deleted");
    }

    #[test]
    fn get_by_row_id_returns_some_and_none() {
        let conn = fresh_db();
        let provider = ProviderId::new("provA");
        upsert_many(
            &conn,
            &provider,
            &[discovered("m1", TargetFormat::Openai)],
            Duration::from_secs(60),
        )
        .expect("upsert");
        let stored = list_all(&conn).expect("list");
        let row_id = stored[0].row_id;

        let fetched = get_by_row_id(&conn, row_id).expect("get").expect("present");
        assert_eq!(fetched.row_id, row_id);
        assert_eq!(fetched.model_id.as_str(), "m1");

        let missing = get_by_row_id(&conn, ModelRowId(9_999_999)).expect("get missing");
        assert!(missing.is_none());
    }

    #[test]
    fn set_active_toggles_visibility() {
        let conn = fresh_db();
        let provider = ProviderId::new("provA");

        // Insert two active models directly so we don't depend on the upsert
        // path. Both default to active=1 (via the schema DEFAULT).
        conn.execute(
            "INSERT INTO models (provider_id, model_id, display_name, target_format, expires_at) \
             VALUES ('provA', 'm1', 'M1', 'openai', datetime('now', '+1 hour'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO models (provider_id, model_id, display_name, target_format, expires_at) \
             VALUES ('provA', 'm2', 'M2', 'openai', datetime('now', '+1 hour'))",
            [],
        )
        .unwrap();

        // Initially both are visible.
        let before = list_active(&conn, &provider).expect("list_active");
        assert_eq!(before.len(), 2, "both rows active by default");

        let m1_id = before.iter().find(|m| m.model_id.as_str() == "m1").unwrap().row_id;
        let m2_id = before.iter().find(|m| m.model_id.as_str() == "m2").unwrap().row_id;

        // Disable m1.
        set_active(&conn, m1_id, false).expect("disable m1");
        let after_disable = list_active(&conn, &provider).expect("list_active");
        assert_eq!(after_disable.len(), 1, "m1 hidden after set_active(false)");
        assert_eq!(after_disable[0].row_id, m2_id);
        // list_all still shows both — set_active is a soft toggle, not a delete.
        assert_eq!(list_all(&conn).unwrap().len(), 2);

        // Re-enable m1.
        set_active(&conn, m1_id, true).expect("enable m1");
        let after_enable = list_active(&conn, &provider).expect("list_active");
        assert_eq!(after_enable.len(), 2, "m1 visible again after set_active(true)");

        // Toggling a missing id is a silent no-op (no error, no panic).
        set_active(&conn, ModelRowId(424242), false).expect("toggle missing is a no-op");
    }

    #[test]
    fn set_active_bulk_updates_non_custom_only() {
        let conn = fresh_db();
        let provider = ProviderId::new("provA");

        // 3 non-custom rows that start active=1 (the schema default).
        for id in ["a", "b", "c"] {
            conn.execute(
                "INSERT INTO models (provider_id, model_id, display_name, target_format, expires_at) \
                 VALUES ('provA', ?1, 'D', 'openai', datetime('now', '+1 hour'))",
                params![id],
            )
            .unwrap();
        }
        // 1 custom row, also starts active=1 — must stay active regardless
        // of the bulk toggle.
        conn.execute(
            "INSERT INTO models (provider_id, model_id, display_name, target_format, expires_at, custom) \
             VALUES ('provA', 'z', 'Z', 'openai', datetime('now', '+1 hour'), 1)",
            [],
        )
        .unwrap();

        // Sanity: list_all sees all 4.
        assert_eq!(list_all(&conn).unwrap().len(), 4);

        // Bulk-disable.
        let updated =
            set_active_bulk(&conn, &provider, false).expect("set_active_bulk false");
        assert_eq!(updated, 3, "exactly 3 non-custom rows touched");

        // Non-custom rows now inactive; custom row untouched.
        for id in ["a", "b", "c"] {
            let m = list_all(&conn)
                .unwrap()
                .into_iter()
                .find(|m| m.model_id.as_str() == id)
                .expect("present");
            assert!(!m.active, "non-custom {} should be inactive", id);
        }
        let z = list_all(&conn)
            .unwrap()
            .into_iter()
            .find(|m| m.model_id.as_str() == "z")
            .expect("present");
        assert!(z.active, "custom row's active bit must NOT be touched");
        assert!(z.custom, "custom bit still set");

        // Bulk-enable brings the 3 non-custom rows back; the custom row
        // was already active and stays that way.
        let updated2 =
            set_active_bulk(&conn, &provider, true).expect("set_active_bulk true");
        assert_eq!(updated2, 3, "3 non-custom rows flipped back on");

        for id in ["a", "b", "c"] {
            let m = list_all(&conn)
                .unwrap()
                .into_iter()
                .find(|m| m.model_id.as_str() == id)
                .expect("present");
            assert!(m.active, "non-custom {} should be active again", id);
        }
        let z2 = list_all(&conn)
            .unwrap()
            .into_iter()
            .find(|m| m.model_id.as_str() == "z")
            .expect("present");
        assert!(z2.active, "custom row still active");
    }

    #[test]
    fn set_test_status_stamps_both_columns() {
        let conn = fresh_db();
        let provider = ProviderId::new("provA");
        upsert_many(
            &conn,
            &provider,
            &[discovered("m1", TargetFormat::Openai)],
            Duration::from_secs(60),
        )
        .expect("upsert");
        let row_id = list_all(&conn).unwrap()[0].row_id;

        // Pre-condition: the columns are NULL on a fresh row.
        let pre = get_by_row_id(&conn, row_id).unwrap().unwrap();
        assert_eq!(pre.last_test_status, None);
        assert_eq!(pre.last_test_at, None);

        // 200 OK from a real upstream.
        set_test_status(&conn, row_id, 200).expect("set 200");
        let post = get_by_row_id(&conn, row_id).unwrap().unwrap();
        assert_eq!(post.last_test_status, Some(200));
        assert!(post.last_test_at.is_some(), "last_test_at stamped");

        // 0 = network error.
        set_test_status(&conn, row_id, 0).expect("set 0");
        let post = get_by_row_id(&conn, row_id).unwrap().unwrap();
        assert_eq!(post.last_test_status, Some(0));

        // Toggling a missing id is a silent no-op.
        set_test_status(&conn, ModelRowId(123_456_789), 500).expect("missing is no-op");
    }

    #[test]
    fn create_custom_inserts_active_row() {
        let conn = fresh_db();
        let provider = ProviderId::new("provA");
        let row_id = create_custom(
            &conn,
            &provider,
            &ModelId::new("manual-model"),
            Some("My Hand-Crafted Model"),
            TargetFormat::Openai,
            3600,
        )
        .expect("create_custom");
        assert!(row_id.0 > 0);

        let m = get_by_row_id(&conn, row_id).unwrap().expect("present");
        assert!(m.custom, "custom bit set");
        assert!(m.active, "active bit set");
        assert_eq!(m.model_id.as_str(), "manual-model");
        assert_eq!(m.display_name.as_deref(), Some("My Hand-Crafted Model"));
        assert!(m.expires_at.is_some(), "ttl > 0 -> expires_at set");
        assert_eq!(m.last_test_status, None);
    }

    #[test]
    fn create_custom_with_ttl_zero_means_no_expiry() {
        let conn = fresh_db();
        let provider = ProviderId::new("provA");
        let row_id = create_custom(
            &conn,
            &provider,
            &ModelId::new("forever-model"),
            None,
            TargetFormat::Anthropic,
            0,
        )
        .expect("create_custom ttl=0");
        let m = get_by_row_id(&conn, row_id).unwrap().expect("present");
        assert!(m.expires_at.is_none(), "ttl=0 -> expires_at NULL");
    }

    #[test]
    fn create_custom_on_existing_row_upserts() {
        let conn = fresh_db();
        let provider = ProviderId::new("provA");
        // Pre-insert a non-custom, non-active row that we'll later
        // "promote" to custom via create_custom.
        conn.execute(
            "INSERT INTO models (provider_id, model_id, display_name, target_format, active, custom) \
             VALUES ('provA', 'shared-id', 'old', 'openai', 0, 0)",
            [],
        )
        .expect("seed non-custom row");
        let original_row_id = list_all(&conn).unwrap()[0].row_id;

        // Now create_custom for the same (provider, model_id) pair.
        let returned = create_custom(
            &conn,
            &provider,
            &ModelId::new("shared-id"),
            Some("new"),
            TargetFormat::Openai,
            60,
        )
        .expect("create_custom on existing");

        assert_eq!(
            returned, original_row_id,
            "upsert preserves the existing row's id"
        );
        let m = get_by_row_id(&conn, returned).unwrap().unwrap();
        assert!(m.custom, "custom bit forced to 1 on conflict");
        assert!(m.active, "active bit forced to 1 on conflict");
        assert_eq!(m.display_name.as_deref(), Some("new"));
    }

    #[test]
    fn create_custom_with_unknown_provider_fails_validation() {
        let conn = fresh_db();
        let err = create_custom(
            &conn,
            &ProviderId::new("does-not-exist"),
            &ModelId::new("m"),
            None,
            TargetFormat::Openai,
            60,
        )
        .expect_err("FK violation");
        assert!(
            matches!(err, CoreError::Validation(_)),
            "expected Validation, got {:?}",
            err
        );
    }

    #[test]
    fn apply_auto_activation_with_keyword_matches_substring() {
        let conn = fresh_db();
        let provider = ProviderId::new("provA");
        for id in ["claude-3", "claude-2", "gpt-4", "gemini-pro"] {
            upsert_many(
                &conn,
                &provider,
                &[discovered(id, TargetFormat::Openai)],
                Duration::from_secs(3600),
            )
            .expect("seed");
        }

        let updated = apply_auto_activation(&conn, &provider, Some("claude"))
            .expect("apply with keyword");
        assert!(updated >= 3, "all 4 rows touched (claude ones -> 1, gpt/gemini -> 0)");

        // claude-3 and claude-2 should now be active; the others inactive.
        let active = list_active(&conn, &provider).expect("list_active");
        let active_ids: Vec<&str> = active.iter().map(|m| m.model_id.as_str()).collect();
        assert!(active_ids.contains(&"claude-3"));
        assert!(active_ids.contains(&"claude-2"));
        assert!(!active_ids.contains(&"gpt-4"));
        assert!(!active_ids.contains(&"gemini-pro"));
    }

    #[test]
    fn apply_auto_activation_with_no_keyword_enables_all() {
        let conn = fresh_db();
        let provider = ProviderId::new("provA");
        for id in ["a", "b", "c"] {
            upsert_many(
                &conn,
                &provider,
                &[discovered(id, TargetFormat::Openai)],
                Duration::from_secs(3600),
            )
            .expect("seed");
        }
        // Start with all rows inactive to make the test meaningful.
        for m in list_all(&conn).unwrap() {
            set_active(&conn, m.row_id, false).expect("disable");
        }
        assert_eq!(list_active(&conn, &provider).unwrap().len(), 0);

        let updated = apply_auto_activation(&conn, &provider, None).expect("apply none");
        assert_eq!(updated, 3, "all three rows flipped back on");
        assert_eq!(list_active(&conn, &provider).unwrap().len(), 3);
    }

    #[test]
    fn apply_auto_activation_skips_custom_rows() {
        let conn = fresh_db();
        let provider = ProviderId::new("provA");
        // Two discovered + one custom. The custom one stays as-is
        // regardless of the keyword.
        for id in ["claude-3", "gpt-4"] {
            upsert_many(
                &conn,
                &provider,
                &[discovered(id, TargetFormat::Openai)],
                Duration::from_secs(3600),
            )
            .expect("seed");
        }
        // Pre-seed a custom row that is currently INACTIVE to prove
        // the auto-activation path leaves it alone.
        conn.execute(
            "INSERT INTO models (provider_id, model_id, target_format, custom, active) \
             VALUES ('provA', 'handpicked', 'openai', 1, 0)",
            [],
        )
        .expect("seed custom");

        apply_auto_activation(&conn, &provider, Some("claude")).expect("apply");

        let handpicked = list_all(&conn)
            .unwrap()
            .into_iter()
            .find(|m| m.model_id.as_str() == "handpicked")
            .expect("present");
        assert!(!handpicked.active, "custom row's active bit untouched");
        assert!(handpicked.custom, "custom bit still set");

        let claude = list_all(&conn)
            .unwrap()
            .into_iter()
            .find(|m| m.model_id.as_str() == "claude-3")
            .expect("present");
        assert!(claude.active, "matched keyword -> active");

        let gpt = list_all(&conn)
            .unwrap()
            .into_iter()
            .find(|m| m.model_id.as_str() == "gpt-4")
            .expect("present");
        assert!(!gpt.active, "unmatched keyword -> inactive");
    }

    /// `apply_auto_activation` is documented to only touch models discovered
    /// in the most recent refresh (last 60s). Pre-existing rows whose
    /// `active` bit the operator toggled by hand must be preserved across
    /// a refresh — this is the core invariant the user asked for.
    #[test]
    fn apply_auto_activation_skips_old_models() {
        let conn = fresh_db();
        let provider = ProviderId::new("provA");

        // Seed two rows that look "old" (2 minutes ago) and start them
        // explicitly INACTIVE — that's the operator's hand-toggle state we
        // want to defend. Then seed one "fresh" row that the upcoming
        // refresh is producing.
        for id in ["old-a", "old-b"] {
            conn.execute(
                "INSERT INTO models (provider_id, model_id, display_name, target_format, \
                                     discovered_at, active) \
                 VALUES ('provA', ?1, ?1, 'openai', datetime('now', '-2 minutes'), 0)",
                params![id],
            )
            .expect("seed old");
        }
        conn.execute(
            "INSERT INTO models (provider_id, model_id, display_name, target_format, \
                                 discovered_at, active) \
             VALUES ('provA', 'new-c', 'new-c', 'openai', datetime('now'), 0)",
            [],
        )
        .expect("seed new");

        let updated = apply_auto_activation(&conn, &provider, None).expect("apply none");
        assert_eq!(
            updated, 1,
            "only the freshly-discovered row should be touched"
        );

        // The two old rows still inactive (operator toggle preserved).
        for id in ["old-a", "old-b"] {
            let m = list_all(&conn)
                .unwrap()
                .into_iter()
                .find(|m| m.model_id.as_str() == id)
                .expect("present");
            assert!(!m.active, "old row {} must keep its hand-set active=0", id);
        }
        // The new row is now active.
        let new_c = list_all(&conn)
            .unwrap()
            .into_iter()
            .find(|m| m.model_id.as_str() == "new-c")
            .expect("present");
        assert!(new_c.active, "freshly discovered row -> active");
    }

    /// With a keyword, the `discovered_at` filter still applies: an old row
    /// whose `active` was disabled by hand must stay disabled, even if its
    /// `model_id` would otherwise match the keyword. A new row matching
    /// the keyword flips on.
    #[test]
    fn apply_auto_activation_with_keyword_only_affects_new() {
        let conn = fresh_db();
        let provider = ProviderId::new("provA");

        // Old row that matches "free" — but is INACTIVE (operator toggle).
        conn.execute(
            "INSERT INTO models (provider_id, model_id, display_name, target_format, \
                                 discovered_at, active) \
             VALUES ('provA', 'old-free', 'Old Free', 'openai', \
                     datetime('now', '-2 minutes'), 0)",
            [],
        )
        .expect("seed old free");
        // New row that also matches "free" — start INACTIVE so we can
        // verify the keyword flips it on.
        conn.execute(
            "INSERT INTO models (provider_id, model_id, display_name, target_format, \
                                 discovered_at, active) \
             VALUES ('provA', 'new-free', 'New Free', 'openai', \
                     datetime('now'), 0)",
            [],
        )
        .expect("seed new free");

        let updated =
            apply_auto_activation(&conn, &provider, Some("free")).expect("apply keyword");
        assert_eq!(updated, 1, "only the new row matches the recency filter");

        let old_free = list_all(&conn)
            .unwrap()
            .into_iter()
            .find(|m| m.model_id.as_str() == "old-free")
            .expect("present");
        assert!(
            !old_free.active,
            "old row keeps its hand-set active=0 even though it matches the keyword"
        );

        let new_free = list_all(&conn)
            .unwrap()
            .into_iter()
            .find(|m| m.model_id.as_str() == "new-free")
            .expect("present");
        assert!(new_free.active, "new row matching keyword -> active");
    }

    /// If no rows are within the 60s recency window, the call is a true
    /// no-op: 0 rows updated, no side effects.
    #[test]
    fn apply_auto_activation_no_new_models_is_noop() {
        let conn = fresh_db();
        let provider = ProviderId::new("provA");

        for id in ["x", "y", "z"] {
            conn.execute(
                "INSERT INTO models (provider_id, model_id, display_name, target_format, \
                                     discovered_at, active) \
                 VALUES ('provA', ?1, ?1, 'openai', datetime('now', '-5 minutes'), 0)",
                params![id],
            )
            .expect("seed");
        }

        let updated_none =
            apply_auto_activation(&conn, &provider, None).expect("apply none");
        assert_eq!(updated_none, 0, "no fresh rows -> 0 updates");

        let updated_kw =
            apply_auto_activation(&conn, &provider, Some("x")).expect("apply kw");
        assert_eq!(updated_kw, 0, "no fresh rows + keyword -> 0 updates");

        // Nothing flipped.
        for id in ["x", "y", "z"] {
            let m = list_all(&conn)
                .unwrap()
                .into_iter()
                .find(|m| m.model_id.as_str() == id)
                .expect("present");
            assert!(!m.active, "row {} stays inactive", id);
        }
    }

    #[test]
    fn delete_removes_model_and_combo_target_references() {
        let conn = fresh_db();
        let provider = ProviderId::new("provA");

        // Seed two models; the test will delete one and verify the
        // other survives plus a combo_target pointing at the deleted
        // model is wiped out by the same transaction.
        upsert_many(
            &conn,
            &provider,
            &[discovered("m1", TargetFormat::Openai), discovered("m2", TargetFormat::Anthropic)],
            Duration::from_secs(3600),
        )
        .expect("seed");
        let all = list_all(&conn).unwrap();
        let m1_id = all.iter().find(|m| m.model_id.as_str() == "m1").unwrap().row_id;
        let m2_id = all.iter().find(|m| m.model_id.as_str() == "m2").unwrap().row_id;

        // Seed a combo + a target pointing at m1. The schema requires
        // a combos table; mirror the FK columns we depend on with a
        // tiny DDL rather than re-running the full migration set.
        conn.execute_batch(
            "CREATE TABLE combos (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, \
                                   strategy TEXT NOT NULL, race_size INTEGER NOT NULL DEFAULT 1); \
             CREATE TABLE combo_targets (id INTEGER PRIMARY KEY AUTOINCREMENT, \
                                          combo_id INTEGER NOT NULL, provider_id TEXT NOT NULL, \
                                          account_id INTEGER, model_row_id INTEGER NOT NULL);",
        )
        .expect("combo tables");
        let combo_id: i64 = conn
            .execute(
                "INSERT INTO combos(name, strategy) VALUES ('c', 'priority')",
                [],
            )
            .expect("insert combo") as i64;
        conn.execute(
            "INSERT INTO combo_targets(combo_id, provider_id, model_row_id) \
             VALUES (?1, ?2, ?3)",
            rusqlite::params![combo_id, "provA", m1_id.0],
        )
        .expect("insert target");

        // Pre-condition: the target exists.
        let pre_targets: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM combo_targets WHERE model_row_id = ?1",
                [m1_id.0],
                |r| r.get(0),
            )
            .expect("count targets");
        assert_eq!(pre_targets, 1);

        // Delete m1.
        let removed = delete(&conn, m1_id).expect("delete m1");
        assert_eq!(removed, 1, "one row removed");

        // m1 is gone, m2 survives, and the combo_target was cleaned
        // up in the same transaction (no FK violation either way).
        assert!(get_by_row_id(&conn, m1_id).unwrap().is_none(), "m1 gone");
        assert!(get_by_row_id(&conn, m2_id).unwrap().is_some(), "m2 alive");
        let post_targets: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM combo_targets WHERE model_row_id = ?1",
                [m1_id.0],
                |r| r.get(0),
            )
            .expect("count targets post");
        assert_eq!(post_targets, 0, "combo_target pointing at m1 was wiped");

        // Idempotent: a second delete returns 0, not an error.
        let removed_again = delete(&conn, m1_id).expect("delete again");
        assert_eq!(removed_again, 0, "missing id is a no-op");
    }
}
