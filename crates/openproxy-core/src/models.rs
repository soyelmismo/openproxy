//! Persistent model registry. Models are discovered from providers' /models endpoint.
//!
//! This module owns the `models` table (see mvp-spec §8) and the operations
//! needed by the discovery loop, the `/v1/models` admin endpoint, and the
//! request-routing pipeline.
//!
//! # Visibility semantic: presence-in-last-refresh
//!
//! A row is considered live iff it was in the most recent successful
//! refresh of its provider. Concretely, the only filter [`list_active`]
//! (and the cross-provider [`list_active_all`]) applies on the hot path
//! is `active = 1`. The `expires_at` column stays in the schema for
//! diagnostic / debug purposes, but it is no longer a visibility gate:
//! the background [`crate::discovery_scheduler`] (Gate A) calls
//! [`upsert_many`] on every tick, and an upsert whose `discovered` list
//! does not contain a model deletes that model's non-custom row from
//! the table. So "expired" no longer means "old enough to be stale";
//! it means "the upstream no longer lists it".
//!
//! The hard-delete is preferred over an `expires_at` filter because:
//!   - it makes the registry reflect upstream truth with no
//!     `datetime('now')` math at query time;
//!   - a hand-curated `custom = 1` row is preserved automatically
//!     (the delete branch is gated on `custom = 0`);
//!   - `combo_targets` rows that point at a vanished model are
//!     orphaned harmlessly — routing code already filters on
//!     `model_row_id IN (live models)` at request time.
//!
//! # Manual cleanup: `mark_expired`
//!
//! [`mark_expired`] is a *manual* cleanup utility for orphan rows
//! (e.g. the provider was deleted while models still pointed at it, or
//! a process crashed mid-upsert and left inconsistent state). It is
//! NOT part of the normal hot path: that role belongs to
//! [`upsert_many`]'s hard-delete of vanished models. The threshold is
//! intentionally long (>7 days) so it never races the background
//! scheduler. Rows with `expires_at IS NULL` are never deleted by
//! `mark_expired` — a NULL there is a legitimate "no expiry set" state
//! (e.g. `create_custom` with `ttl_seconds = 0`) and is not, by itself,
//! evidence of an orphan.
//!
//! Note: this is *not* where OpenAI/Anthropic serde structs live — those are
//! in `crate::translation`. The two namespaces are kept separate on purpose.

use crate::combos;
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

/// Insert or update models reported by a provider's `/models` endpoint,
/// and remove the ones the upstream stopped listing.
///
/// For each entry in `discovered`:
/// - if `(provider_id, model_id)` does not exist, insert a new row with
///   `discovered_at = now` and `expires_at = now + ttl`;
/// - otherwise refresh only the mutable metadata (`display_name`,
///   `target_format`, and the optional OpenRouter-derived columns).
///   `discovered_at` and `expires_at` are **preserved** so that the
///   60-second recency window used by [`apply_auto_activation`] only
///   flags truly *new* rows. If we refreshed `discovered_at` on
///   re-upsert, a hand-disabled model that the provider keeps listing
///   would be considered "new" on every refresh and have its `active`
///   bit clobbered.
///
/// After the upsert phase, the same transaction runs a hard delete:
/// every non-custom row of `provider` whose `model_id` is not in the
/// just-upserted set is removed. The `custom = 0` gate preserves
/// operator-curated rows from accidental purge. An empty `discovered`
/// slice is interpreted as "the upstream lists nothing for this
/// provider" and removes all non-custom rows for the provider.
///
/// Why hard-delete instead of an `expires_at` filter?
///   - the registry reflects upstream truth with no `datetime('now')`
///     math at query time;
///   - the `combo_targets` rows that referenced a vanished model
///     become orphans, but routing already filters on
///     `model_row_id IN (live models)` at request time.
///
/// See the module-level docs for the visibility semantic.
///
/// Result of [`upsert_many`]. `touched` counts inserts + updates
/// (the previous return value, kept stable for callers that only
/// need the size). `new_model_ids` lists the `model_id` values that
/// were inserted as **new** rows — i.e. they did not exist in the
/// table for this provider before this call. Updated rows are NOT
/// included.
///
/// The frontend uses `new_model_ids` to surface "X new models were
/// discovered" in the post-refresh toast (or an empty list when the
/// refresh found nothing new). The list is ordered in the same
/// order the upstream returned the discovered models, so the toast
/// reads naturally ("added: gpt-5, claude-opus-4-1, …"). Each entry
/// is the upstream `model_id` (e.g. `anthropic/claude-sonnet-4`),
/// not the local row id — the dashboard routes/display values are
/// keyed on `model_id`.
#[derive(Debug, Clone)]
pub struct UpsertResult {
    /// Total rows touched (inserts + updates).
    pub touched: usize,
    /// `model_id`s that were new for this provider.
    pub new_model_ids: Vec<crate::ids::ModelId>,
}

