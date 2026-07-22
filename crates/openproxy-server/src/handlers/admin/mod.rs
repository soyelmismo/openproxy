//! Admin HTTP handlers.
//!
//! Spec §2.3 enumerates the admin surface:
//!
//! - `GET    /admin/health`                          — process liveness.
//! - `*      /admin/providers[...]`                  — provider CRUD.
//! - `*      /admin/accounts[...]`                   — account CRUD
//!   (including `POST .../health` to force-set the health flag).
//! - `*      /admin/combos[...]`                     — combo CRUD
//!   (including `PATCH /:id` for `race_size` and
//!   `PATCH /:id/targets/:target_id` for `priority_order`).
//! - `*      /admin/usage/*`                         — usage analytics,
//!   plus `GET /usage/recent?since_id=N` for the dashboard's
//!   long-polling live tail.
//! - `POST   /admin/models/:id/refresh`              — model discovery refresh.
//! - `POST   /admin/models/:id/toggle`               — soft-disable a model.
//!
//! Each handler is a thin axum wrapper over the [`openproxy_core::admin`]
//! service layer; the service layer is the one that owns the SQL, and
//! these handlers just translate HTTP shapes (`Path<i64>`, `Query<T>`,
//! `Json<T>`) into the service calls and back into JSON responses.
//!
//! ## Path parameter types
//!
//! Provider ids are strings (e.g. `"openrouter"`) and so use
//! `Path<String>`; account and combo ids are `i64` and use
//! `Path<i64>`-or-better [`openproxy_core::ids::AccountId`] /
//! [`openproxy_core::ids::ComboId`] via the [`axum::extract::Path`]
//! `Deserialize` impls (those newtypes deserialize transparently from
//! the URL segment). Model row ids are also `i64`.
//!
//! ## `?` and `ApiResult`
//!
//! `ApiResult<T>` is a *newtype* (not the std `Result`) so the
//! `IntoResponse` impl can live in this crate — orphan rules prevent
//! impl-ing it on `Result<T, ApiError>` directly. The Rust 1.96 `Try`
//! trait is still nightly-gated, so `?` doesn't work on `ApiResult`
//! directly. Each handler body therefore runs in an inner `async {}`
//! block that returns the std `Result<T, ApiError>`, and we lift it
//! to `ApiResult<T>` via `From` at the very end.

pub mod providers;

pub mod accounts;
use openproxy_core::usage as core_usage;

pub mod api_keys;
pub mod combos;
pub mod debug;
pub mod models;
pub mod notifications;
pub mod oauth;
pub mod proxies;
pub mod runtime;
pub mod usage;

pub(crate) use auth::{admin_auth_middleware, authenticate_admin_ws};
pub(crate) use models::{resolve_adapter, run_test_for_model};
pub(crate) use oauth::refresh_oauth_if_needed;
pub(crate) use openproxy_db::combos as core_combos;
pub(crate) use openproxy_types::combos as types_combos;

pub mod auth;
use openproxy_core::accounts as core_accounts;
use openproxy_core::providers as core_providers;

