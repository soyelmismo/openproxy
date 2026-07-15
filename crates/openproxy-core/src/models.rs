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
pub use openproxy_types::{TargetFormat, DiscoveredModel, Model};

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