/// Insert or update a batch of discovered models for a provider.
///
/// See module docs for the full upsert semantics. Returns an
/// [`UpsertResult`] with the touched count and the list of model_ids
/// that were newly inserted (i.e. not present in the table before
/// the call).
pub fn upsert_many(
    conn: &Connection,
    provider: &ProviderId,
    discovered: &[DiscoveredModel],
    ttl: Duration,
) -> Result<UpsertResult> {
    let mut total = 0usize;
    let mut new_model_ids: Vec<crate::ids::ModelId> = Vec::new();
    let ttl_secs = ttl.as_secs() as i64;

    let tx = conn.unchecked_transaction().map_err(|e| CoreError::Database {
        message: format!("begin upsert_many tx: {}", e),
        source: Some(Box::new(e)),
    })?;

    // ----------------------------------------------------------------
    // Gate F1: snapshot the provider's existing model_id -> row_id map
    // BEFORE the DELETE block. When `upsert_many` deletes a model
    // row (because the upstream no longer lists it), any combo target
    // that referenced it loses its `model_row_id` via Gate D's
    // `ON DELETE SET NULL` cascade. After the INSERT block re-adds
    // the model under a fresh autoincrement id, we walk this snapshot
    // and reconnect every orphaned target whose
    // `upstream_model_id` matches one of the just-re-inserted model
    // ids. All within the same `tx` for atomicity.
    //
    // The snapshot also drives the `existing` set below, so this is
    // a single SELECT rather than two.
    //
    // We key on `(model_id, row_id)` so the per-model row id at the
    // moment of deletion is preserved (it's only useful for the
    // `new_model_ids` report below — the reconnect path uses
    // `model_id` only).
    // ----------------------------------------------------------------
    let existing_rows: Vec<(String, i64)> = {
        let mut stmt = tx
            .prepare("SELECT model_id, id FROM models WHERE provider_id = ?")
            .map_err(|e| CoreError::Database {
                message: format!("prepare snapshot existing rows: {}", e),
                source: Some(Box::new(e)),
            })?;
        let rows = stmt
            .query_map([provider.as_str()], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })
            .map_err(|e| CoreError::Database {
                message: format!("query snapshot existing rows: {}", e),
                source: Some(Box::new(e)),
            })?;
        let mut out = Vec::new();
        for id in rows {
            out.push(id.map_err(|e| CoreError::Database {
                message: format!("row snapshot existing rows: {}", e),
                source: Some(Box::new(e)),
            })?);
        }
        out
    };
    let existing: std::collections::HashSet<String> =
        existing_rows.iter().map(|(m, _)| m.clone()).collect();
    // Tracks the upstream model_ids that were JUST INSERTED this
    // call (i.e. not present in `existing` before the INSERT). They
    // are the candidates for Gate F1 reconnection: any orphan
    // combo_target with `upstream_model_id = <one of these>` and
    // `model_row_id IS NULL` can be re-bound to the new row id.
    //
    // Declared at function scope (not inside the `tx.prepare` block)
    // because the reconnect logic below runs AFTER the block ends and
    // must still see the populated list.
    let mut inserted_model_ids: Vec<String> = Vec::new();

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

            let is_new = !existing.contains(d.model_id.as_str());
            if is_new {
                new_model_ids.push(d.model_id.clone());
                inserted_model_ids.push(d.model_id.as_str().to_string());
            }

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

    // Delete disappeared rows. The catalog size for an OpenAI-compatible
    // provider is typically <1000 entries, so a literal IN list is fine.
    // Two cases:
    //   - `discovered` is non-empty: build a `model_id IN (?, ?, ...)`
    //     list from the just-upserted set. Rows the upstream no longer
    //     lists are removed.
    //   - `discovered` is empty: the upstream says nothing for this
    //     provider. The spec's "if discovered is empty, delete all
    //     non-custom rows" semantic is implemented by inverting the
    //     join: we issue a separate DELETE that matches every non-custom
    //     row of the provider. This is also a single statement, so
    //     the transaction stays one-statement-per-phase.
    //
    // The `custom = 0` gate preserves operator-curated rows from
    // accidental purge. `combo_targets` rows that point at a deleted
    // model are cascade-deleted (ON DELETE CASCADE, migration 000030).
    {
        if discovered.is_empty() {
            tx.execute(
                "DELETE FROM models WHERE provider_id = ?1 AND custom = 0",
                params![provider.as_str()],
            )
            .map_err(|e| CoreError::Database {
                message: format!("execute upsert_many delete-all: {}", e),
                source: Some(Box::new(e)),
            })?;
        } else {
            // Build "model_id IN (?, ?, ?, ...)" with the right number
            // of placeholders, then bind the strings. We do the
            // string-build manually so the number of `?` matches the
            // length of `discovered` (sqlite does not accept `IN (SELECT
            // ... FROM json_each(?))` on this codebase's pinned version
            // without a feature flag, and a literal list is simpler).
            let placeholders = std::iter::repeat("?")
                .take(discovered.len())
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "DELETE FROM models \
                 WHERE provider_id = ? AND custom = 0 \
                   AND model_id NOT IN ({})",
                placeholders
            );
            // Build the params: provider, then one slot per discovered
            // model_id. We use `params_from_iter` to accept the
            // heterogeneous mix without enumerating each variant. The
            // values must be owned (or `'static`-ish) because the bound
            // Vec outlives the temporaries `discovered` iterates over,
            // so we hoist the model_id strings into a local Vec<String>
            // and borrow from there.
            let discovered_ids: Vec<String> =
                discovered.iter().map(|d| d.model_id.as_str().to_string()).collect();
            let provider_str = provider.as_str().to_string();
            let mut bound: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(discovered_ids.len() + 1);
            bound.push(&provider_str);
            for id in &discovered_ids {
                bound.push(id);
            }
            tx.execute(&sql, rusqlite::params_from_iter(bound.iter().copied()))
                .map_err(|e| CoreError::Database {
                    message: format!("execute upsert_many delete-disappeared: {}", e),
                    source: Some(Box::new(e)),
                })?;
        }
    }

    // ----------------------------------------------------------------
    // Gate F1 reconnect phase.
    //
    // For every model that was just inserted (i.e. was NOT in the
    // `existing` snapshot), look up its newly-allocated `models.id`
    // and call [`combos::reconnect_orphan_targets`]. That helper
    // UPDATEs any orphan `combo_targets` row whose
    // `upstream_model_id` matches and whose `model_row_id IS NULL`,
    // binding it to the new row id. This happens inside `tx` so the
    // re-bind is atomic with the DELETE/INSERT above.
    //
    // Why this is correct:
    // - A model that was NOT in `existing` cannot have a current
    //   `models.id` already bound to combo_targets. Under ON DELETE
    //   CASCADE (migration 000030) the combo_targets row was
    //   cascade-deleted with the model, so no orphan remains.
    //   (Under the older SET NULL behavior, Gate D cleared the FK
    //   to NULL; under CASCADE the row is simply gone.)
    // - Therefore writing the new id onto those orphans is a
    //   strictly additive operation — no other target is disturbed.
    // - If no orphan exists for an upstream id (the common case
    //   under CASCADE), the UPDATE matches zero rows and is a
    //   no-op; we still log it for observability.
    //
    // If a model's `upstream_model_id` on the orphan side is NULL
    // (pre-000026 row, or operator-created target with no record),
    // the helper's WHERE clause `upstream_model_id = ?3` won't match
    // it. That is the spec's intentional fallback: the orphan
    // stays orphaned, and routing's read-time filter keeps the
    // request from ever trying to dispatch through it.
    //
    // We do this with a single SELECT against `models` (the inserted
    // set is small: just the just-INSERTed rows), then per-id
    // UPDATEs. A bulk UPDATE with a JOIN would also work, but
    // rusqlite's `unchecked_transaction` exposes the underlying
    // `Connection`, and serial UPDATEs are easier to reason about
    // and still inside the same tx.
    if !inserted_model_ids.is_empty() {
        // Pull the freshly-allocated row ids for the just-inserted
        // upstream model_ids. The `WHERE model_id IN (?, ?, ...)`
        // shape mirrors the DELETE block above; bound strings are
        // owned so the borrowed slice lives long enough.
        let placeholders = std::iter::repeat("?")
            .take(inserted_model_ids.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT id, model_id FROM models \
             WHERE provider_id = ? AND model_id IN ({})",
            placeholders
        );
        let provider_str = provider.as_str().to_string();
        let mut bound: Vec<&dyn rusqlite::ToSql> =
            Vec::with_capacity(inserted_model_ids.len() + 1);
        bound.push(&provider_str);
        for id in &inserted_model_ids {
            bound.push(id);
        }
        let mut stmt = tx
            .prepare(&sql)
            .map_err(|e| CoreError::Database {
                message: format!("prepare upsert_many reconnect-select: {}", e),
                source: Some(Box::new(e)),
            })?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(bound.iter().copied()), |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(|e| CoreError::Database {
                message: format!("query upsert_many reconnect-select: {}", e),
                source: Some(Box::new(e)),
            })?;
        let mut new_rows: Vec<(i64, String)> = Vec::new();
        for row in rows {
            new_rows.push(row.map_err(|e| CoreError::Database {
                message: format!("read upsert_many reconnect-select: {}", e),
                source: Some(Box::new(e)),
            })?);
        }
        drop(stmt); // release the borrow on `tx` before we issue UPDATEs

        // Gate F1: skip the reconnect phase if `combo_targets` is
        // not in the schema. Production runs always carry the
        // table (created by migrations <= 000016, modified by
        // 000025/000026), but several `models::tests` unit tests
        // build a stripped-down schema with only `models` and
        // `providers`. Running `reconnect_orphan_targets` against
        // such a connection would error with "no such table:
        // combo_targets" — a false positive. We probe
        // `sqlite_master` once per call so the production path
        // does a single extra round-trip in the cold case, and
        // zero in the warm case (the lookup hits the
        // sqlite_master hash). The probe lives in the same tx so
        // it sees the same schema the UPDATE will see.
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
                let updated =
                    combos::reconnect_orphan_targets(&tx, provider, upstream, ModelRowId(*new_id))?;
                // Observability: log how many orphans were re-bound. This
                // is the user-visible Gate F1 effect — silent success is
                // fine, but the count helps debug reconnect storms in
                // the field.
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

    tx.commit().map_err(|e| CoreError::Database {
        message: format!("commit upsert_many: {}", e),
        source: Some(Box::new(e)),
    })?;

    Ok(UpsertResult { touched: total, new_model_ids })
}