use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use futures::StreamExt;
use openproxy_adapters::adapters;
use openproxy_core::{
    analytics, api_keys as core_api_keys,
    config::{CircuitBreakerConfig, RacingConfig, RetriesConfig, TimeoutsConfig},
    models as core_models, oauth as core_oauth, seed,
    usage::UsageFilter,
};
use openproxy_db as core_db;
use openproxy_db::conn::ADMIN_LOCK_TIMEOUT;
use openproxy_types::{
    CoreError,
    ids::{
        AccountId, ApiKeyId, ComboId, ComboTargetId, ModelRowId, ProviderId, RequestId, TraceId,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;

use crate::{
    error::{ApiError, ApiResult},
    state::AppState,
};

// Resolve the adapter for a given provider.
//
// First checks the built-in adapter registry. If no built-in adapter
// matches, falls back to loading the provider row from the DB and
// constructing a [`adapters::CustomAdapter`]. Returns `Err` only when
// the provider doesn't exist in the DB at all.

// Optional filters shared by all `GET /admin/usage/*` endpoints.
//
// All fields are `Option<_>` so a request with no query string is
// valid and means "no filter". Strings are forwarded verbatim into
// `UsageFilter`; date bounds are expected to be ISO-8601 (the spec
// calls this out explicitly).

// Parse a `from` or `to` timestamp from the dashboard into the
// canonical RFC-3339 form the SQL builder expects. Returns a
// 400-style [`CoreError::Validation`] on malformed input.

// Format a UTC `DateTime` as the canonical ISO-8601 string the SQL
// builder expects (e.g. `2026-06-18T07:00:00Z`).

// Resolve a `preset` query parameter into `(from, to)` UTC timestamps.
//
// Returns `None` when `preset` is `None`, `Some(None)` when the preset
// is recognized but explicitly opts out of date filtering (none of the
// current presets do this, but the `custom` sentinel means "use the
// explicit `from`/`to` as-is").
//
// Returns `Err` for unknown preset strings so the operator sees a 400
// instead of silently falling back to "no filter" (which would return
// the wrong data and confuse debugging).

// =====================================================================
// Health
// =====================================================================

// `GET /admin/health` — process liveness with a version tag.

// =====================================================================
// Runtime configuration (read-only view of the parsed AppConfig)
// =====================================================================

/// Read-only view of the relevant `AppConfig` sections.
///
/// Surfaced to the dashboard as a single JSON envelope so the UI can
/// render the current values without five round-trips. The shape
/// intentionally mirrors the `[timeouts]` / `[retries]` /
/// `[circuit_breaker]` / `[racing]` blocks of `config.example.toml`
/// so the operator can copy the values back into the file verbatim.
///
/// **Variant A of the "Config" menu** — these values are read once
/// at startup from `config.toml` (with `OPENPROXY_*` env overrides);
/// they cannot be mutated from this endpoint. The dashboard's
/// `#/config` view is honest about this and shows the inputs as
/// disabled with a banner explaining "edit `config.toml` and
/// restart". When/if we ever want hot-reload (variant B), this
/// struct is the read-side of that contract — adding a `PUT`
/// companion is a forward-compatible change.
#[derive(Debug, Clone, Serialize)]
pub struct RuntimeConfigResponse {
    pub timeouts: TimeoutsConfig,
    pub retries: RetriesConfig,
    pub circuit_breaker: CircuitBreakerConfig,
    pub racing: RacingConfig,
    /// Lifetime in seconds for recorded request/response bodies and
    /// headers. `0` means bodies are pruned immediately on the next
    /// prune tick.
    pub recording_ttl_secs: i64,
    pub compression: openproxy_compression::CompressionMode,
    /// When true, idle_chunk timeouts are treated as retryable
    /// (pipeline falls through to the next target).
    pub idle_chunk_retryable: bool,
    pub quota_protection: openproxy_types::config::QuotaProtectionConfig,
}

// `GET /admin/config` — return the currently-loaded runtime
// configuration (timeouts, retries, circuit-breaker, racing).
//
// This is a thin, side-effect-free snapshot of `AppState::config()`:
// no DB, no DB writer, no I/O. The auth check mirrors
// `get_recording` / `usage_*` because the operator might consider
// these values sensitive (they leak upstream connection budgets).

// `PUT /admin/config/timeouts` — hot-reload the system default
// timeouts. Body is a full [`TimeoutsConfig`] (5 `u64` fields, all
// required). On success the value is persisted in the `app_config`
// table and the in-memory `timeouts_cell` slot is updated. Future
// chat requests see the new values; requests already in flight keep
// the previous value (consistent with the per-pipeline
// `PipelineConfig::defaults` snapshot taken in `chat.rs:201-203`).
//
// **Auth**: same as `get_runtime_config` — `manage` scope via
// `authenticate_admin_ws`.
//
// **Validation**: structural only. serde rejects missing or
// wrong-type fields; we do not check business ranges (a zero is
// allowed, matching the current `TimeoutsConfig` policy).
//
// **Side-effect order**: DB first, memory second. If the DB UPSERT
// fails, the response is 500 and the in-memory value is unchanged.

// `PUT /admin/config/compression` — hot-reload the compression
// mode. Body: `{"mode": "off" | "lite" | "rtk"}`.

// =====================================================================
// Idle chunk retryable
// =====================================================================

// `PUT /admin/config/idle-chunk-retryable` — hot-reload the
// `idle_chunk_retryable` flag. Body: `{"idle_chunk_retryable": true}`
// or `{"idle_chunk_retryable": false}`.
//
// When true, idle_chunk timeouts are treated as retryable: the
// pipeline falls through to the next target instead of aborting.
// When false (default), idle_chunk timeouts return an error
// immediately and the walk is aborted.

// =====================================================================
// Quota protection
// =====================================================================

// `PUT /admin/config/quota-protection` — hot-reload the `quota_protection`
// config. Body: `{"enabled": true, "threshold_percentage": 10}`.

// =====================================================================
// Recording TTL
// =====================================================================

// `GET /admin/config/maintenance` — read the current maintenance
// config (auto_vacuum, vacuum_interval_hours, usage_retention_days)
// and the VACUUM status (last_run, in_progress, next_scheduled).

// `PUT /admin/config/maintenance` — update the maintenance config at
// runtime. Body: `{ "auto_vacuum": bool, "vacuum_interval_hours": u32,
// "usage_retention_days": u32 }`. All fields are optional — missing
// fields keep their current value. Changes take effect on the next
// background tick (no restart needed).

// `GET /admin/config/vacuum-status` — read the current VACUUM status
// (last_run, last_result, in_progress, next_scheduled). Polled by
// the dashboard's config view button.

// `GET /admin/config/recording-ttl` — read the current recording body
// TTL in seconds.

// `PUT /admin/config/recording-ttl` — hot-reload the recording body
// TTL. Body: `{"recording_ttl_secs": N}` where `N` is the lifetime in
// seconds for recorded request/response bodies and headers.

// =====================================================================
// Providers
// =====================================================================

#[derive(serde::Serialize)]
pub struct ProviderWithOAuth {
    #[serde(flatten)]
    pub provider: core_providers::Provider,
    pub oauth_flows: Option<Vec<String>>,
    pub metadata: openproxy_core::providers::ProviderMetadata,
    pub active_models: i64,
    pub total_models: i64,
}

// `GET /admin/providers` — list all providers.

// `POST /admin/providers` — create a provider.

// `GET /admin/providers/:id` — fetch a single provider.

// `DELETE /admin/providers/:id` — delete a provider. Idempotent
// for custom providers.
//
// Built-in providers (the ones seeded on first run — see
// [`openproxy_core::seed::builtin_provider_ids`]) are rejected
// with a 400 (Validation) instructing the operator to use
// `POST /admin/providers/:id/active` to deactivate the
// provider instead. Built-ins are protected because removing
// the row would leave dangling references in the adapter
// registry, and the operator can get the "stop using this
// provider" effect cheaply and reversibly via the
// deactivate flag.
//
// The guard is implemented in two places by design: this
// handler does a fast pre-check on the URL id so the DB write
// is never attempted, and [`openproxy_core::admin::delete_provider`]
// repeats the check on the typed id. Either one alone is
// sufficient for correctness; both makes the rejection
// observable from any future caller of the service layer.

// `POST /admin/providers/:id/active` — flip the soft-disable flag
// on a provider. Body: `{"active": true|false}`. Returns the new state.
//
// This is the dashboard's "Deactivate" / "Activate" button path. A
// deactivated provider stays in the DB (accounts and models
// preserved) and can be reactivated later. While deactivated, combo-
// target lookups skip it; the pipeline surfaces `NoHealthyTargets`
// when a combo has no active providers left.
//
// Missing id is a silent no-op (matches the rest of the providers
// helpers), so the dashboard's optimistic refetch never has to
// special-case a 404.

// =====================================================================
// Accounts
// =====================================================================

/// Query string for `GET /admin/accounts` — supports `?provider_id=...`.
#[derive(Debug, Default, Deserialize)]
pub struct AccountListQuery {
    pub provider_id: Option<String>,
}

// `GET /admin/accounts` — list accounts, optionally filtered by provider.

// `POST /admin/accounts` — create an account. `api_key` is encrypted
// before insertion; the response only echoes the new id.

// `DELETE /admin/accounts/:id` — delete an account by numeric id. Idempotent.

// =====================================================================
// Combos
// =====================================================================

// `GET /admin/combos` — list all combos.

// `POST /admin/combos` — create a combo. `race_size` defaults to 1.

// `GET /admin/combos/:id` — fetch a single combo.

// `POST /admin/combos/:id/test-all` — fan-out a test request to
// every target of a combo and return a list of per-target results.
//
// The handler:
//
// 1. Loads the combo's targets via
//    [`openproxy_core::combos::list_targets_with_model`] (which
//    already LEFT-JOINs `target_cooldowns`, so the
//    `in_cooldown` flag is populated for free).
// 2. For each target:
//    - If it is a sub-combo target, mark it as `skipped` with
//      `error_msg = "sub-combo; test children individually"`. The
//      actual children are reachable via the parent combo's id in
//      the dashboard.
//    - If it is currently parked in `target_cooldowns`, mark it as
//      `skipped` with `error_msg = "in_cooldown"` so the operator
//      can see *why* we didn't fire a real request.
//    - Otherwise, call [`run_test_for_model`] to actually ping the
//      upstream and capture the live status.
//
// The shape is intentionally compatible with the per-row result
// returned by [`test_model`] (the same `status` / `elapsed_ms` /
// `error_msg` fields), so the UI's renderer doesn't have to
// special-case the two endpoints.
//
// The wall-clock cost is bounded by a 180 s timeout (see the
// `tokio::time::timeout` wrap below) and by a per-target 15 s
// timeout inside [`run_test_for_model`]. With ~8 targets and a
// stuck upstream, the endpoint may still take ~2 minutes — the
// dashboard button flips to "🧪 Testing..." for the duration.

// `DELETE /admin/combos/:id` — delete a combo. Idempotent; cascade
// removes its targets.

// `GET /admin/combos/:id/targets` — list a combo's targets in
// `(priority_order ASC, id ASC)` order, enriched with the model's
// upstream id and human-readable display name so the dashboard
// doesn't have to do a per-row roundtrip.
//
// Returns [`core_combos::ComboTargetWithModel`] (superset of
// [`core_combos::ComboTarget`]); the extra fields are `model_id` (upstream
// id, e.g. `"anthropic/claude-3.5-sonnet"`) and `model_display_name`
// (the row's `display_name`, or `None` if unset).

// `POST /admin/combos/:id/targets` — add a target to a combo.

// `GET /admin/combos/:id/valid-sub-combos` — list combos that
// can be added as a sub-combo target of `:id` (i.e. excluding the
// combo itself and any combo whose addition would close a cycle).
// Drives the "Add sub-combo target" picker in the dashboard.

// =====================================================================
// Usage analytics
// =====================================================================

// Helper: run an analytics query with the filter, retry on disk I/O
// error. The retry does a `PRAGMA wal_checkpoint(TRUNCATE)` on the
// writer connection (to flush the WAL and release any locked pages)
// and then retries the query on a FRESH reader connection (the old
// reader may have a stale page cache that references the corrupt page).
//
// This addresses the "disk I/O error" the user sees on analytics
// endpoints with large/fragmented DBs. The root cause is typically
// WAL checkpoint contention or stale page cache on the long-lived
// reader connection — the retry + checkpoint clears both.

// `GET /admin/usage/summary` — top-line roll-up.

// `GET /admin/usage/by-model` — per-(provider, model) breakdown.

// `GET /admin/usage/by-provider` — per-provider breakdown.
//
// Mirrors `usage_by_model` but groups by `provider_id` only. The
// frontend's "monthly usage by provider" report uses this for the
// top-level roll-up and `usage_monthly_by_provider` for the
// time-bucketed breakdown.

// `GET /admin/usage/monthly-by-provider` — per-(provider, month)
// breakdown.
//
// `month` is `strftime('%Y-%m', created_at)`. Ordered by
// `month ASC, total_cost_usd DESC` so the frontend can pivot into a
// providers × months matrix that walks time forward.

// `GET /admin/usage/by-day` — daily usage totals for charting.

// `GET /admin/usage/by-account` — per-account breakdown.

// `GET /admin/usage/by-status` — counts grouped by HTTP status code.

/// Cap on inline error rows. Spec §7.2 says "the most recent 100".
const ERRORS_DEFAULT_LIMIT: u32 = 100;

// `GET /admin/usage/errors` — recent error rows, newest first.

// `GET /admin/usage/latency` — p50/p95 across connect/ttft/total/tokens_per_sec.

// `GET /admin/usage/races` — race outcome statistics.

// =====================================================================
// Model refresh
// =====================================================================

/// Query string for `POST /admin/models/:id/refresh` — lets the caller
/// override the refresh TTL in seconds and pin a specific account.
#[derive(Debug, Default, Deserialize)]
pub struct RefreshQuery {
    /// Cache TTL in seconds for the discovered rows. Defaults to 1 hour.
    pub ttl_seconds: Option<i64>,
    /// Account id whose API key will be used. Required when the provider
    /// has more than one account; otherwise the first account wins. The
    /// API key is decrypted on the fly and is never logged or echoed.
    pub account_id: Option<i64>,
}

// `POST /admin/models/:id/refresh` — re-discover models for the
// provider that owns the given `model_row_id`.
//
// The handler:
// 1. Loads the model row to find its provider.
// 2. Resolves the provider's adapter from the in-process registry.
// 3. Picks an account (the explicit `account_id` query param, or the
//    first account of the provider) and decrypts its API key.
// 4. Calls [`core_admin::refresh_models`], which fetches the upstream's
//    `/models` endpoint and upserts the results.
//
// On success, returns the number of rows touched.
// `POST /admin/models/sync-models-dev` — one-shot sync from models.dev.

// `POST /admin/usage/recompute-costs` — re-price historical usage
// rows that have `cost_usd = 0` AND `prompt_tokens > 0`. This walks
// every unpriced row, re-applies `pricing::lookup_with_db` (which
// consults the sync table + static table), and updates `cost_usd`.
//
// Use this after a models.dev sync populates new pricing data, or
// after manually setting pricing, to backfill costs for rows that
// were recorded before pricing was available.

// The body of [`refresh_models`], factored out so the async state
// machine (which holds a `parking_lot::MutexGuard` across an await)
// doesn't entangle the handler's own future.

// =====================================================================
// Live-tail usage (long-polling)
// =====================================================================

/// Default cap for `GET /admin/usage/recent` when the client omits
/// `?limit=`. Mirrors the spec's "up to 50 rows per poll" guidance.
const USAGE_RECENT_DEFAULT_LIMIT: u32 = 50;

/// Hard cap on `?limit=` so a misbehaving client can't pull the whole
/// table in a single request.
const USAGE_RECENT_MAX_LIMIT: u32 = 500;

// Sanity cap on `?since_id=`. The `usage.id` PK is autoincrement i64;
// a legitimate client polls forward from "the last id it has seen",
// which is bounded by the highest value the server has ever produced.
// A request passing a `since_id` larger than this is either a bug or
// malicious — clamp instead of forwarding, so the SQL plan stays
// index-driven (`WHERE id > ?1` on the PK) and the response is empty
// rather than scanning garbage.
const USAGE_RECENT_MAX_SINCE_ID: i64 = i64::MAX / 2;

/// Query string for `GET /admin/usage/recent`.
///
/// `since_id` is the largest `usage.id` the caller has already seen; the
/// handler returns every row whose `id` is strictly greater. `limit`
/// caps the page size (default [`USAGE_RECENT_DEFAULT_LIMIT`], max
/// [`USAGE_RECENT_MAX_LIMIT`]).
#[derive(Debug, Default, Deserialize)]
pub struct RecentQuery {
    pub since_id: Option<i64>,
    pub limit: Option<u32>,
}

// `GET /admin/usage/recent?since_id=N&limit=K` — long-polling tail of
// the `usage` table.
//
// The dashboard polls this endpoint on a short timer, passing back the
// `id` of the last row it has rendered. The handler returns the next
// page of rows (oldest-first, by `id`) so the client can append them in
// order. This is the pragmatic alternative to an SSE channel called
// out by the spec; it needs no new dependencies and gives the same
// "live tail" effect for the dashboard.

#[derive(Debug, Deserialize)]
struct ClientWsMessage {
    #[serde(rename = "type")]
    msg_type: String,
    since_id: Option<i64>,
}

/// Outcome of one poll of the optional notifications broadcast receiver
/// inside `stream_usage_rows` (F2). The receiver is `Option<...>` (the
/// channel may not be initialized in tests), so we wrap the recv result
/// in this enum and match on it in the `select!` arm. `Closed` is
/// separate from `Lagged` because we want to drop the receiver (set
/// the Option to None) on Closed without breaking the WS connection —
/// the stage/usage channels may still be live.
enum NotifRxEvent {
    Event(openproxy_core::notifications::NotificationEvent),
    Lagged(u64),
    Closed,
}

// Axum middleware wrapper around [`authenticate_admin_ws`].
//
// Runs on every request that flows through the admin router
// (registered via `axum::middleware::from_fn_with_state` in
// `router.rs`). On success it forwards to the inner handler; on
// failure it short-circuits with a 401 from the same
// [`ApiError::IntoResponse`] impl the per-handler calls use, so
// the wire shape (`{"error": {"code", "message"}}`) is identical
// to the per-handler path.
//
// The middleware does NOT touch the `?token=` query parameter on
// its own — the WebSocket upgrade handler (`usage_stream`) needs
// that path, and the per-handler `authenticate_admin_ws` already
// accepts the query token. The middleware reads only the
// `Authorization` header, which is the contract for the HTTP
// path. WebSocket clients that pass `?token=` get a single auth
// call inside `usage_stream` after the upgrade completes.

/// Capacity of the bounded mpsc channel that decouples the broadcast
/// receiver loop from the WS sender task. The receiver loop forwards
/// every broadcast event into this mpsc; the dedicated sender task
/// drains it and calls `socket.send().await`.
///
/// This is the CRITICAL fix for the "second request doesn't appear
/// in real-time after a failure" bug. Previously the select! loop
/// awaited `socket.send()` directly, which blocks when the browser's
/// WS write buffer fills up (e.g. during a slow full-DOM rebuild).
/// While blocked, neither `usage_rx.recv()` nor `stage_rx.recv()` was
/// polled, so the broadcast channels' buffers filled up and started
/// dropping the OLDEST events for this receiver — including the
/// `started`/`connecting` stage events of subsequent requests. Those
/// requests then appeared "stuck" until their terminal usage row
/// landed, which is exactly the symptom the user reported.
///
/// With the mpsc in between, the select! loop's only job is to
/// `try_send` into the mpsc (which is bounded but never blocks the
/// receiver loop for long — `try_send` returns immediately with
/// `TrySendError::Full`, which we handle by counting the dropped
/// message and emitting a lag_warning). The dedicated sender task
/// owns the only `socket.send` calls, so a slow browser stalls the
/// sender task but NOT the receiver loop. Broadcast events keep
/// being drained into the mpsc buffer (capacity below), which buys
/// the browser time to catch up without dropping anything.
///
/// 2048 × ~256 B (avg WS message size) ≈ ~512 KB — a trivial amount
/// of memory that buys us minutes of browser-side stall tolerance.
/// Was 512 (128 KB), which was too small for bursts of stage events
/// during multi-target races or rapid-fire streaming. When the outbox
/// filled, the receiver loop blocked on `send().await` (after the
/// fix), which is correct — but a larger buffer means fewer blocking
/// events and smoother real-time delivery.
const WS_OUTBOX_CAPACITY: usize = 2048;

// Forward a JSON value into the bounded outbox mpsc. If the outbox
// is full (sender task is stalled on a slow browser), we try to
// make room by waiting briefly — this is CRITICAL for stage events
// which carry the "in progress" status. Dropping them silently
// makes the dashboard miss real-time updates, which is the exact
// bug the user reported ("peticiones no llegan en tiempo real").
//
// We use `send().await` (bounded) instead of `try_send()` (non-blocking)
// for STAGE events and ROW events, because these are the real-time
// updates the operator needs. If the outbox is genuinely stuck
// (browser dead for seconds), the broadcast channel will lag and
// trigger a resync — but for the common case of a 10-50ms render
// stall, waiting is far better than dropping.
//
// For non-critical messages (pong, error, lag_warning), we still
// use `try_send` so the receiver loop never blocks on unimportant
// traffic.

// Same as `outbox_send` but uses `try_send` (non-blocking) for
// non-critical messages where dropping is acceptable (pong, error,
// lag_warning). This prevents the receiver loop from blocking on
// unimportant traffic when the browser is slow.

/// Query for WebSocket token in `/admin/usage/stream`
#[derive(Debug, Default, Deserialize)]
pub struct UsageStreamQuery {
    pub token: Option<String>,
}

// `GET /admin/usage/stream` — upgraded WebSocket handler.

// `GET /admin/usage/detail?id=<usage_id>&trace_id=<trace_id>` — full detail for a single usage row.

/// Query for `GET /admin/usage/detail`.
#[derive(Debug, Default, Deserialize)]
pub struct DetailQuery {
    pub id: Option<i64>,
    pub trace_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct UsageDetailResponse {
    pub row: core_usage::UsageDetailRow,
}

// =====================================================================
// Recording toggle (Live Logs detail modal)
// =====================================================================

// `GET /admin/recording` — read the current recording state.
//
// Returns `{"recording": true|false}`. Used by the dashboard's Live
// Logs section to render the "Record" toggle on initial load.

// `POST /admin/recording` — flip the process-wide recording state.
//
// Body: `{"enabled": true|false}`. When enabled, every new chat
// request will record the full request/response bodies and headers
// in the `usage` table. Useful for debugging from the Live Logs
// detail modal: turn it on, fire a few requests, click into a row
// to see the actual JSON.

// Model toggling
// =====================================================================

// `POST /admin/models/:id/toggle` — flip the soft-disable bit on a
// model row. Body: `{"active": true|false}`. Returns the new state.
//
// An unknown id is a silent no-op (`set_active` doesn't error); the
// dashboard will simply see no change.

// `POST /admin/models/bulk-toggle` — flip the soft-disable bit on
// every non-custom model of a provider in a single SQL UPDATE.
//
// Body: `{"provider_id": "...", "active": true|false}`.
//
// Returns `{"updated": N}` where `N` is the number of rows actually
// changed (i.e. the count of non-custom rows that were not already
// in the target state). Custom rows are skipped — same policy as
// `apply_auto_activation` — so an operator's hand-picked entries
// survive a bulk toggle.
//
// This is the dashboard's "Enable all" / "Disable all" path. Doing
// it as a single statement (instead of iterating the per-row
// `toggle_model` endpoint N times) closes a race window where a
// concurrent `apply_auto_activation` could re-activate rows
// mid-iteration and leave the table half-toggled.

// `DELETE /admin/models/:id` — hard-delete a model row.
//
// Companion to [`toggle_model`]: that endpoint hides a row from
// routing while preserving the audit trail; this one removes the row
// outright. Combo-targets referencing the model are preserved with
// `model_row_id = NULL` (migration 000025 `ON DELETE SET NULL`); they
// are filtered from routing by `core_combos::list_targets` (Gate E3). The
// row is kept in the table for audit / re-binding.
//
// A missing id is a silent no-op — `core_models::delete` reports 0 rows
// removed and we surface that as `{"deleted": 0}` so the dashboard can
// distinguish "row was actually removed" from "row was already gone".

// =====================================================================
// Combo mutations
// =====================================================================

// `PATCH /admin/combos/:id` — partial update of a combo row.
//
// Recognized body fields (all optional — absent fields are left
// untouched):
//
// - `race_size`: `1..=8`. Out-of-range is a 400.
// - `context_window`: `null` or an integer. `null` means
//   "auto-compute from targets".
// - `priority_mode`: `"strict"` | `"lkgp"` | `"weighted"` |
//   `"least_used"` | `"p2c"`. `null` clears the column back to
//   the legacy `strict` default. Ignored for `RoundRobin` /
//   `Shuffle` strategies (stored but not consulted).
// - `cooldown_mode`: `"flat"` | `"exponential"`. `null` clears
//   the column back to the legacy `flat` default.
// - `cooldown_base_secs` / `cooldown_max_secs` / `cooldown_factor`:
//   per-combo overrides for the cooldown formula. `null` clears
//   each one back to "use the global `[cooldown]` default".
//   These three fields are written in a single UPDATE so the
//   dashboard's "Cooldown" form can POST them atomically.
// - `lkgp_exploration_rate`: float in `[0.0, 1.0]`. `null`
//   clears the column back to the default 0.1.
// - `selection_window_secs`: positive integer. `null` clears the
//   column back to the default 3600.

// `PATCH /admin/combos/:id/targets/:target_id` — update mutable
// fields of a single target. Recognized body fields (all optional —
// absent fields are left untouched):
//
// - `priority_order`: `i32`. The caller picks a sane value relative
//   to siblings; we don't re-number the rest of the rowset here.
// - `weight`: positive `i32`. Per-target weight for the `weighted`
//   priority mode (migration 000035). Default 1; weights `<= 0`
//   are rejected with a 400.
//
// For backwards compatibility, the legacy single-field form
// `{"priority_order": <i32>}` is still accepted (and required when
// `weight` is absent). The dashboard's combo editor upgrades the
// call to include both fields when the operator is editing the
// weight column.

// `DELETE /admin/combos/:id/targets/:target_id` — remove a single
// target from a combo. The handler validates that the target actually
// belongs to the requested combo (defense in depth: a mismatched URL
// surfaces as a 400 instead of silently deleting from another combo).

// `POST /admin/combos/:id/targets/:target_id/clear-cooldown`
// — force-clear the persistent cooldown row for a single target.
// See [`openproxy_core::admin::clear_combo_target_cooldown`].
//
// The dashboard's "Reset cooldown" button calls this when the
// operator has manually verified the upstream is healthy again
// and wants to short-circuit the `cooldown_secs` wait. The
// handler does not write to `models.last_test_status` (the row
// might be parked because of a 5xx on the *combo target* layer,
// not on the model itself).
//
// IMPORTANT: this literal-segment route MUST be registered
// before `/admin/combos/:id/targets/:target_id` in
// `router.rs`, otherwise axum's :target_id segment will happily
// swallow `clear-cooldown` and 405 the POST.

// =====================================================================
// Combo target reorder
// =====================================================================

/// Body for `POST /admin/combos/:id/targets/reorder`. The frontend's
/// ↑/↓ buttons compute the new order client-side (swap the moved
/// target with its neighbor) and post the full ordered list back; the
/// backend renumbers everything in a single transaction.
#[derive(Debug, Deserialize)]
pub struct ReorderComboTargetsInput {
    pub target_ids: Vec<i64>,
}

// `POST /admin/combos/:id/targets/reorder` — atomically reassign
// `priority_order` for every target of a combo so the new order
// matches `body.target_ids`.
//
// The backend does the swap, not the caller: an "old + delta" PATCH
// would leave a half-swapped state on disk for the duration of two
// HTTP calls and could leave two targets with the same
// `priority_order` if the calls interleave. Doing the renumber in
// a single `IMMEDIATE` transaction closes both holes.
//
// `target_ids` must be a permutation of the combo's current target
// ids; otherwise the call fails with a 400 and the combo is left
// untouched.
//
// Follow-up (not implemented in this pass): nested combos. Allowing
// a target to reference another combo (combo-of-combos) would need
// a new column on `combo_targets` (or a separate join table), a
// dedicated `combo:<name>` resolver in the routing layer, and an
// updated auto-populate path. Out of scope for the dashboard
// "reorder" fix and tracked as a phase-2 item.

// =====================================================================
// Provider update
// =====================================================================

// `PATCH /admin/providers/:id` — partial update of a provider row.
//
// Body: any subset of `name`, `base_url`, `extra_headers_json`,
// `auto_activate_keyword`. The keyword uses a three-state encoding:
// * `null`/`absent` — leave the column alone.
// * `{"auto_activate_keyword": null}` — clear the column to `NULL`.
// * `{"auto_activate_keyword": "claude"}` — set the column to `"claude"`.

// =====================================================================
// Custom model creation
// =====================================================================

// `POST /admin/models/custom` — hand-create a model row. The row is
// stamped with `custom = 1` and `active = 1` so it is routable as soon
// as the call returns. Use this when a model is missing from the
// provider's `/models` endpoint but the operator knows the upstream
// will accept it anyway.
//
// Body: `{ "provider_id": "...", "model_id": "...", "display_name":
//         "...", "target_format": "openai"|"anthropic", "ttl_seconds": N }`.

// =====================================================================
// Model test
// =====================================================================

/// Maximum number of characters from a failing response body that we
/// surface back to the dashboard. Long upstream error pages are
/// truncated; the test handler is not a debugger.
const TEST_ERROR_BODY_MAX_CHARS: usize = 512;

/// The outcome of a single test ping. Used by both [`test_model`]
/// (the per-row handler) and [`test_combo_targets`] (the per-combo
/// fan-out). The shape is intentionally close to the JSON the
/// handlers return to the dashboard so callers can lift it into a
/// `serde_json::Value` without an extra struct -> value conversion.
///
/// `row_id` is informational: it identifies the *model* row that
/// was pinged. The combo handler overlays it with the *target* id
/// in its response so the frontend can cross-reference the row in
/// the targets table.
///
/// `skipped = true` means the helper did not actually fire an HTTP
/// request. `skip_reason` is a short human-readable string the
/// dashboard can show in the `error_msg` column. The two boolean
/// fields are mutually exclusive in practice: a real attempt has
/// `skipped = false` regardless of HTTP status (4xx is a real
/// attempt that failed), and a skipped attempt has no `status`
/// from the upstream.
#[derive(Debug, Clone)]
pub struct TestResult {
    pub row_id: i64,
    pub status: u16,
    pub elapsed_ms: u64,
    pub error_msg: Option<String>,
    pub skipped: bool,
    pub skip_reason: Option<String>,
}

impl TestResult {
    fn skipped(row_id: i64, reason: impl Into<String>) -> Self {
        let r: String = reason.into();
        Self {
            row_id,
            status: 0,
            elapsed_ms: 0,
            error_msg: Some(r.clone()),
            skipped: true,
            skip_reason: Some(r),
        }
    }
}

/// Knobs that distinguish the per-row test path from the
/// per-combo fan-out, without resorting to thread-local state.
///
/// `in_combo_fanout = true` flips two behaviors:
///
/// 1. Inactive models are *skipped* instead of being pinged. The
///    per-row path lets the operator ping an inactive model (they
///    may be debugging why a model was toggled off), but a fan-out
///    should never bombard a model the operator has explicitly
///    deactivated.
/// 2. The `last_test_status` side-effect on the model row is
///    skipped. The combo's "test all" is a transient probe, not a
///    model-level quality stamp; overwriting the row's status with
///    a fan-out result would be misleading.
#[derive(Debug, Clone, Copy, Default)]
pub struct TestOptions {
    pub in_combo_fanout: bool,
}

// Core of the test flow: load the model, pick (or accept) an
// account, decrypt the API key, build a minimal "ping" request,
// fire it upstream with a 15 s timeout, capture the status and
// elapsed milliseconds, and (only for the per-row path) persist
// `last_test_status` on the model row.
//
// The helper is called by both [`test_model`] (the per-row POST
// handler) and [`test_combo_targets`] (the per-combo fan-out).
// The behavior of the two call sites diverges only in the
// [`TestOptions`] they pass — the underlying HTTP plumbing is the
// same.

/// `POST /admin/models/:id/test` — send a tiny "ping" request to the
/// model and stamp the result onto its row.
///
/// Thin wrapper over [`run_test_for_model`]. The helper is the
/// real implementation; this handler exists for backwards
/// compatibility with the dashboard's "Test" button on the model
/// row, which expects the same `last_test_status` side-effect
/// the original handler had.
#[derive(serde::Deserialize, Default)]
pub struct TestModelInput {
    pub account_id: Option<i64>,
    pub proxy_id: Option<String>,
}

// =====================================================================
// Account health
// =====================================================================

// `POST /admin/accounts/:id/health` — force-set an account's
// health flag. Body: `{"health": "healthy"|"degraded"|"unhealthy"}`.
//
// Bypasses the runtime's automatic health tracking so an operator can
// manually re-enable an account after fixing it (or take it offline
// during an incident).

// `PUT /admin/accounts/:id/api-key` — encrypt and store a new API
// key for an existing account (or clear it by passing `null`).
//
// Body: `{"api_key": "sk-..."}` or `{"api_key": null}`.
//
// The plaintext is encrypted with the server's master key and stored as
// a BLOB. Returns 404 when the account does not exist.

// =====================================================================
// Admin model listing (internal shape, includes row_id + active)
// =====================================================================

/// `GET /admin/models` — every row in the `models` table, in the
/// internal `Model` shape.
///
/// The public `GET /v1/models` projects each row down to an OpenAI-shaped
/// payload (no `row_id`, no `active`) so SDKs that expect the OpenAI
/// contract can consume it. The dashboard, on the other hand, needs the
/// extra fields to drive the toggle and refresh buttons; this endpoint
/// returns them. There is no filter — the dashboard's model list is
/// small enough that a single shot is fine.
#[derive(serde::Deserialize)]
pub struct ListModelsQuery {
    pub provider_id: Option<String>,
}

// =====================================================================
// Account quota refresh
// =====================================================================

// `POST /admin/accounts/:id/refresh-quota` — fetch a fresh quota
// snapshot for a single account and persist it.
//
// Flow:
// 1. Look up the account to find its `provider_id`.
// 2. If the provider has no quota fetcher implemented (anything other
//    than `minimax` / `minimax-cn` in the MVP), short-circuit with a
//    `{"supported": false, "message": "..."}` body. The HTTP status is
//    still 200 — the call did not fail, it just isn't meaningful for
//    this provider — so the dashboard can render the response inline.
// 3. Decrypt the API key.
// 4. Fire the upstream quota call. On success, stamp the result onto
//    the row. On failure, stamp a quota row that records the error
//    (so the UI can show "fetch failed: ..." and the next manual
//    refresh isn't blocked by a stale `fetch_error`).
//
// The endpoint always returns 200; the success/failure bit is in the
// JSON body, and the operator-facing `last_fetched_at` is updated even
// on a failed fetch (so the dashboard can show "tried 12s ago").

// Inspect a freshly-fetched [`AccountQuota`] and decide whether to
// fire a `quota_low` notification. Returns `(scope, remaining, limit)`
// for the FIRST low window found (session checked before weekly), or
// `None` if both windows are healthy / unknown.
//
// `scope` is `"session"` or `"weekly"` — surfaced in the notification
// details so the dashboard can render "low session quota" vs "low
// weekly quota" distinctly.

// Low-water test. When `limit > 0`, fires iff
// `remaining < limit * 0.10` (i.e. < 10% remaining).
// When `limit == 0` (degenerate row), falls back to the absolute
// floor: `remaining < QUOTA_LOW_ABSOLUTE_FLOOR`.

// =====================================================================
// Provider model refresh
// =====================================================================

/// Default TTL for the `POST /admin/providers/:id/refresh` handler when
/// the caller doesn't pin one in the query string. Matches the value
/// used by the row-level `POST /admin/models/:id/refresh` handler so
/// the two endpoints behave consistently.
const PROVIDER_REFRESH_DEFAULT_TTL_SECS: i64 = 3_600;

/// Query string for `POST /admin/providers/:id/refresh`. Mirrors
/// [`RefreshQuery`] but lives in a separate type because the path
/// parameter is a string id, not a numeric row id, so a single shared
/// type would be misleading.
#[derive(Debug, Default, Deserialize)]
pub struct ProviderRefreshQuery {
    /// Cache TTL in seconds for the discovered rows. Defaults to 1 hour.
    pub ttl_seconds: Option<i64>,
    /// Account id whose API key will be used. Required when the provider
    /// has more than one account; otherwise the first *healthy* account
    /// wins.
    pub account_id: Option<i64>,
}

// `POST /admin/providers/:id/refresh` — re-discover the model list
// for a whole provider in one shot.
//
// The handler is the provider-level counterpart to
// [`refresh_models`] (which is keyed by a single model row). It is the
// path the dashboard's "Refresh models" button uses: the UI knows the
// provider but not a specific model row, so it asks the server to walk
// the full discovery flow end-to-end.
//
// Flow:
// 1. Find the adapter for `provider_id` in the in-process registry.
// 2. If the adapter has no `/models` endpoint, return a 0-row result
//    with a `note` field so the UI can show a friendly message instead
//    of an error (e.g. MiniMax, which has no model-list endpoint).
// 3. Pick a healthy account (or the explicit `?account_id=`) and
//    decrypt its API key.
// 4. Call [`core_admin::refresh_models`], which fetches the upstream's
//    `/models` and upserts the results.
//
// On success: `{"provider": "...", "models_refreshed": N}`.

// Inner body of [`refresh_provider_models`], factored out so the
// handler's future is a small wrapper and the actual work (which holds
// `parking_lot::MutexGuard`s across an `await` point, then drops them)
// lives in a clearly-named function.
//
// ## Locking note
//
// The handler **does not hold any DB lock across the `await`** on the
// upstream HTTP call. We collect everything we need from the DB
// (provider id, adapter clone, decrypted API key) and drop the
// writer guard before calling `adapter.fetch_models(...).await`. The
// final `core_admin::refresh_models` step opens its *own* `Connection` via
// `DbPool::open_connection` (see the doc on `refresh_models` for the
// `Send` rationale), so even that final write doesn't block any other
// request's read.

// =====================================================================
// API keys
// =====================================================================
//
// The admin surface for API keys. Backed by the `api_keys` table
// (migration 000015) and the [`core_api_keys`] service module.
//
// Important: the create / regenerate responses include the
// **plaintext** key. It is shown to the user exactly once and never
// re-derivable from the database. The dashboard's UI is responsible
// for highlighting this fact to the operator ("Save this key now,
// you will not see it again").

// `GET /admin/keys` — list every key, newest first.

// `POST /admin/keys` — create a new key.
//
// Response shape:
//
// ```json
// { "key": <ApiKey metadata>, "plaintext": "op_live_..." }
// ```
//
// The plaintext is the *only* time the user will see the secret.
// The dashboard's "Save this key now" modal is the consumer here.

// `GET /admin/keys/:id` — fetch a single key by id. 404 if absent.

// `PATCH /admin/keys/:id` — partial update.
//
// Body fields are all optional. The `Option<Option<T>>` shape on
// `allowed_models` / `allowed_combos` / `expires_at` lets the caller
// distinguish "leave alone" (key absent) from "clear to NULL" (key
// present with value `null`).

// `POST /admin/keys/:id/revoke` — soft-disable. Idempotent (a
// second call preserves the original `revoked_at` stamp).

// `DELETE /admin/keys/:id` — hard delete. Idempotent (a missing
// id is a silent no-op, matching the `core_accounts::delete` policy).

// `POST /admin/keys/:id/regenerate` — issue a new plaintext and
// re-hash the row. The previous plaintext is invalidated
// immediately. Response shape matches `create_api_key` so the
// dashboard's "Save this key now" modal can be reused.

// `GET /admin/keys/:id/usage` — headline metrics for one key.
// Returns a flat `UsageSummary` (no grouping) plus the standard
// usage roll-up so the dashboard can show a one-screen recap.

// =====================================================================
// OAuth endpoints
// =====================================================================

// `GET /admin/oauth/:provider/authorize` — start a PKCE flow.
//
// Returns `{ "authorization_url": "...", "code_verifier": "...",
// "redirect_uri": "..." }`. The caller opens `authorization_url`
// in a browser, the user authorizes, and the callback delivers an
// authorization code. The `code_verifier` must be saved and sent
// back via the `/exchange` endpoint.
//
// The `redirect_uri` is derived dynamically from the request's
// `Origin` header (or `X-Forwarded-Host` / `Host` fallback) so the
// OAuth flow works from any dashboard URL.

// `POST /admin/oauth/:provider/exchange` — exchange authorization code
// for tokens (PKCE flow).
//
// Body: `{ "code": "...", "code_verifier": "...", "redirect_uri": "http://...",
//          "account_id": 123 }`.
// The `redirect_uri` must match the one used during the authorize step.
// If `account_id` is omitted, a new account is created for this provider.
// Stores the tokens on the account and returns success.

// `POST /admin/oauth/:provider/device-code` — request a device code
// (Device Code flow).
//
// Returns `{ "device_code", "user_code", "verification_uri", ... }`.

// `POST /admin/oauth/:provider/device-poll` — poll for a device code token.
//
// Body: `{ "device_code": "...", "account_id": 123 }`.
// If `account_id` is omitted, a new account is created for this provider.
// Returns `{ "status": "pending" }` or `{ "status": "ok" }` on success.

// `GET /admin/oauth/callback` — OAuth callback handler (MVP).
//
// Displays the authorization code extracted from the callback query
// parameters. In production this would be a redirect page or popup
// closer; for MVP it just shows the code so the user can copy it.

// =====================================================================
// Debug logs — in-memory ring buffer of recent `tracing` events.
// =====================================================================

/// Query parameters for `GET /admin/debug/logs`.
#[derive(Debug, Default, Deserialize)]
pub struct DebugLogsQuery {
    /// If set, only return entries with `seq > since`. Used by the
    /// dashboard's polling loop to fetch only new entries.
    pub since: Option<u64>,
    /// Optional filter by `request_id`. When set, only entries whose
    /// `request_id` field matches are returned.
    pub request_id: Option<String>,
    /// Optional filter by `trace_id`. When set, only entries whose
    /// `trace_id` field matches are returned.
    pub trace_id: Option<String>,
    /// Optional filter by level (case-insensitive). When set, only
    /// entries whose `level` matches are returned. Comma-separated
    /// list supported (e.g. `WARN,ERROR`).
    pub level: Option<String>,
    /// Optional limit on the number of entries returned. Default
    /// 100, max 1000. The ring buffer itself holds 1000 entries, so
    /// `limit=1000` returns everything.
    pub limit: Option<u32>,
}

/// Response envelope for `GET /admin/debug/logs`.
#[derive(Debug, Serialize)]
pub struct DebugLogsResponse {
    /// The entries matching the query, oldest-first.
    pub entries: Vec<crate::debug_log::DebugLogEntry>,
    /// The highest `seq` in the returned set. The frontend passes
    /// this as `since` on the next poll to fetch only new entries.
    pub latest_seq: u64,
    /// Total entries currently in the ring buffer (before filtering).
    pub total_in_buffer: usize,
}

// `GET /admin/debug/logs` — return recent `tracing` events from the
// in-memory ring buffer. Used by the dashboard's Debug Logs view to
// show detailed error context that doesn't fit in the `usage`
// table's `error_msg` column (discovery scheduler skips, OAuth
// refresh failures, race cancellation reasons, etc.).

// `POST /admin/debug/clear` — wipe the in-memory debug log ring
// buffer. Used for "reproduce then capture" workflows: the operator
// clears the buffer, reproduces the bug, then reads the buffer to
// see only the events from the reproduction.

// `POST /admin/debug/vacuum` — manually trigger a SQLite VACUUM + WAL
// checkpoint. Used to repair a fragmented DB that's causing "disk I/O
// error" on analytics queries. The operator can call this endpoint
// when analytics starts failing — it compacts free pages, flushes the
// WAL, and returns the DB to a healthy state.
//
// This is a synchronous, blocking operation (takes the writer lock for
// the duration). For a 300MB DB it takes ~5-15 seconds. Returns 503
// if the writer lock can't be acquired (another write is in progress).

// `POST /admin/api/debug/recover` — attempt to repair a corrupt
// database by dumping all recoverable rows to a SQL script and
// reimporting them into a fresh DB. This is the programmatic
// equivalent of:
//   sqlite3 data.db ".recover" > recovered.sql
//   mv data.db data.db.bak
//   sqlite3 data.db < recovered.sql
//
// **Destructive**: replaces the current DB file with the recovered
// version. The old file is backed up as `data.db.bak.<timestamp>`.
// All in-flight requests will fail during the repair (the writer
// lock is held for the entire duration).
//
// Use this when `POST /admin/api/debug/vacuum` returns "disk I/O
// error" — it means the DB file has page-level corruption that
// VACUUM can't fix.

// =====================================================================
// Notifications tray (F1)
// =====================================================================
//
// The notifications tray surfaces discovery + system events to the
// dashboard. The persistence layer + broadcast channel live in
// `openproxy_core::notifications`; these handlers are thin HTTP
// wrappers around the query / mutation functions there. Real-time
// push is delivered via the WS handler in `stream_usage_rows` (F2
// will subscribe it to the notification broadcast channel); these
// REST endpoints are for the initial load + user-initiated mutations
// (mark read, archive, delete).
//
// All handlers rely on the `admin_auth_middleware` that's layered on
// the `admin_api_routes` router — no per-handler auth check needed.

/// Query string for `GET /admin/api/notifications`.
///
/// - `unread` — if `"true"`, filter to unread rows only.
/// - `limit`  — page size, default 50, clamped to `[1, 200]` by the
///   core layer.
/// - `before_id` — cursor for pagination: only return rows with
///   `id < before_id`.
#[derive(Debug, Default, Deserialize)]
pub struct NotificationsQuery {
    pub unread: Option<bool>,
    pub limit: Option<i64>,
    pub before_id: Option<i64>,
}

// `GET /admin/api/notifications` — list notifications as core_notifications, most recent
// first. Archived rows are always excluded (audit-only).

// `GET /admin/api/notifications/unread-count` — count of unread,
// non-archived rows. Drives the sidebar badge.

// `POST /admin/api/notifications/{id}/read` — mark a single
// notification as read (sets `read_at = now`). Idempotent: re-marking
// a read row is a no-op.

// `POST /admin/api/notifications/read-all` — mark every unread,
// non-archived notification as read. Returns the number of rows
// updated.

// `POST /admin/api/notifications/{id}/archive` — archive a single
// notification (sets `archived_at = now`). The row is preserved for
// audit but hidden from the tray. Idempotent.

// `DELETE /admin/api/notifications/{id}` — permanently delete a
// notification. The DB layer's WHERE clause gates the delete on
// `kind = 'system' OR created_at < datetime('now', '-30 days')` so
// `model_*` rows within their 30-day audit window cannot be deleted.
//
// Returns `{"ok": true}` if a row was deleted, or HTTP 400 with
// an `"notification not deletable..."` message if the row was not
// eligible (either didn't exist, or was a `model_*` row younger than
// 30 days). We use 400 (`CoreError::Validation`) instead of 403 to
// avoid introducing a new `CoreError` variant for this one call site;
// the message text disambiguates "refused" from "not found" for the
// client.

// =====================================================================
// Free Proxy Management Handlers
// =====================================================================

// ...
#[derive(serde::Deserialize)]
pub struct ListProxiesQuery {
    pub source: Option<String>,
    pub status: Option<String>,
    pub protocol: Option<String>,
    pub search: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}
// ...

#[derive(serde::Deserialize)]
pub struct CreateCustomProxyInput {
    pub host: String,
    pub port: u16,
    pub r#type: String,
    pub country_code: Option<String>,
}

// =====================================================================
//
// Spec §9 documents the smoke test as either a manual `curl` against
// a running server OR a Rust test that calls the handler directly.
// The handler is small (auth + DB UPSERT + in-memory slot update), so
// the most informative test is an in-process end-to-end one that
// inserts a `manage`-scope API key in the DB, mounts the handler on
// an `axum::Router`, fires a PUT, and asserts (a) the response shape,
// (b) the DB row was written, and (c) `AppState::timeouts()` reflects
// the new value.
#[cfg(test)]
pub mod tests;
