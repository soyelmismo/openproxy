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
//!
//! # Module layout
//!
//! - **`crud`** — free functions for every SQL operation on the `models`
//!   table. These are the building blocks consumed by `SqliteModelRepository`.
//! - **`sync`** — diff computation, transactional upsert, and notification
//!   broadcasting used by [`upsert_many`].
//! - **`repository`** — [`ModelRepository`] trait and its SQLite
//!   implementation [`SqliteModelRepository`].
//! - **`discovery`** — [`DiscoveryService`] that orchestrates
//!   fetch → upsert → auto-activate.

use crate::error::{CoreError, Result};
use crate::ids::{ModelId, ModelRowId, ProviderId};
use serde::{Deserialize, Serialize};

// ── Submodules ──────────────────────────────────────────────────────
pub mod crud;
pub mod discovery;
pub mod repository;
pub mod sync;

#[cfg(test)]
mod tests;

// ── Re-exports ──────────────────────────────────────────────────────
// Keep the `models::function_name` call sites working across the
// crate without requiring callers to change to `models::crud::*`.
pub use crud::{
    apply_auto_activation, create_custom, delete, find_active_by_name,
    find_active_by_provider_and_name, get_by_row_id, list_active, list_active_all, list_all,
    mark_expired, set_active, set_active_bulk, set_test_status, upsert_many,
};

pub use discovery::DiscoveryService;
pub use repository::{ModelRepository, SqliteModelRepository};

// ── Domain types ────────────────────────────────────────────────────

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
            other => Err(CoreError::Validation(format!(
                "invalid target_format: {}",
                other
            ))),
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