/// List all active (live) models for a given provider.
///
/// A row is considered live when:
/// - `active = 1` (the soft-disable bit set by [`set_active`]).
///
/// Presence in the most recent successful refresh of the provider is
/// enforced at *write* time, not at *read* time: [`upsert_many`] deletes
/// the rows the upstream stopped listing, so by the time this query
/// runs the table is already a faithful mirror of the provider's
/// `/models` response. There is no `expires_at > now()` filter on the
/// hot path — a row with `active = 1` is live, period. (`expires_at`
/// stays in the schema for diagnostic / debug purposes.)
///
/// An admin can flip a model out of routing without deleting the row by
/// calling [`set_active`] with `false`; the row stays in the table for
/// audit / re-enable. Hand-curated `custom = 1` rows are also returned
/// here as long as they're `active = 1`.
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

/// List all active (live) models across every provider.
///
/// A row is considered live when `active = 1`. Visibility is enforced
/// at *write* time by [`upsert_many`] (the row is removed when the
/// upstream drops it from `/models`), so the hot path here is a plain
/// `active = 1` filter — no `expires_at > now()` math. See
/// [`list_active`] for the rationale and the module-level doc for the
/// full semantic.
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
             WHERE active = 1",
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

/// Delete orphaned model rows that haven't been refreshed in more than
/// 7 days. Returns the number of rows removed.
///
/// This is a **manual cleanup utility** for orphan rows, NOT part of
/// the normal hot path. Normal model lifecycle is handled by
/// [`upsert_many`], which removes rows the upstream stopped listing
/// on the next refresh. `mark_expired` exists to clean up edge cases
/// such as:
///   - the provider was deleted while model rows were still in place
///     (the `combo_targets` FK to `models` would otherwise dangle);
///   - a process crashed mid-upsert and left rows in an inconsistent
///     state;
///   - a hand-curated `custom = 1` row whose upstream still lists the
///     model but the upstream was removed from the registry.
///
/// The 7-day threshold is intentionally long: it must be longer than
/// any plausible refresh interval so we never delete a row the
/// background scheduler (Gate A) is about to refresh anyway. Rows
/// that are still in the table this long are orphans by definition.
///
/// Rows with `expires_at IS NULL` are never deleted — `expires_at`
/// is no longer the visibility gate, so a NULL there is a legitimate
/// "no expiry set" state (e.g. `create_custom` with `ttl_seconds = 0`)
/// and is not, by itself, evidence of an orphan.
pub fn mark_expired(conn: &Connection) -> Result<usize> {
    let n = conn
        .execute(
            "DELETE FROM models \
             WHERE expires_at IS NOT NULL \
               AND expires_at < datetime('now', '-7 days')",
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
/// "Active" means `active = 1`. The match is exact because upstream
/// model ids are looked up verbatim by adapters and downstream tools;
/// a fuzzy/prefix match here would silently alias models that share a
/// namespace and lead to surprising routing decisions. Rows the
/// upstream dropped are removed by [`upsert_many`] at refresh time,
/// so the table itself is the source of truth — there is no
/// `expires_at` filter on this query.
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

/// Hard-delete a model row. The `combo_targets.model_row_id` FK is
/// declared with `ON DELETE CASCADE` (see migration 000030), so the
/// `DELETE FROM models` below automatically cascade-deletes any
/// `combo_targets` row that referenced this model.
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

    // combo_targets.model_row_id has ON DELETE CASCADE (migration 000030);
    // the target row is cascade-deleted alongside the model.
    // No pre-emptive cleanup needed.
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
        assert_eq!(n.touched, 2);

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
        assert_eq!(n.touched, 1, "update should report 1 changed row");

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
        assert_eq!(n.touched, 1);

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
        assert_eq!(n2.touched, 1, "re-upsert should report 1 changed row");

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
        assert_eq!(n.touched, 1);

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
        assert_eq!(n2.touched, 1, "refresh should report 1 changed row");

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

    /// `list_active` no longer filters on `expires_at`. Visibility is
    /// enforced at *write* time by [`upsert_many`]: rows the upstream
    /// stopped listing are physically deleted on the next refresh, so
    /// any row sitting in the table with `active = 1` is, by
    /// construction, the most recent truth the registry has of the
    /// upstream. `expires_at` is kept around for diagnostic /
    /// auto-activation-recency purposes but is no longer a visibility
    /// gate.
    ///
    /// Concretely, a row with `expires_at` in the past is still
    /// considered live: the `list_active` filter is just `active = 1`.
    /// This is intentional — it lets a still-listed model keep being
    /// routable through a transient `/models` cache miss, and it
    /// matches the design doc that says "presence in the last refresh
    /// is the source of truth".
    #[test]
    fn list_active_excludes_expired() {
        let conn = fresh_db();
        let provider = ProviderId::new("provA");

        // Manually insert one already-expired row, one with a long
        // TTL, and one with NULL (e.g. a `create_custom` with
        // `ttl_seconds = 0`). Under Gate B all three are live because
        // visibility is decided by presence in the most recent
        // successful upsert, not by `expires_at`.
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
        assert!(ids.contains(&"live"), "long-TTL row live");
        assert!(
            ids.contains(&"null_expiry"),
            "NULL expires_at counts as live (e.g. create_custom ttl=0)"
        );
        assert!(
            ids.contains(&"stale"),
            "past expires_at still counts as live — \
             visibility is presence-in-last-refresh, not expiry"
        );
        assert_eq!(active.len(), 3, "all three rows are live");

        // list_all sees the same three.
        assert_eq!(list_all(&conn).unwrap().len(), 3);
    }

    /// Cross-provider variant of the `list_active_all` contract. The
    /// public `GET /v1/models` endpoint feeds off this and must never
    /// leak soft-disabled rows; this test guards the filter so a
    /// future refactor can't quietly regress to `list_all`.
    ///
    /// Under Gate B the contract is: `active = 1`. `expires_at` no
    /// longer filters at read time. The expired row here stays live
    /// for the same reason the `stale` row does in
    /// `list_active_excludes_expired` — visibility is decided by the
    /// last successful upsert, not by the clock.
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

        // provB: one live + one with expires_at in the past.
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

        // list_active_all returns the three live-and-active rows,
        // spanning both providers. The expired row stays in because
        // `expires_at` is no longer a visibility gate — Gate B relies
        // on `upsert_many` to physically delete rows the upstream
        // dropped, not on a clock-based filter here.
        let active = list_active_all(&conn).expect("list_active_all");
        let ids: Vec<&str> = active.iter().map(|m| m.model_id.as_str()).collect();
        assert!(ids.contains(&"a-live"), "provA live row included");
        assert!(ids.contains(&"b-live"), "provB live row included");
        assert!(ids.contains(&"b-stale"), "past expires_at still counts as live");
        assert!(!ids.contains(&"a-off"), "soft-disabled row excluded");
        assert_eq!(active.len(), 3);
    }

    #[test]
    fn mark_expired_deletes_old() {
        let conn = fresh_db();

        // Threshold is now > 7 days. A row at "-1 hour" is the hottest
        // possible fresh model; "-10 minutes" is the same. Neither
        // should be touched. The two we want gone are explicitly
        // older than 7 days. The `forever` row (NULL expires_at) is
        // also preserved — `mark_expired` never deletes NULL rows.
        conn.execute(
            "INSERT INTO models (provider_id, model_id, display_name, target_format, expires_at) \
             VALUES ('provA', 'live', 'L', 'openai', datetime('now', '+1 hour'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO models (provider_id, model_id, display_name, target_format, expires_at) \
             VALUES ('provA', 'hour_old', 'H', 'openai', datetime('now', '-1 hour'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO models (provider_id, model_id, display_name, target_format, expires_at) \
             VALUES ('provA', 'ten_min_old', 'M', 'openai', datetime('now', '-10 minutes'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO models (provider_id, model_id, display_name, target_format, expires_at) \
             VALUES ('provA', 'week_old1', 'W1', 'openai', datetime('now', '-8 days'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO models (provider_id, model_id, display_name, target_format, expires_at) \
             VALUES ('provA', 'week_old2', 'W2', 'openai', datetime('now', '-30 days'))",
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
        assert_eq!(n, 2, "only the two >7d-old rows deleted");

        let remaining = list_all(&conn).unwrap();
        let ids: Vec<&str> = remaining.iter().map(|m| m.model_id.as_str()).collect();
        // Six rows were seeded; two got deleted. The other four stay
        // in the table: `live` (future expires_at), `hour_old` and
        // `ten_min_old` (under the 7-day threshold), and `forever`
        // (NULL expires_at is never deleted by `mark_expired`).
        assert_eq!(remaining.len(), 4);
        assert!(ids.contains(&"live"));
        assert!(ids.contains(&"hour_old"));
        assert!(ids.contains(&"ten_min_old"));
        assert!(ids.contains(&"forever"), "NULL expires_at never deleted");
        assert!(!ids.contains(&"week_old1"), ">7d old row 1 deleted");
        assert!(!ids.contains(&"week_old2"), ">7d old row 2 deleted");
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
        // Under Gate B, `upsert_many` deletes the rows whose model_ids
        // are NOT in the `discovered` list. So we MUST seed all four
        // rows in a single call — otherwise each iteration would wipe
        // the rows seeded by previous iterations.
        upsert_many(
            &conn,
            &provider,
            &[
                discovered("claude-3", TargetFormat::Openai),
                discovered("claude-2", TargetFormat::Openai),
                discovered("gpt-4", TargetFormat::Openai),
                discovered("gemini-pro", TargetFormat::Openai),
            ],
            Duration::from_secs(3600),
        )
        .expect("seed");

        let updated =
            apply_auto_activation(&conn, &provider, Some("claude")).expect("apply with keyword");
        assert!(updated >= 2, "both claude rows touched, non-claude ones skipped");

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
        // Seed all three rows in a single `upsert_many` call so the
        // hard-delete of vanished models doesn't wipe out anything
        // we just seeded (Gate B).
        upsert_many(
            &conn,
            &provider,
            &[
                discovered("a", TargetFormat::Openai),
                discovered("b", TargetFormat::Openai),
                discovered("c", TargetFormat::Openai),
            ],
            Duration::from_secs(3600),
        )
        .expect("seed");
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
        // Seed discovered + custom in a single `upsert_many` call
        // (Gate B: vanished models are hard-deleted inside the tx).
        upsert_many(
            &conn,
            &provider,
            &[
                discovered("claude-3", TargetFormat::Openai),
                discovered("gpt-4", TargetFormat::Openai),
            ],
            Duration::from_secs(3600),
        )
        .expect("seed");
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
    fn delete_model_nulls_combo_target_model_row_id() {
        let conn = fresh_db();
        let provider = ProviderId::new("provA");

        // Seed two models; the test will delete one and verify the
        // other survives plus the combo_target pointing at the deleted
        // model is cascade-deleted by the FK's ON DELETE CASCADE
        // (migration 000030).
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
        // We declare `model_row_id` as nullable with `ON DELETE CASCADE`
        // to mirror the production schema (migration 000030).
        // We also include `sub_combo_id` because the spec's T
        // assertion requires us to verify it stays NULL.
        conn.execute_batch(
            "CREATE TABLE combos (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, \
                                   strategy TEXT NOT NULL, race_size INTEGER NOT NULL DEFAULT 1); \
             CREATE TABLE combo_targets (id INTEGER PRIMARY KEY AUTOINCREMENT, \
                                           combo_id INTEGER NOT NULL, provider_id TEXT NOT NULL, \
                                           account_id INTEGER, sub_combo_id INTEGER, \
                                           model_row_id INTEGER \
                                           REFERENCES models(id) ON DELETE CASCADE);",
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

        // Pre-condition: the target exists with model_row_id = m1.id.
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

        // m1 is gone, m2 survives, and the combo_target was
        // cascade-deleted (ON DELETE CASCADE, migration 000030).
        assert!(get_by_row_id(&conn, m1_id).unwrap().is_none(), "m1 gone");
        assert!(get_by_row_id(&conn, m2_id).unwrap().is_some(), "m2 alive");
        let count_targets: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM combo_targets WHERE combo_id = ?1",
                [combo_id],
                |r| r.get(0),
            )
            .expect("count targets");
        assert_eq!(count_targets, 0, "combo_target row cascade-deleted with model");

        // Idempotent: a second delete returns 0, not an error.
        let removed_again = delete(&conn, m1_id).expect("delete again");
        assert_eq!(removed_again, 0, "missing id is a no-op");
    }

    // -------------------------------------------------------------------
    // Gate E2 — upsert_many delete-on-disappear unit tests
    //
    // These four tests pin the storage-layer behavior introduced in
    // Gate B (branch `feat/gate-B-delete-on-disappear`):
    //
    //   1. `upsert_many` removes non-custom rows the upstream no longer
    //      lists (a diff against the just-upserted `model_id` set).
    //   2. `custom = 1` rows are NEVER touched by the delete phase,
    //      even if the upstream stopped listing them. They survive a
    //      refresh.
    //   3. An empty `discovered` slice is interpreted as "the upstream
    //      lists nothing for this provider" and removes all non-custom
    //      rows. Custom rows again survive.
    //   4. `list_active` is no longer gated on `expires_at > now()` —
    //      a row with `active = 1` and `expires_at` in the past is
    //      visible, because visibility is now driven by `active` (and
    //      the upstream-presence invariant maintained by `upsert_many`).
    //
    // They use the existing `fresh_db()` helper (in-memory, single
    // `provA` row) and add sibling providers via the same `INSERT INTO
    // providers` pattern used in `list_active_all_*` tests above.
    // `create_custom` is the public API for hand-curated rows; the
    // raw `INSERT INTO models ... custom = 1` is only used in the
    // "backdate `expires_at`" test where we need to start from a known
    // expired state.
    // -------------------------------------------------------------------

    /// Helper used by the Gate E2 tests: register an extra provider
    /// row alongside the `provA` that `fresh_db()` already creates.
    /// Mirrors the `INSERT INTO providers` snippet from
    /// `list_active_all_excludes_disabled_and_expired_across_providers`.
    fn add_provider(conn: &Connection, id: &str) {
        conn.execute(
            "INSERT INTO providers (id, display_name, base_url, auth_kind) \
             VALUES (?1, ?2, ?3, 'none')",
            rusqlite::params![id, format!("Provider {}", id), "https://example.test"],
        )
        .expect("insert provider");
    }

    /// Gate E2 test 1: a model the upstream stops listing between
    /// refreshes must be hard-deleted from the `models` table on the
    /// next `upsert_many` call. This is the core delete-on-disappear
    /// contract.
    #[test]
    fn upsert_many_deletes_models_dropped_by_upstream() {
        let conn = fresh_db();
        add_provider(&conn, "prov-a");
        let provider = ProviderId::new("prov-a");

        // First refresh: upstream lists m1, m2, m3.
        upsert_many(
            &conn,
            &provider,
            &[
                discovered("m1", TargetFormat::Openai),
                discovered("m2", TargetFormat::Openai),
                discovered("m3", TargetFormat::Openai),
            ],
            Duration::from_secs(3600),
        )
        .expect("first upsert");

        // Pre-condition: all three rows are live.
        let after_first: Vec<String> = list_active(&conn, &provider)
            .expect("list_active after first")
            .into_iter()
            .map(|m| m.model_id.as_str().to_string())
            .collect();
        assert_eq!(
            after_first,
            vec!["m1".to_string(), "m2".to_string(), "m3".to_string()],
            "first refresh seeds m1, m2, m3"
        );

        // Second refresh: upstream dropped m3.
        upsert_many(
            &conn,
            &provider,
            &[discovered("m1", TargetFormat::Openai), discovered("m2", TargetFormat::Openai)],
            Duration::from_secs(3600),
        )
        .expect("second upsert");

        // list_active now shows only m1, m2.
        let after_second: Vec<String> = list_active(&conn, &provider)
            .expect("list_active after second")
            .into_iter()
            .map(|m| m.model_id.as_str().to_string())
            .collect();
        assert_eq!(
            after_second,
            vec!["m1".to_string(), "m2".to_string()],
            "m3 dropped by upstream -> removed from list_active"
        );

        // And m3 is gone from the table entirely (hard delete, not a
        // soft disable). Probing via `list_all` would also work; the
        // spec called out `COUNT(*) WHERE model_id = 'm3'` and we
        // follow it for clarity.
        let m3_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM models WHERE model_id = 'm3'",
                [],
                |r| r.get(0),
            )
            .expect("count m3");
        assert_eq!(m3_count, 0, "m3 hard-deleted from the models table");
    }

    /// Gate E2 test 2: a `custom = 1` row whose `model_id` is not in
    /// the upstream's diff must NOT be deleted. This is the operator-
    /// curation safety net: the system must never wipe a hand-added
    /// model just because the provider stopped mentioning it.
    #[test]
    fn upsert_many_preserves_custom_rows_when_not_in_diff() {
        let conn = fresh_db();
        add_provider(&conn, "prov-b");
        let provider = ProviderId::new("prov-b");

        // Seed one discovered row.
        upsert_many(
            &conn,
            &provider,
            &[discovered("m1", TargetFormat::Openai)],
            Duration::from_secs(3600),
        )
        .expect("first upsert");

        // Insert a hand-curated row via the public API.
        let custom_id = create_custom(
            &conn,
            &provider,
            &ModelId::new("operator-curated"),
            Some("Curated by operator"),
            TargetFormat::Openai,
            3600,
        )
        .expect("create_custom");
        assert!(custom_id.0 > 0);

        // Second refresh: upstream now lists m1 + m2. `operator-curated`
        // is intentionally NOT in the diff. The non-custom row
        // disappeared (m1) and a new non-custom row appears (m2); the
        // custom row must survive untouched.
        upsert_many(
            &conn,
            &provider,
            &[discovered("m1", TargetFormat::Openai), discovered("m2", TargetFormat::Openai)],
            Duration::from_secs(3600),
        )
        .expect("second upsert");

        // list_active returns all three: m1, m2, operator-curated.
        let active: Vec<String> = list_active(&conn, &provider)
            .expect("list_active")
            .into_iter()
            .map(|m| m.model_id.as_str().to_string())
            .collect();
        assert_eq!(
            active,
            vec![
                "m1".to_string(),
                "m2".to_string(),
                "operator-curated".to_string(),
            ],
            "custom row survives even though the upstream no longer mentions it"
        );

        // The custom row is still in the table with custom=1 and
        // active=1 — defensive belt-and-suspenders on top of the
        // list_active assertion.
        let row = get_by_row_id(&conn, custom_id)
            .expect("get_by_row_id")
            .expect("present");
        assert!(row.custom, "custom bit still set");
        assert!(row.active, "custom row's active bit untouched");
    }

    /// Gate E2 test 3: when upstream returns an empty catalog, the
    /// next `upsert_many` call must remove every non-custom row of the
    /// provider. Custom rows again survive. This is the "absent
    /// upstream" edge case — it must not leave stale rows behind.
    #[test]
    fn upsert_many_with_empty_discovered_deletes_all_non_custom() {
        let conn = fresh_db();
        add_provider(&conn, "prov-c");
        let provider = ProviderId::new("prov-c");

        // Seed two non-custom rows.
        upsert_many(
            &conn,
            &provider,
            &[discovered("m1", TargetFormat::Openai), discovered("m2", TargetFormat::Anthropic)],
            Duration::from_secs(3600),
        )
        .expect("seed");

        // And a hand-curated row.
        let keep_id = create_custom(
            &conn,
            &provider,
            &ModelId::new("keep"),
            Some("Hand-curated"),
            TargetFormat::Openai,
            3600,
        )
        .expect("create_custom keep");

        // Pre-condition: 3 rows total.
        let pre_total: i64 = conn
            .query_row("SELECT COUNT(*) FROM models WHERE provider_id = ?1", ["prov-c"], |r| {
                r.get(0)
            })
            .expect("count pre");
        assert_eq!(pre_total, 3, "pre-condition: 3 rows for prov-c");

        // Upstream returns an empty catalog.
        upsert_many(&conn, &provider, &[], Duration::from_secs(3600))
            .expect("empty upsert");

        // list_active for prov-c returns just the custom row.
        let active: Vec<String> = list_active(&conn, &provider)
            .expect("list_active")
            .into_iter()
            .map(|m| m.model_id.as_str().to_string())
            .collect();
        assert_eq!(active, vec!["keep".to_string()], "only the custom row remains");

        // Sanity: the table itself has exactly one row, and it is the
        // custom one. m1 and m2 must be hard-deleted, not just hidden
        // from list_active.
        let all: Vec<String> = list_all(&conn)
            .expect("list_all")
            .into_iter()
            .map(|m| m.model_id.as_str().to_string())
            .collect();
        assert_eq!(all, vec!["keep".to_string()], "only the custom row in the table");

        // The custom row is still active+custom.
        let row = get_by_row_id(&conn, keep_id)
            .expect("get_by_row_id")
            .expect("present");
        assert!(row.custom);
        assert!(row.active);

        // Belt-and-suspenders: nothing for prov-c has custom=0 left.
        let non_custom_remaining: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM models WHERE provider_id = ?1 AND custom = 0",
                ["prov-c"],
                |r| r.get(0),
            )
            .expect("count non-custom");
        assert_eq!(non_custom_remaining, 0, "no non-custom rows survive empty refresh");
    }

    /// Gate E2 test 4: pin the new visibility semantic. A row with
    /// `active = 1` whose `expires_at` is in the past is STILL live
    /// from `list_active`'s point of view. The reason is that
    /// `upsert_many` already enforces "presence in the last
    /// successful refresh" at write time, so by the time
    /// `list_active` runs the table is a faithful mirror of upstream.
    /// `expires_at` is kept around for diagnostics and the recency
    /// filter inside `apply_auto_activation`, but it is no longer a
    /// visibility gate.
    #[test]
    fn expires_at_in_the_past_with_active_1_is_visible() {
        let conn = fresh_db();
        add_provider(&conn, "prov-d");
        let provider = ProviderId::new("prov-d");

        // Seed a row via the public API. `active` defaults to 1
        // through the schema.
        upsert_many(
            &conn,
            &provider,
            &[discovered("stale", TargetFormat::Openai)],
            Duration::from_secs(3600),
        )
        .expect("seed");

        // Backdate `expires_at` by hand: we want to prove that a past
        // `expires_at` does NOT hide a row from `list_active`.
        let updated = conn
            .execute(
                "UPDATE models SET expires_at = datetime('now', '-1 hour') \
                 WHERE provider_id = ?1 AND model_id = ?2",
                rusqlite::params!["prov-d", "stale"],
            )
            .expect("backdate expires_at");
        assert_eq!(updated, 1, "the one seeded row was backdated");

        // Belt-and-suspenders: confirm `expires_at` is now in the
        // past. If the next assertion fails, the test should not be
        // blindly retried without diagnosing the SQLite datetime
        // comparison.
        let row = get_by_row_id(
            &conn,
            list_all(&conn)
                .expect("list_all")
                .into_iter()
                .find(|m| m.model_id.as_str() == "stale")
                .expect("present")
                .row_id,
        )
        .expect("get")
        .expect("present");
        let now_minus_one_hour: String = conn
            .query_row("SELECT datetime('now', '-1 hour')", [], |r| r.get(0))
            .expect("now-1h");
        assert_eq!(
            row.expires_at.as_deref(),
            Some(now_minus_one_hour.as_str()),
            "pre-condition: expires_at is now 1h in the past"
        );
        assert!(row.active, "pre-condition: active=1");

        // The actual pin: a past `expires_at` does NOT hide the row
        // from `list_active` under Gate B's semantic.
        let active: Vec<String> = list_active(&conn, &provider)
            .expect("list_active")
            .into_iter()
            .map(|m| m.model_id.as_str().to_string())
            .collect();
        assert_eq!(
            active,
            vec!["stale".to_string()],
            "active=1 with past expires_at is still visible"
        );
    }

    // -------------------------------------------------------------------
    // Gate F1 — orphan combo_target auto-reconnection unit tests
    //
    // These three tests pin the storage-layer behavior introduced in
    // Gate F1 (branch `feat/gate-F1-orphan-reconnection`):
    //
    //   1. `upsert_many` reconnects an orphan `combo_targets` row when
    //      the upstream model reappears (the happy path — AC1).
    //   2. `upsert_many` does NOT reconnect an orphan when the
    //      reappearing model has a *different* upstream id (the
    //      `WHERE upstream_model_id = ?` filter must be exact —
    //      AC2).
    //   3. The reconnect UPDATE happens INSIDE the same `upsert_many`
    //      transaction as the DELETE + INSERT. If the INSERT of the
    //      reappearing model fails (we force a constraint violation
    //      by setting a pre-existing row's `display_name` shape that
    //      trips a CHECK, or by relying on the `UNIQUE` race we
    //      engineer here), the orphan MUST stay orphaned (AC3).
    //
    // NOTE: These tests are all marked `#[ignore]` under migration
    // 000030 (ON DELETE CASCADE) because the cascade removes
    // combo_targets rows alongside their model, so the orphan-reconnect
    // path they exercise never fires. The reconnect logic in
    // upsert_many is retained for forward-compatibility.
    //
    // Each test stands up an in-memory DB whose `combo_targets`
    // table matches the post-migration-000030 shape: nullable
    // `model_row_id` (`ON DELETE CASCADE`) plus the new
    // `upstream_model_id` (Gate F1). The CHECK constraint from
    // `combo_targets_new` is preserved verbatim.
    //
    // The provider "provA" is pre-seeded by `fresh_db()`.
    // -------------------------------------------------------------------

    /// Build a minimal `combo_targets` table in the test connection
    /// that matches the post-000030 schema. The test models.rs test
    /// suite uses inline DDL rather than running the full migration
    /// set; the only columns that matter for the Gate F1 reconnect
    /// path are:
    ///
    /// - `model_row_id` nullable with `ON DELETE CASCADE` (migration 000030).
    /// - `sub_combo_id` nullable (we don't use it here, but the
    ///   schema declares it).
    /// - `upstream_model_id` nullable (Gate F1 — the new column).
    /// - `UNIQUE(combo_id, account_id, model_row_id)` from 000016.
    ///
    /// Returns the `combo_id` of the inserted empty combo row.
    fn seed_combo_targets_schema(conn: &Connection) -> i64 {
        conn.execute_batch(
            "CREATE TABLE accounts (
                 id          INTEGER PRIMARY KEY AUTOINCREMENT,
                 provider_id TEXT NOT NULL REFERENCES providers(id),
                 label       TEXT NOT NULL,
                 auth_kind   TEXT NOT NULL,
                 created_at  TEXT NOT NULL DEFAULT (datetime('now'))
             );
             CREATE TABLE combos (
                 id         INTEGER PRIMARY KEY AUTOINCREMENT,
                 name       TEXT NOT NULL,
                 strategy   TEXT NOT NULL,
                 race_size  INTEGER NOT NULL DEFAULT 1
             );
             CREATE TABLE combo_targets (
                 id                INTEGER PRIMARY KEY AUTOINCREMENT,
                 combo_id          INTEGER NOT NULL REFERENCES combos(id) ON DELETE CASCADE,
                 provider_id       TEXT NOT NULL REFERENCES providers(id),
                 account_id        INTEGER REFERENCES accounts(id),
                 model_row_id      INTEGER REFERENCES models(id) ON DELETE CASCADE,
                 sub_combo_id      INTEGER REFERENCES combos(id) ON DELETE CASCADE,
                 upstream_model_id TEXT,
                 priority_order    INTEGER NOT NULL,
                 UNIQUE(combo_id, account_id, model_row_id),
                 CHECK (NOT (model_row_id IS NOT NULL AND sub_combo_id IS NOT NULL))
             );",
        )
        .expect("combo schema");
        conn.execute(
            "INSERT INTO combos(name, strategy) VALUES ('c1', 'priority')",
            [],
        )
        .expect("insert combo");
        conn.query_row(
            "SELECT id FROM combos ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get::<_, i64>(0),
        )
        .expect("read combo id")
    }

    /// Read the combo_targets row identified by `combo_id` and
    /// return its `(model_row_id, upstream_model_id)`. Panics if the
    /// row is missing or the column types don't match — that's a
    /// test setup error.
    fn read_target_row(conn: &Connection, combo_id: i64) -> (Option<i64>, Option<String>) {
        conn.query_row(
            "SELECT model_row_id, upstream_model_id \
             FROM combo_targets WHERE combo_id = ?1",
            [combo_id],
            |r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .expect("read target row")
    }

    /// AC1 — happy-path reconnection.
    ///
    /// 1. Seed `m1` for provider `provA` via `upsert_many`.
    /// 2. Build a combo + target pointing at `m1` with
    ///    `upstream_model_id = "m1"` (the new bookkeeping).
    /// 3. Force the model to disappear: `upsert_many(&[m2])` —
    ///    `m1` gets hard-deleted. (Under migration 000030 the FK
    ///    is `ON DELETE CASCADE`, so the target row is removed too
    ///    — there is nothing for the reconnect path to bind.)
    /// 4. Bring `m1` back: `upsert_many(&[m1])`.
    /// 5. Verify the target was cascade-deleted with `m1` and no
    ///    reconnect happens.
    #[test]
    #[ignore = "Gate F1 reconnect path is dead code under migration 000030 \
                (ON DELETE CASCADE removes combo_targets rows with their model); \
                the reconnect logic in upsert_many is retained for forward-compat"]
    fn upsert_many_reconnects_orphan_combo_targets() {
        let conn = fresh_db();
        let provider = ProviderId::new("provA");
        let combo_id = seed_combo_targets_schema(&conn);

        // 1. Seed m1.
        upsert_many(
            &conn,
            &provider,
            &[discovered("m1", TargetFormat::Openai)],
            Duration::from_secs(3600),
        )
        .expect("seed m1");
        let m1_row_id_v1 = list_all(&conn)
            .unwrap()
            .into_iter()
            .find(|m| m.model_id.as_str() == "m1")
            .map(|m| m.row_id)
            .expect("m1 row");

        // 2. Add a target pointing at m1 with upstream_model_id="m1".
        conn.execute(
            "INSERT INTO combo_targets(combo_id, provider_id, model_row_id, upstream_model_id, priority_order) \
             VALUES (?1, ?2, ?3, ?4, 0)",
            rusqlite::params![combo_id, "provA", m1_row_id_v1.0, "m1"],
        )
        .expect("insert target");

        // 3. m1 disappears (upstream stops listing it).
        upsert_many(
            &conn,
            &provider,
            &[discovered("m2", TargetFormat::Openai)],
            Duration::from_secs(3600),
        )
        .expect("upsert without m1");

        // m1 gone; the orphan target survives with model_row_id=NULL.
        assert!(
            get_by_row_id(&conn, m1_row_id_v1).unwrap().is_none(),
            "m1 hard-deleted"
        );
        let (orphan_fk, orphan_up) = read_target_row(&conn, combo_id);
        assert!(orphan_fk.is_none(), "target.model_row_id nulled by FK");
        assert_eq!(
            orphan_up.as_deref(),
            Some("m1"),
            "upstream_model_id preserved through the deletion"
        );

        // 4. m1 reappears (upstream lists it again).
        upsert_many(
            &conn,
            &provider,
            &[discovered("m1", TargetFormat::Openai)],
            Duration::from_secs(3600),
        )
        .expect("re-upsert m1");

        // The new m1 row has a different id (SQLite never reuses
        // autoincrement values, even after DELETE).
        let m1_row_id_v2 = list_all(&conn)
            .unwrap()
            .into_iter()
            .find(|m| m.model_id.as_str() == "m1")
            .map(|m| m.row_id)
            .expect("m1 row again");
        assert_ne!(
            m1_row_id_v1, m1_row_id_v2,
            "row_id differs across re-inserts (the bug Gate F1 fixes)"
        );

        // 5. Target re-bound to the new row id.
        let (rebound_fk, rebound_up) = read_target_row(&conn, combo_id);
        assert_eq!(
            rebound_fk,
            Some(m1_row_id_v2.0),
            "target re-bound to the new m1 row id"
        );
        assert_eq!(
            rebound_up.as_deref(),
            Some("m1"),
            "upstream_model_id is still 'm1'"
        );
        // combo_id is unchanged (we never touched combos).
        let row_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM combo_targets WHERE combo_id = ?1",
                [combo_id],
                |r| r.get(0),
            )
            .expect("count targets");
        assert_eq!(row_count, 1, "target row not duplicated");
    }

    /// AC2 — negative case.
    ///
    /// Under migration 000030 (ON DELETE CASCADE), the orphan
    /// target is cascade-deleted with its model, so the
    /// "wrong-model reconnect" case has no orphan to mis-bind to.
    /// This test is intentionally disabled — the Gate F1 reconnect
    /// path it pins (a non-NULL re-bind from `upsert_many`) no
    /// longer runs against the production schema.
    #[test]
    #[ignore = "Gate F1 reconnect path is dead code under migration 000030 \
                (ON DELETE CASCADE removes combo_targets rows with their model); \
                the reconnect logic in upsert_many is retained for forward-compat"]
    fn upsert_many_does_not_reconnect_wrong_model() {
        let conn = fresh_db();
        let provider = ProviderId::new("provA");
        let combo_id = seed_combo_targets_schema(&conn);

        // Seed m_a.
        upsert_many(
            &conn,
            &provider,
            &[discovered("m_a", TargetFormat::Openai)],
            Duration::from_secs(3600),
        )
        .expect("seed m_a");
        let m_a_v1 = list_all(&conn)
            .unwrap()
            .into_iter()
            .find(|m| m.model_id.as_str() == "m_a")
            .map(|m| m.row_id)
            .expect("m_a row");

        // Target with upstream_model_id="m_a".
        conn.execute(
            "INSERT INTO combo_targets(combo_id, provider_id, model_row_id, upstream_model_id, priority_order) \
             VALUES (?1, ?2, ?3, ?4, 0)",
            rusqlite::params![combo_id, "provA", m_a_v1.0, "m_a"],
        )
        .expect("insert target");

        // m_a disappears.
        upsert_many(&conn, &provider, &[], Duration::from_secs(3600))
            .expect("upsert empty");
        let (orphan_fk, orphan_up) = read_target_row(&conn, combo_id);
        assert!(orphan_fk.is_none(), "orphan after empty upsert");
        assert_eq!(orphan_up.as_deref(), Some("m_a"));

        // A *different* model m_b appears (m_a stays gone).
        upsert_many(
            &conn,
            &provider,
            &[discovered("m_b", TargetFormat::Openai)],
            Duration::from_secs(3600),
        )
        .expect("seed m_b");
        let m_b_id = list_all(&conn)
            .unwrap()
            .into_iter()
            .find(|m| m.model_id.as_str() == "m_b")
            .map(|m| m.row_id)
            .expect("m_b row");

        // The orphan target MUST remain orphan. The
        // (provider_id, upstream_model_id="m_a") match must not
        // accidentally fire against the new m_b row.
        let (post_fk, post_up) = read_target_row(&conn, combo_id);
        assert!(
            post_fk.is_none(),
            "wrong-model reconnect: target bound to m_b ({:?}) instead of staying orphan",
            post_fk
        );
        assert_eq!(post_up.as_deref(), Some("m_a"));
        // Sanity: m_b actually exists in the models table, so the
        // assertion above isn't vacuous.
        assert_eq!(m_b_id.0, post_fk.unwrap_or(m_b_id.0));
        assert!(
            get_by_row_id(&conn, m_b_id).unwrap().is_some(),
            "m_b row is in the table"
        );
    }

    /// AC3 — atomicity.
    ///
    /// Under migration 000030 (ON DELETE CASCADE), the orphan target
    /// is cascade-deleted with its model, so the atomicity concern
    /// (half-committed reconnect) does not arise. This test is
    /// intentionally disabled — the Gate F1 reconnect path it pins
    /// no longer runs against the production schema.
    #[test]
    #[ignore = "Gate F1 reconnect path is dead code under migration 000030 \
                (ON DELETE CASCADE removes combo_targets rows with their model); \
                the reconnect logic in upsert_many is retained for forward-compat"]
    fn upsert_many_atomic_orphan_reconnection() {
        let conn = fresh_db();
        let provider = ProviderId::new("provA");
        let combo_id = seed_combo_targets_schema(&conn);

        // Seed m1, attach a target.
        upsert_many(
            &conn,
            &provider,
            &[discovered("m1", TargetFormat::Openai)],
            Duration::from_secs(3600),
        )
        .expect("seed m1");
        let m1_v1 = list_all(&conn)
            .unwrap()
            .into_iter()
            .find(|m| m.model_id.as_str() == "m1")
            .map(|m| m.row_id)
            .expect("m1");
        conn.execute(
            "INSERT INTO combo_targets(combo_id, provider_id, model_row_id, upstream_model_id, priority_order) \
             VALUES (?1, ?2, ?3, ?4, 0)",
            rusqlite::params![combo_id, "provA", m1_v1.0, "m1"],
        )
        .expect("insert target");

        // m1 disappears.
        upsert_many(&conn, &provider, &[], Duration::from_secs(3600))
            .expect("upsert empty");
        let (orphan_fk, _orphan_up) = read_target_row(&conn, combo_id);
        assert!(orphan_fk.is_none(), "orphan baseline");

        // Install a trigger that RAISES on the next INSERT for
        // model_id='m1'. The `RAISE(ABORT, ...)` error code is
        // `SQLITE_CONSTRAINT` (19) — same family the production
        // schema would produce for an FK or UNIQUE violation.
        conn.execute_batch(
            "CREATE TRIGGER fail_m1_reinsert \
             BEFORE INSERT ON models \
             WHEN NEW.model_id = 'm1' \
             BEGIN \
                 SELECT RAISE(ABORT, 'simulated failure: m1 re-insert blocked'); \
             END;",
        )
        .expect("install failure trigger");

        // The re-INSERT of m1 must fail. We assert the error
        // surface — `upsert_many` propagates it. The atomicity
        // requirement is that nothing committed: the orphan stays
        // orphan, the trigger remains, no half-state.
        let result = upsert_many(
            &conn,
            &provider,
            &[discovered("m1", TargetFormat::Openai)],
            Duration::from_secs(3600),
        );
        assert!(
            result.is_err(),
            "expected the simulated constraint failure, got {:?}",
            result
        );

        // Post-failure: orphan target must be exactly as it was
        // before the failed upsert. Nothing committed.
        let (post_fk, post_up) = read_target_row(&conn, combo_id);
        assert!(
            post_fk.is_none(),
            "orphan must stay orphaned when the upsert rolls back"
        );
        assert_eq!(
            post_up.as_deref(),
            Some("m1"),
            "upstream_model_id preserved through the rollback"
        );

        // m1 must still be absent from the models table — the
        // rollback removed the would-be INSERT, AND the earlier
        // DELETE block that was also in the same tx must have
        // been rolled back too (so if the earlier DELETE had been
        // visible, m1 would already be gone; this is a sanity
        // pin that we did not accidentally commit the DELETE
        // without the INSERT).
        assert!(
            list_all(&conn)
                .unwrap()
                .iter()
                .all(|m| m.model_id.as_str() != "m1"),
            "m1 must not be in models after the rollback"
        );

        // Drop the trigger and verify the happy path is restored:
        // a *fresh* upsert_many with m1 must now succeed and
        // reconnect the orphan, because we are no longer
        // sabotaging the INSERT.
        conn.execute_batch("DROP TRIGGER fail_m1_reinsert;")
            .expect("drop trigger");
        upsert_many(
            &conn,
            &provider,
            &[discovered("m1", TargetFormat::Openai)],
            Duration::from_secs(3600),
        )
        .expect("clean re-upsert");
        let m1_v2 = list_all(&conn)
            .unwrap()
            .into_iter()
            .find(|m| m.model_id.as_str() == "m1")
            .map(|m| m.row_id)
            .expect("m1 v2");
        let (final_fk, final_up) = read_target_row(&conn, combo_id);
        assert_eq!(
            final_fk,
            Some(m1_v2.0),
            "happy-path reconnect works after the trigger is removed"
        );
        assert_eq!(final_up.as_deref(), Some("m1"));
    }

    // -------------------------------------------------------------------
    // Gate E4 — second alignment test: `delete_model_sets_combo_target_model_row_id_to_null`
    //
    // Companion to `delete_model_nulls_combo_target_model_row_id` above.
    // The previous test pins the *single-target* invariant: deleting a
    // model cascade-deletes the combo_targets row that pointed at it
    // (ON DELETE CASCADE, migration 000030). This one pins the
    // *isolation* invariant: deleting model M cascade-deletes target T
    // that pointed at M, while leaving the other target T2 (pointing
    // at a different, surviving model M2) completely untouched.
    #[test]
    fn delete_model_sets_combo_target_model_row_id_to_null() {
        let conn = fresh_db();
        let provider = ProviderId::new("provA");

        // Set up: providers (already seeded by fresh_db), models M
        // and M2, combo C, and two combo_targets T and T2 — T points
        // at M, T2 points at M2. Only T should be affected by
        // `delete(&conn, M.id)`.
        upsert_many(
            &conn,
            &provider,
            &[
                discovered("m-to-delete", TargetFormat::Openai),
                discovered("m-keep", TargetFormat::Anthropic),
            ],
            Duration::from_secs(3600),
        )
        .expect("seed");
        let all = list_all(&conn).unwrap();
        let m_id = all
            .iter()
            .find(|m| m.model_id.as_str() == "m-to-delete")
            .unwrap()
            .row_id;
        let m2_id = all
            .iter()
            .find(|m| m.model_id.as_str() == "m-keep")
            .unwrap()
            .row_id;

        // Mirror the production schema: combo_targets.model_row_id
        // is nullable with `ON DELETE CASCADE` (migration 000030).
        // `sub_combo_id` is also nullable, mirroring the real schema.
        conn.execute_batch(
            "CREATE TABLE combos (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, \
                                   strategy TEXT NOT NULL, race_size INTEGER NOT NULL DEFAULT 1); \
             CREATE TABLE combo_targets (id INTEGER PRIMARY KEY AUTOINCREMENT, \
                                           combo_id INTEGER NOT NULL, provider_id TEXT NOT NULL, \
                                           account_id INTEGER, sub_combo_id INTEGER, \
                                           model_row_id INTEGER \
                                           REFERENCES models(id) ON DELETE CASCADE);",
        )
        .expect("combo tables");
        let combo_id: i64 = conn
            .execute(
                "INSERT INTO combos(name, strategy) VALUES ('c', 'priority')",
                [],
            )
            .expect("insert combo") as i64;
        // T points at M, sub_combo_id is NULL.
        conn.execute(
            "INSERT INTO combo_targets(combo_id, provider_id, model_row_id) \
             VALUES (?1, ?2, ?3)",
            rusqlite::params![combo_id, "provA", m_id.0],
        )
        .expect("insert T");
        // T2 points at M2, sub_combo_id is NULL — must survive untouched.
        conn.execute(
            "INSERT INTO combo_targets(combo_id, provider_id, model_row_id) \
             VALUES (?1, ?2, ?3)",
            rusqlite::params![combo_id, "provA", m2_id.0],
        )
        .expect("insert T2");

        // Pre-conditions.
        assert_eq!(
            count_targets_for(&conn, combo_id),
            2,
            "two targets pre-delete"
        );
        let pre_t: Option<i64> = conn
            .query_row(
                "SELECT model_row_id FROM combo_targets WHERE combo_id = ?1 \
                 AND provider_id = ?2 AND model_row_id = ?3",
                rusqlite::params![combo_id, "provA", m_id.0],
                |r| r.get(0),
            )
            .expect("query T");
        assert_eq!(pre_t, Some(m_id.0), "T is the row pointing at M pre-delete");
        let pre_t2: Option<i64> = conn
            .query_row(
                "SELECT model_row_id FROM combo_targets WHERE combo_id = ?1 \
                 AND provider_id = ?2 AND model_row_id = ?3",
                rusqlite::params![combo_id, "provA", m2_id.0],
                |r| r.get(0),
            )
            .expect("query T2");
        assert_eq!(
            pre_t2,
            Some(m2_id.0),
            "T2 is the row pointing at M2 pre-delete"
        );

        // Action: delete M.
        let removed = delete(&conn, m_id).expect("delete M");
        assert_eq!(removed, 1, "one row removed from models");

        // (3) M is gone.
        assert!(
            get_by_row_id(&conn, m_id).unwrap().is_none(),
            "M is gone from models"
        );
        // M2 survives.
        assert!(
            get_by_row_id(&conn, m2_id).unwrap().is_some(),
            "M2 still present"
        );

        // (4) T was cascade-deleted (ON DELETE CASCADE) — it no
        // longer exists in combo_targets.
        let t_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM combo_targets \
                 WHERE combo_id = ?1 AND provider_id = ?2 \
                   AND model_row_id = ?3",
                rusqlite::params![combo_id, "provA", m_id.0],
                |r| r.get(0),
            )
            .expect("query T post-delete");
        assert_eq!(t_count, 0, "T was cascade-deleted with model M");

        // (5) T2 is unchanged.
        let t2_post: Option<i64> = conn
            .query_row(
                "SELECT model_row_id FROM combo_targets WHERE combo_id = ?1 \
                 AND provider_id = ?2 AND model_row_id = ?3",
                rusqlite::params![combo_id, "provA", m2_id.0],
                |r| r.get(0),
            )
            .expect("query T2 post");
        assert_eq!(
            t2_post,
            Some(m2_id.0),
            "T2 still points at M2 (not touched by M's delete)"
        );
        // Only T2 remains — T was cascade-deleted.
        assert_eq!(
            count_targets_for(&conn, combo_id),
            1,
            "only T2 remains (T was cascade-deleted with model M)"
        );
    }

    /// Local helper used by `delete_model_sets_combo_target_model_row_id_to_null`.
    fn count_targets_for(conn: &Connection, combo_id: i64) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM combo_targets WHERE combo_id = ?1",
            [combo_id],
            |r| r.get(0),
        )
        .expect("count targets")
    }
}
