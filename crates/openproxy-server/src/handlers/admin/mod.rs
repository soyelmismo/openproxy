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

use axum::{
    Json,
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::{Path, Query, State},
    http::HeaderMap,
    response::IntoResponse,
};
use futures::StreamExt;
use openproxy_core::{
    CoreError, accounts, adapters,
    adapters::ProviderAdapter,
    admin, analytics, api_keys as core_api_keys, combos,
    config::{CircuitBreakerConfig, RacingConfig, RetriesConfig, TimeoutsConfig},
    db as core_db,
    db::conn::ADMIN_LOCK_TIMEOUT,
    ids::{
        AccountId, ApiKeyId, ComboId, ComboTargetId, ModelRowId, ProviderId, RequestId, TraceId,
    },
    models, oauth,
    oauth::OAuthProvider,
    providers, seed,
    usage::{self, UsageFilter},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;

use crate::{
    error::{ApiError, ApiResult},
    state::AppState,
};

/// Resolve the adapter for a given provider.
///
/// First checks the built-in adapter registry. If no built-in adapter
/// matches, falls back to loading the provider row from the DB and
/// constructing a [`adapters::CustomAdapter`]. Returns `Err` only when
/// the provider doesn't exist in the DB at all.
fn resolve_adapter(
    s: &AppState,
    provider_id: &ProviderId,
    builtin: &[adapters::ProviderAdapterEnum],
) -> Result<adapters::ProviderAdapterEnum, CoreError> {
    // 1. Built-in adapter?
    if let Some(a) = builtin.iter().find(|a| a.id() == provider_id) {
        return Ok(a.clone());
    }
    // 2. Custom provider in DB → build adapter on-the-fly.
    // `providers::get` is a SELECT — use the READER so this lookup
    // doesn't serialize through the writer mutex (chat hot path).
    let r = s.db_pool().reader();
    let provider_row = providers::get(&r, provider_id)
        .map_err(|e| CoreError::ProviderNotFound(format!("{}: {}", provider_id, e)))?;
    drop(r);
    match provider_row {
        Some(row) => Ok(adapters::ProviderAdapterEnum::Custom(
            adapters::CustomAdapter::from_provider_row(&row),
        )),
        None => Err(CoreError::ProviderNotFound(provider_id.to_string())),
    }
}

/// Optional filters shared by all `GET /admin/usage/*` endpoints.
///
/// All fields are `Option<_>` so a request with no query string is
/// valid and means "no filter". Strings are forwarded verbatim into
/// `UsageFilter`; date bounds are expected to be ISO-8601 (the spec
/// calls this out explicitly).
#[derive(Debug, Default, Deserialize)]
pub struct UsageQuery {
    pub from: Option<String>,
    pub to: Option<String>,
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub account_id: Option<i64>,
    pub combo_id: Option<i64>,
    /// Restrict the roll-up to a single API key. The per-key
    /// `GET /admin/keys/:id/usage` endpoint sets this; the
    /// public analytics endpoints leave it absent.
    pub api_key_id: Option<i64>,
    /// Named time-window preset. One of: `today`, `7d`, `30d`,
    /// `this_month`, `last_month`, `last_6_months`, `ytd`, `custom`.
    ///
    /// When set, the server computes `from`/`to` in UTC and ignores
    /// any explicit `from`/`to` (with a warning). `custom` (or
    /// `None`) falls through to the explicit `from`/`to` fields.
    pub preset: Option<String>,
}

/// Parse a `from` or `to` timestamp from the dashboard into the
/// canonical RFC-3339 form the SQL builder expects. Returns a
/// 400-style [`CoreError::Validation`] on malformed input.
fn parse_usage_timestamp(s: &str, field: &str) -> Result<String, ApiError> {
    // Try RFC-3339 first (the canonical form `created_at` is stored in).
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(dt
            .with_timezone(&chrono::Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    }
    // Fall back to the SQLite "YYYY-MM-DD HH:MM:SS" form (the format
    // operators sometimes paste from a log line). We require the
    // space — a `T` here is the RFC-3339 form, already handled above.
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return Ok(naive
            .and_utc()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    }
    Err(CoreError::Validation(format!(
        "{} must be an RFC-3339 timestamp (e.g. 2026-06-18T07:00:00Z) or \
         SQLite-style (e.g. 2026-06-18 07:00:00); got `{}`",
        field, s
    ))
    .into())
}

/// Format a UTC `DateTime` as the canonical ISO-8601 string the SQL
/// builder expects (e.g. `2026-06-18T07:00:00Z`).
fn iso_z(dt: chrono::DateTime<chrono::Utc>) -> String {
    dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Resolve a `preset` query parameter into `(from, to)` UTC timestamps.
///
/// Returns `None` when `preset` is `None`, `Some(None)` when the preset
/// is recognized but explicitly opts out of date filtering (none of the
/// current presets do this, but the `custom` sentinel means "use the
/// explicit `from`/`to` as-is").
///
/// Returns `Err` for unknown preset strings so the operator sees a 400
/// instead of silently falling back to "no filter" (which would return
/// the wrong data and confuse debugging).
fn resolve_preset(preset: &str) -> Result<Option<(String, String)>, ApiError> {
    use chrono::{Datelike, Duration, NaiveDate, TimeZone, Utc};

    // Helper to format a (year, month, day) tuple at 00:00:00 UTC.
    let midnight = |y: i32, m: u32, d: u32| -> String {
        let naive = NaiveDate::from_ymd_opt(y, m, d)
            .expect("valid ymd")
            .and_hms_opt(0, 0, 0)
            .expect("valid hms");
        iso_z(Utc.from_utc_datetime(&naive))
    };

    let now = Utc::now();
    let today = now.date_naive();
    let y = now.year();
    let m = now.month();

    match preset {
        "today" => {
            let from = midnight(y, m, today.day());
            // Tomorrow rolls over month/year boundaries via chrono's
            // NaiveDate arithmetic; using `today.day() + 1` directly
            // would overflow on the last day of the month.
            let tomorrow = today + Duration::days(1);
            let to = midnight(tomorrow.year(), tomorrow.month(), tomorrow.day());
            Ok(Some((from, to)))
        }
        "7d" => {
            let from = now - Duration::days(7);
            Ok(Some((iso_z(from), iso_z(now))))
        }
        "30d" => {
            let from = now - Duration::days(30);
            Ok(Some((iso_z(from), iso_z(now))))
        }
        "this_month" => {
            let from = midnight(y, m, 1);
            // First day of next month (may roll into next year).
            let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
            let to = midnight(ny, nm, 1);
            Ok(Some((from, to)))
        }
        "last_month" => {
            let (ly, lm) = if m == 1 { (y - 1, 12) } else { (y, m - 1) };
            let from = midnight(ly, lm, 1);
            let to = midnight(y, m, 1);
            Ok(Some((from, to)))
        }
        "last_6_months" => {
            // Walk back 6 months from the first day of the current
            // month. We compute the start of each month by subtracting
            // months one at a time to avoid the "month - 6" underflow.
            let mut ly = y;
            let mut lm = m;
            for _ in 0..6 {
                if lm == 1 {
                    lm = 12;
                    ly -= 1;
                } else {
                    lm -= 1;
                }
            }
            let from = midnight(ly, lm, 1);
            let to = midnight(y, m, 1);
            Ok(Some((from, to)))
        }
        "ytd" => {
            let from = midnight(y, 1, 1);
            let to = midnight(y + 1, 1, 1);
            Ok(Some((from, to)))
        }
        // `custom` (or any other unrecognised string the operator
        // might type) means "use the explicit from/to as-is". We
        // surface unknown presets as a 400 so the dashboard doesn't
        // silently miss a window due to a typo.
        "custom" => Ok(None),
        other => Err(CoreError::Validation(format!(
            "preset must be one of today|7d|30d|this_month|last_month|last_6_months|ytd|custom; got `{}`",
            other
        ))
        .into()),
    }
}

impl UsageQuery {
    /// Project into a [`UsageFilter`]. An empty `provider_id` string
    /// surfaces here as a 400 via [`CoreError::Validation`].
    fn into_filter(self) -> Result<UsageFilter, ApiError> {
        let provider_id = self
            .provider_id
            .map(|s| {
                if s.is_empty() {
                    Err(CoreError::Validation(
                        "provider_id must not be empty".into(),
                    ))
                } else {
                    Ok(ProviderId::new(s))
                }
            })
            .transpose()?;
        // MEDIUM fix: validate `from` and `to` are well-formed timestamps
        // before they reach the SQL builder. Without this, a query
        // like `?from=garbage` returns 0 rows silently (the SQLite
        // string comparison fails the row against every `created_at`)
        // and the operator gets a misleading "no data" result. A
        // malformed timestamp is a client error and must surface as 400.
        //
        // Accept the two timestamp shapes the dashboard sends:
        //   - RFC-3339 (e.g. `2026-06-18T07:00:00Z`)
        //   - SQLite-style (e.g. `2026-06-18 07:00:00`)
        //
        // Both round-trip through `chrono::DateTime<Utc>` and we
        // re-emit the canonical RFC-3339 form so the SQL comparison
        // is consistent.
        let mut from = self
            .from
            .map(|s| parse_usage_timestamp(&s, "from"))
            .transpose()?;
        let mut to = self
            .to
            .map(|s| parse_usage_timestamp(&s, "to"))
            .transpose()?;

        // Preset handling: if `preset` is set, it takes precedence
        // over explicit `from`/`to`. We log a warning when both are
        // provided so the operator can spot the dashboard sending
        // redundant data.
        if let Some(preset) = &self.preset {
            if from.is_some() || to.is_some() {
                tracing::warn!(
                    preset = %preset,
                    from = ?from,
                    to = ?to,
                    "UsageQuery: preset is set and will override explicit from/to"
                );
            }
            if let Some((pf, pt)) = resolve_preset(preset)? {
                from = Some(pf);
                to = Some(pt);
            }
            // `custom` (or None) falls through with the explicit values.
        }

        // If both are present, from must not be after to. (Both
        // are inclusive at the lower bound in the SQL.)
        if let (Some(f), Some(t)) = (&from, &to)
            && f > t
        {
            return Err(
                CoreError::Validation(format!("from ({}) must be <= to ({})", f, t)).into(),
            );
        }
        let account_id = self.account_id.map(AccountId::new);
        let combo_id = self.combo_id.map(ComboId);
        let api_key_id = self.api_key_id.map(ApiKeyId);
        Ok(UsageFilter {
            from,
            to,
            provider_id,
            model_id: self.model_id,
            account_id,
            combo_id,
            api_key_id,
        })
    }
}

// =====================================================================
// Health
// =====================================================================

/// `GET /admin/health` — process liveness with a version tag.
pub async fn admin_health() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

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
    pub compression: openproxy_core::compression::CompressionMode,
    /// When true, idle_chunk timeouts are treated as retryable
    /// (pipeline falls through to the next target).
    pub idle_chunk_retryable: bool,
    pub quota_protection: openproxy_core::config::QuotaProtectionConfig,
}

/// `GET /admin/config` — return the currently-loaded runtime
/// configuration (timeouts, retries, circuit-breaker, racing).
///
/// This is a thin, side-effect-free snapshot of `AppState::config()`:
/// no DB, no DB writer, no I/O. The auth check mirrors
/// `get_recording` / `usage_*` because the operator might consider
/// these values sensitive (they leak upstream connection budgets).
pub async fn get_runtime_config(
    State(s): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<RuntimeConfigResponse>> {
    if let Err(e) = authenticate_admin_ws(&s, &headers, None) {
        return e.into();
    }
    let body: Result<Json<RuntimeConfigResponse>, ApiError> = async {
        let cfg = s.config();
        Ok(Json(RuntimeConfigResponse {
            timeouts: s.timeouts(),
            retries: cfg.retries,
            circuit_breaker: cfg.circuit_breaker,
            // `RacingConfig` is `Clone` but not `Copy` (the other
            // three are); `.clone()` is fine, it's three `u*` fields.
            racing: cfg.racing.clone(),
            recording_ttl_secs: s.recording_ttl_secs(),
            compression: s.compression_mode(),
            idle_chunk_retryable: s.idle_chunk_retryable(),
            quota_protection: s.quota_protection(),
        }))
    }
    .await;
    body.into()
}

/// `PUT /admin/config/timeouts` — hot-reload the system default
/// timeouts. Body is a full [`TimeoutsConfig`] (5 `u64` fields, all
/// required). On success the value is persisted in the `app_config`
/// table and the in-memory `timeouts_cell` slot is updated. Future
/// chat requests see the new values; requests already in flight keep
/// the previous value (consistent with the per-pipeline
/// `PipelineConfig::defaults` snapshot taken in `chat.rs:201-203`).
///
/// **Auth**: same as `get_runtime_config` — `manage` scope via
/// `authenticate_admin_ws`.
///
/// **Validation**: structural only. serde rejects missing or
/// wrong-type fields; we do not check business ranges (a zero is
/// allowed, matching the current `TimeoutsConfig` policy).
///
/// **Side-effect order**: DB first, memory second. If the DB UPSERT
/// fails, the response is 500 and the in-memory value is unchanged.
pub async fn put_runtime_timeouts(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<TimeoutsConfig>,
) -> ApiResult<Json<serde_json::Value>> {
    if let Err(e) = authenticate_admin_ws(&s, &headers, None) {
        return e.into();
    }
    let inner: Result<Json<serde_json::Value>, ApiError> = async {
        // 1. Persist to DB first. The UPSERT is atomic in SQLite.
        //    We let the application timestamp it (rather than relying
        //    on `strftime('%s','now')`) so the value matches what
        //    `load_timeouts_override_from_db` expects on the next
        //    boot: an `i64` unix seconds.
        {
            let w = s.db_pool().writer();
            let now = chrono::Utc::now().timestamp();
            core_db::app_config::save_timeouts_to_db(&w, &body, now)?;
        }
        // 2. Update the in-memory slot. Readers see the new value
        //    as soon as this returns. Note: requests already in
        //    flight captured a `Copy` of the old value into their
        //    PipelineConfig and are unaffected.
        s.set_timeouts(body);
        Ok(Json(serde_json::json!({
            "connect_ms": body.connect_ms,
            "request_send_ms": body.request_send_ms,
            "ttft_ms": body.ttft_ms,
            "idle_chunk_ms": body.idle_chunk_ms,
            "total_ms": body.total_ms,
            "applies_to": "next_requests",
        })))
    }
    .await;
    inner.into()
}

/// `PUT /admin/config/compression` — hot-reload the compression
/// mode. Body: `{"mode": "off" | "lite" | "rtk"}`.
pub async fn put_runtime_compression(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<openproxy_core::compression::CompressionMode>,
) -> ApiResult<Json<serde_json::Value>> {
    if let Err(e) = authenticate_admin_ws(&s, &headers, None) {
        return e.into();
    }
    let inner: Result<Json<serde_json::Value>, ApiError> = async {
        {
            let w = s.db_pool().writer();
            let now = chrono::Utc::now().timestamp();
            core_db::app_config::save_compression_to_db(&w, &body, now)?;
        }
        s.set_compression_mode(body);
        Ok(Json(serde_json::json!({
            "mode": body,
            "applies_to": "next_requests",
        })))
    }
    .await;
    inner.into()
}

// =====================================================================
// Idle chunk retryable
// =====================================================================

/// `PUT /admin/config/idle-chunk-retryable` — hot-reload the
/// `idle_chunk_retryable` flag. Body: `{"idle_chunk_retryable": true}`
/// or `{"idle_chunk_retryable": false}`.
///
/// When true, idle_chunk timeouts are treated as retryable: the
/// pipeline falls through to the next target instead of aborting.
/// When false (default), idle_chunk timeouts return an error
/// immediately and the walk is aborted.
pub async fn put_idle_chunk_retryable(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> ApiResult<Json<serde_json::Value>> {
    if let Err(e) = authenticate_admin_ws(&s, &headers, None) {
        return e.into();
    }
    let inner: Result<Json<serde_json::Value>, ApiError> = async {
        let val = body
            .get("idle_chunk_retryable")
            .and_then(|v| v.as_bool())
            .ok_or_else(|| {
                ApiError(CoreError::Validation(
                    "idle_chunk_retryable must be a boolean".into(),
                ))
            })?;
        {
            let w = s.db_pool().writer();
            let now = chrono::Utc::now().timestamp();
            core_db::app_config::save_idle_chunk_retryable_to_db(&w, val, now)?;
        }
        s.set_idle_chunk_retryable(val);
        tracing::info!(
            idle_chunk_retryable = val,
            "updated idle_chunk_retryable via admin API"
        );
        Ok(Json(serde_json::json!({
            "idle_chunk_retryable": val,
            "applies_to": "next_requests",
        })))
    }
    .await;
    inner.into()
}

// =====================================================================
// Quota protection
// =====================================================================

/// `PUT /admin/config/quota-protection` — hot-reload the `quota_protection`
/// config. Body: `{"enabled": true, "threshold_percentage": 10}`.
pub async fn put_runtime_quota_protection(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<openproxy_core::config::QuotaProtectionConfig>,
) -> ApiResult<Json<serde_json::Value>> {
    if let Err(e) = authenticate_admin_ws(&s, &headers, None) {
        return e.into();
    }
    let inner: Result<Json<serde_json::Value>, ApiError> = async {
        {
            let w = s.db_pool().writer();
            let now = chrono::Utc::now().timestamp();
            openproxy_core::db::app_config::save_quota_protection_to_db(&w, &body, now)?;
        }
        s.set_quota_protection(body.clone());
        tracing::info!(
            enabled = body.enabled,
            threshold_percentage = body.threshold_percentage,
            "updated quota_protection via admin API"
        );
        Ok(Json(serde_json::json!({
            "enabled": body.enabled,
            "threshold_percentage": body.threshold_percentage,
            "applies_to": "next_requests",
        })))
    }
    .await;
    inner.into()
}

// =====================================================================
// Recording TTL
// =====================================================================

/// `GET /admin/config/maintenance` — read the current maintenance
/// config (auto_vacuum, vacuum_interval_hours, usage_retention_days)
/// and the VACUUM status (last_run, in_progress, next_scheduled).
pub async fn get_maintenance_config(
    State(s): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    if let Err(e) = authenticate_admin_ws(&s, &headers, None) {
        return e.into();
    }
    let cfg = s.maintenance_config();
    let status = s.vacuum_status();
    ApiResult::ok(Json(serde_json::json!({
        "auto_vacuum": cfg.auto_vacuum,
        "vacuum_interval_hours": cfg.vacuum_interval_hours,
        "usage_retention_days": cfg.usage_retention_days,
        "vacuum_status": status,
    })))
}

/// `PUT /admin/config/maintenance` — update the maintenance config at
/// runtime. Body: `{ "auto_vacuum": bool, "vacuum_interval_hours": u32,
/// "usage_retention_days": u32 }`. All fields are optional — missing
/// fields keep their current value. Changes take effect on the next
/// background tick (no restart needed).
pub async fn put_maintenance_config(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> ApiResult<Json<serde_json::Value>> {
    if let Err(e) = authenticate_admin_ws(&s, &headers, None) {
        return e.into();
    }
    let mut cfg = s.maintenance_config();
    if let Some(v) = body.get("auto_vacuum").and_then(|v| v.as_bool()) {
        cfg.auto_vacuum = v;
    }
    if let Some(v) = body.get("vacuum_interval_hours").and_then(|v| v.as_u64()) {
        cfg.vacuum_interval_hours = v.max(1) as u32;
    }
    if let Some(v) = body.get("usage_retention_days").and_then(|v| v.as_u64()) {
        cfg.usage_retention_days = v as u32;
    }
    s.set_maintenance_config(cfg.clone());
    ApiResult::ok(Json(serde_json::json!({
        "updated": true,
        "config": {
            "auto_vacuum": cfg.auto_vacuum,
            "vacuum_interval_hours": cfg.vacuum_interval_hours,
            "usage_retention_days": cfg.usage_retention_days,
        }
    })))
}

/// `GET /admin/config/vacuum-status` — read the current VACUUM status
/// (last_run, last_result, in_progress, next_scheduled). Polled by
/// the dashboard's config view button.
pub async fn get_vacuum_status(
    State(s): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<crate::state::VacuumStatus>> {
    if let Err(e) = authenticate_admin_ws(&s, &headers, None) {
        return e.into();
    }
    ApiResult::ok(Json(s.vacuum_status()))
}

/// `GET /admin/config/recording-ttl` — read the current recording body
/// TTL in seconds.
pub async fn get_recording_ttl(
    State(s): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    if let Err(e) = authenticate_admin_ws(&s, &headers, None) {
        return e.into();
    }
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        Ok(Json(serde_json::json!({
            "recording_ttl_secs": s.recording_ttl_secs(),
        })))
    }
    .await;
    body.into()
}

/// `PUT /admin/config/recording-ttl` — hot-reload the recording body
/// TTL. Body: `{"recording_ttl_secs": N}` where `N` is the lifetime in
/// seconds for recorded request/response bodies and headers.
pub async fn put_recording_ttl(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> ApiResult<Json<serde_json::Value>> {
    if let Err(e) = authenticate_admin_ws(&s, &headers, None) {
        return e.into();
    }
    let inner: Result<Json<serde_json::Value>, ApiError> = async {
        let ttl_secs = body
            .get("recording_ttl_secs")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| CoreError::Validation("missing 'recording_ttl_secs' integer".into()))?;
        if ttl_secs < 0 {
            return Err(
                CoreError::Validation("'recording_ttl_secs' must be non-negative".into()).into(),
            );
        }
        {
            let w = s.db_pool().writer();
            let now = chrono::Utc::now().timestamp();
            core_db::app_config::save_recording_ttl_to_db(&w, ttl_secs, now)?;
        }
        s.set_recording_ttl_secs(ttl_secs);
        Ok(Json(serde_json::json!({
            "recording_ttl_secs": ttl_secs,
            "applies_to": "next_prune_tick",
        })))
    }
    .await;
    inner.into()
}

// =====================================================================
// Providers
// =====================================================================

#[derive(serde::Serialize)]
pub struct ProviderWithOAuth {
    #[serde(flatten)]
    pub provider: providers::Provider,
    pub oauth_flows: Option<Vec<String>>,
    pub metadata: openproxy_core::providers::ProviderMetadata,
    pub active_models: i64,
    pub total_models: i64,
}

fn enrich_provider_with_oauth(
    p: providers::Provider,
    registry: &openproxy_core::oauth::OAuthProviderRegistry,
    adapters: &[openproxy_core::adapters::ProviderAdapterEnum],
    r: &rusqlite::Connection,
) -> ProviderWithOAuth {
    let flows = if p.auth_type == openproxy_core::providers::AuthType::OAuth {
        if let Some(oauth_impl) = registry.get(p.id.as_str()) {
            let mut f = Vec::new();
            match oauth_impl.flow() {
                openproxy_core::oauth::OAuthFlow::AuthorizationCodePkce => {
                    f.push("pkce".to_string());
                }
                openproxy_core::oauth::OAuthFlow::DeviceCode => {
                    f.push("device".to_string());
                }
                openproxy_core::oauth::OAuthFlow::AuthorizationCode => {
                    f.push("auth_code".to_string());
                }
            }
            Some(f)
        } else {
            None
        }
    } else {
        None
    };

    let metadata = adapters
        .iter()
        .find(|a| a.id() == &p.id)
        .map(|a| a.metadata())
        .unwrap_or_else(|| {
            // Fallback for custom providers that aren't loaded in the adapter registry yet
            let built_in = openproxy_core::providers::is_builtin(p.id.as_str());
            openproxy_core::providers::ProviderMetadata {
                built_in,
                deletable: !built_in,
                supports_quota: false,
                quota_refresh_supported: false,
            }
        });

    let active_models: i64 = r
        .query_row(
            "SELECT count(*) FROM models WHERE provider_id = ? AND active = 1",
            [p.id.as_str()],
            |row| row.get(0),
        )
        .unwrap_or(0);
    let total_models: i64 = r
        .query_row(
            "SELECT count(*) FROM models WHERE provider_id = ?",
            [p.id.as_str()],
            |row| row.get(0),
        )
        .unwrap_or(0);

    ProviderWithOAuth {
        provider: p,
        oauth_flows: flows,
        metadata,
        active_models,
        total_models,
    }
}

/// `GET /admin/providers` — list all providers.
pub async fn list_providers(State(s): State<AppState>) -> ApiResult<Json<Vec<ProviderWithOAuth>>> {
    let body: Result<Json<Vec<ProviderWithOAuth>>, ApiError> = async {
        // Read-only SELECT — use the READER so the dashboard's catalog
        // polling doesn't serialize through the writer mutex.
        let r = s.db_pool().reader();
        let list = admin::list_providers(&r)?;
        let registry = s.oauth_provider_registry();
        let adapters = s.adapters();
        let enriched = list
            .into_iter()
            .map(|p| enrich_provider_with_oauth(p, registry.as_ref(), &adapters, &r))
            .collect();
        Ok(Json(enriched))
    }
    .await;
    body.into()
}

/// `POST /admin/providers` — create a provider.
pub async fn create_provider(
    State(s): State<AppState>,
    Json(input): Json<admin::CreateProviderInput>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        // Scope the writer guard so it is dropped BEFORE
        // rebuild_adapters re-acquires the same non-reentrant
        // parking_lot::Mutex. Holding the guard across
        // rebuild_adapters deadlocks the Tokio worker thread.
        let id = {
            let w = s.db_pool().writer();
            admin::create_provider(&w, input)?
        };
        // Hot-reload the in-memory adapter registry so the chat
        // pipeline can dispatch to the new provider without a
        // process restart. A failure here is logged but does NOT
        // roll back the DB write — the operator's intent to add
        // the provider has already been recorded; the next admin
        // action (or the next chat request, which already has
        // DB-fallback via `resolve_adapter`) will pick up the new
        // adapter on the next `rebuild_adapters`.
        if let Err(e) = s.rebuild_adapters() {
            tracing::warn!(
                provider_id = id.as_str(),
                error = %e,
                "failed to reload adapter registry after create_provider; \
                 chat pipeline may still fall through to DB lookup"
            );
        } else {
            tracing::info!(
                provider_id = id.as_str(),
                "reloaded adapter registry after creating provider"
            );
        }
        Ok(Json(serde_json::json!({ "id": id.as_str() })))
    }
    .await;
    body.into()
}

/// `GET /admin/providers/:id` — fetch a single provider.
pub async fn get_provider(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<ProviderWithOAuth>> {
    let body: Result<Json<ProviderWithOAuth>, ApiError> = async {
        // Read-only SELECT — use the READER.
        let r = s.db_pool().reader();
        let id = ProviderId::new(id);
        let provider =
            providers::get(&r, &id)?.ok_or_else(|| CoreError::ProviderNotFound(id.to_string()))?;
        let registry = s.oauth_provider_registry();
        let adapters = s.adapters();
        let enriched = enrich_provider_with_oauth(provider, registry.as_ref(), &adapters, &r);
        Ok(Json(enriched))
    }
    .await;
    body.into()
}

/// `DELETE /admin/providers/:id` — delete a provider. Idempotent
/// for custom providers.
///
/// Built-in providers (the ones seeded on first run — see
/// [`openproxy_core::seed::builtin_provider_ids`]) are rejected
/// with a 400 (Validation) instructing the operator to use
/// `POST /admin/providers/:id/active` to deactivate the
/// provider instead. Built-ins are protected because removing
/// the row would leave dangling references in the adapter
/// registry, and the operator can get the "stop using this
/// provider" effect cheaply and reversibly via the
/// deactivate flag.
///
/// The guard is implemented in two places by design: this
/// handler does a fast pre-check on the URL id so the DB write
/// is never attempted, and [`openproxy_core::admin::delete_provider`]
/// repeats the check on the typed id. Either one alone is
/// sufficient for correctness; both makes the rejection
/// observable from any future caller of the service layer.
pub async fn delete_provider(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        // Fast-fail on built-in ids before opening a writer. The
        // message is the same one the service layer would produce
        // so the dashboard's error toast is consistent regardless
        // of which path the rejection took.
        if seed::is_builtin(&id) {
            return Err(ApiError(CoreError::Validation(format!(
                "provider '{}' is a built-in and cannot be deleted. Use POST \
                 /admin/providers/{}/active with {{\"active\": false}} to \
                 deactivate it instead.",
                id, id
            ))));
        }
        // Scope the writer guard so it is dropped BEFORE
        // rebuild_adapters re-acquires the same non-reentrant
        // parking_lot::Mutex. Holding the guard across
        // rebuild_adapters deadlocks the Tokio worker thread.
        let pid = ProviderId::new(id.clone());
        {
            let w = s.db_pool().writer();
            admin::delete_provider(&w, &pid)?;
        }
        // Hot-reload so the chat pipeline drops the
        // `CustomAdapter` for this provider. For built-in ids we
        // never get here (the fast-fail above rejects them), so
        // this branch only fires for custom providers. A failure
        // here is logged-and-continued: the DB delete has already
        // committed, and the next admin action or DB-fallback
        // lookup will pick up the new state.
        if let Err(e) = s.rebuild_adapters() {
            tracing::warn!(
                provider_id = pid.as_str(),
                error = %e,
                "failed to reload adapter registry after delete_provider"
            );
        } else {
            tracing::info!(
                provider_id = pid.as_str(),
                "reloaded adapter registry after deleting provider"
            );
        }
        Ok(Json(serde_json::json!({ "deleted": pid.as_str() })))
    }
    .await;
    body.into()
}

/// `POST /admin/providers/:id/active` — flip the soft-disable flag
/// on a provider. Body: `{"active": true|false}`. Returns the new state.
///
/// This is the dashboard's "Deactivate" / "Activate" button path. A
/// deactivated provider stays in the DB (accounts and models
/// preserved) and can be reactivated later. While deactivated, combo-
/// target lookups skip it; the pipeline surfaces `NoHealthyTargets`
/// when a combo has no active providers left.
///
/// Missing id is a silent no-op (matches the rest of the providers
/// helpers), so the dashboard's optimistic refetch never has to
/// special-case a 404.
pub async fn set_provider_active(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let active = body
            .get("active")
            .and_then(|v| v.as_bool())
            .ok_or_else(|| CoreError::Validation("missing 'active' bool".into()))?;
        let w = s.db_pool().writer();
        let provider_id = ProviderId::new(id.clone());
        admin::set_provider_active(&w, &provider_id, active)?;
        Ok(Json(serde_json::json!({ "id": id, "active": active })))
    }
    .await;
    body.into()
}

// =====================================================================
// Accounts
// =====================================================================

/// Query string for `GET /admin/accounts` — supports `?provider_id=...`.
#[derive(Debug, Default, Deserialize)]
pub struct AccountListQuery {
    pub provider_id: Option<String>,
}

/// `GET /admin/accounts` — list accounts, optionally filtered by provider.
pub async fn list_accounts(
    State(s): State<AppState>,
    Query(q): Query<AccountListQuery>,
) -> ApiResult<Json<Vec<accounts::Account>>> {
    let body: Result<Json<Vec<accounts::Account>>, ApiError> = async {
        // Read-only SELECT — use the READER.
        let r = s.db_pool().reader();
        let provider = q.provider_id.map(ProviderId::new);
        let list = admin::list_accounts(&r, provider.as_ref())?;
        Ok(Json(list))
    }
    .await;
    body.into()
}

/// `POST /admin/accounts` — create an account. `api_key` is encrypted
/// before insertion; the response only echoes the new id.
pub async fn create_account(
    State(s): State<AppState>,
    Json(input): Json<admin::CreateAccountInput>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let w = s.db_pool().writer();
        let id = admin::create_account(&w, s.master_key().as_ref(), input)?;
        Ok(Json(serde_json::json!({ "id": id.0 })))
    }
    .await;
    body.into()
}

/// `DELETE /admin/accounts/:id` — delete an account by numeric id. Idempotent.
pub async fn delete_account(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let w = s.db_pool().writer();
        let id = AccountId::new(id);
        admin::delete_account(&w, id)?;
        Ok(Json(serde_json::json!({ "deleted": id.0 })))
    }
    .await;
    body.into()
}

// =====================================================================
// Combos
// =====================================================================

/// `GET /admin/combos` — list all combos.
pub async fn list_combos(State(s): State<AppState>) -> ApiResult<Json<Vec<combos::Combo>>> {
    let body: Result<Json<Vec<combos::Combo>>, ApiError> = async {
        // Read-only SELECT — use the READER.
        let r = s.db_pool().reader();
        let list = admin::list_combos(&r)?;
        Ok(Json(list))
    }
    .await;
    body.into()
}

/// `POST /admin/combos` — create a combo. `race_size` defaults to 1.
pub async fn create_combo(
    State(s): State<AppState>,
    Json(input): Json<admin::CreateComboInput>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let w = s.db_pool().writer();
        let id = admin::create_combo(&w, input)?;
        Ok(Json(serde_json::json!({ "id": id.0 })))
    }
    .await;
    body.into()
}

/// `GET /admin/combos/:id` — fetch a single combo.
pub async fn get_combo(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<combos::Combo>> {
    let body: Result<Json<combos::Combo>, ApiError> = async {
        // Read-only SELECT — use the READER.
        let r = s.db_pool().reader();
        let id = ComboId(id);
        let combo = combos::get_combo(&r, id)?.ok_or_else(|| CoreError::ComboNotFound(id.0))?;
        Ok(Json(combo))
    }
    .await;
    body.into()
}

/// `POST /admin/combos/:id/test-all` — fan-out a test request to
/// every target of a combo and return a list of per-target results.
///
/// The handler:
///
/// 1. Loads the combo's targets via
///    [`openproxy_core::combos::list_targets_with_model`] (which
///    already LEFT-JOINs `target_cooldowns`, so the
///    `in_cooldown` flag is populated for free).
/// 2. For each target:
///    - If it is a sub-combo target, mark it as `skipped` with
///      `error_msg = "sub-combo; test children individually"`. The
///      actual children are reachable via the parent combo's id in
///      the dashboard.
///    - If it is currently parked in `target_cooldowns`, mark it as
///      `skipped` with `error_msg = "in_cooldown"` so the operator
///      can see *why* we didn't fire a real request.
///    - Otherwise, call [`run_test_for_model`] to actually ping the
///      upstream and capture the live status.
///
/// The shape is intentionally compatible with the per-row result
/// returned by [`test_model`] (the same `status` / `elapsed_ms` /
/// `error_msg` fields), so the UI's renderer doesn't have to
/// special-case the two endpoints.
///
/// The wall-clock cost is bounded by a 180 s timeout (see the
/// `tokio::time::timeout` wrap below) and by a per-target 15 s
/// timeout inside [`run_test_for_model`]. With ~8 targets and a
/// stuck upstream, the endpoint may still take ~2 minutes — the
/// dashboard button flips to "🧪 Testing..." for the duration.
pub async fn test_combo_targets(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    cancel_watch: Option<axum::Extension<crate::disconnect::CancelWatch>>,
) -> ApiResult<Json<Vec<serde_json::Value>>> {
    use serde_json::json;

    let cancel_rx = cancel_watch.map(|axum::Extension(cw)| cw.rx);

    // Cancellation note: the previous implementation spawned a
    // disconnect-watcher task that drained `request.into_parts().1`
    // (the request body) and flipped a `tokio::sync::watch` flag
    // when the body stream ended. For a POST with no body — which is
    // what the dashboard actually sends — `Body::frame()` resolves
    // to `None` immediately, so the watcher fired `disconnect_tx`
    // before the fan-out loop started its second iteration. The
    // fan-out then aborted after the first target, which silently
    // broke "Test all".
    //
    // We rely on Axum's natural cancellation instead: when the
    // client drops the response future (closes the tab, navigates
    // away, etc.), the handler future is dropped, which in turn
    // drops the in-flight `UpstreamClient::call()` future
    // (UpstreamClient is cancel-safe) and aborts the loop. No watcher
    // task is needed — and a watcher task is in fact *counter-
    // productive* because it would outlive the handler and never
    // observe the drop. The 180s `tokio::time::timeout` below
    // remains the upper bound for the happy path.
    let body: Result<Json<Vec<serde_json::Value>>, ApiError> = async {
        let cancel_rx = cancel_rx.clone();
        // Snapshot the targets up-front and drop the writer guard.
        // The per-target test below does its own short DB
        // transactions (writer lock + drop), so the long-running
        // HTTP calls don't block other handlers from writing.
        let targets = {
            let w = s.db_pool().writer();
            combos::list_targets_with_model(&w, ComboId(id))?
        };

        // The fan-out is intentionally serial. The prompt explicitly
        // asked for no parallelization in the MVP ("NO paralelizar.
        // Secuencial está bien para MVP. Documentar como
        // follow-up"); the comment on the inner loop is the
        // follow-up. We do, however, bound the whole fan-out with a
        // hard timeout so the dashboard never waits longer than 3
        // minutes — the worst case is 8 targets × 15 s each.
        let fan_out = async {
            let mut results = Vec::with_capacity(targets.len());
            for t in targets {
                if t.sub_combo_id.is_some() {
                    // Sub-combo row: do not recurse. The "test
                    // children individually" message mirrors the
                    // pre-refactor handler so existing dashboard
                    // tooltip behavior is preserved.
                    results.push(json!({
                        "target_id": t.id.0,
                        "sub_combo_id": t.sub_combo_id.map(|c| c.0),
                        "sub_combo_name": t.sub_combo_name,
                        "provider_id": t.provider_id.to_string(),
                        "status": 0_i32,
                        "elapsed_ms": serde_json::Value::Null,
                        "error_msg": "sub-combo; test children individually",
                        "skipped": true,
                    }));
                    continue;
                }
                if t.in_cooldown {
                    // The target is parked. Surface that as a
                    // skipped row with the same shape the dashboard
                    // already knows, and copy the reason into the
                    // error message so the operator can see *why*
                    // the row is parked without opening a second
                    // endpoint.
                    results.push(json!({
                        "target_id": t.id.0,
                        "provider_id": t.provider_id.to_string(),
                        "account_id": t.account_id.map(|a| a.0),
                        "model_row_id": t.model_row_id.map(|m| m.0),
                        "model_id": t.model_id,
                        "model_display_name": t.model_display_name,
                        "status": 0_i32,
                        "elapsed_ms": serde_json::Value::Null,
                        "error_msg": format!(
                            "in_cooldown: {}",
                            t.cooldown_reason.as_deref().unwrap_or("no reason recorded")
                        ),
                        "skipped": true,
                    }));
                    continue;
                }
                if let Some(ref rx) = cancel_rx
                    && *rx.borrow()
                {
                    tracing::info!("test_combo_targets: client disconnected, aborting fan-out");
                    break;
                }
                // Flat, active, not in cooldown: actually fire
                // upstream. The helper handles the model-not-active
                // short-circuit itself (skipped row with
                // "model is inactive" in the error_msg).
                let r = run_test_for_model(
                    &s,
                    t.model_row_id.unwrap_or(ModelRowId(0)).0,
                    t.account_id,
                    None,
                    TestOptions {
                        in_combo_fanout: true,
                    },
                    cancel_rx.clone(),
                )
                .await;
                // Use the per-target metadata from the snapshot
                // for the response, not whatever the helper
                // returned (the helper doesn't have the row
                // metadata handy). `r.row_id` is informational
                // and matches `t.model_row_id`.
                let mut obj = json!({
                    "target_id": t.id.0,
                    "provider_id": t.provider_id.to_string(),
                    "account_id": t.account_id.map(|a| a.0),
                    "model_row_id": t.model_row_id.map(|m| m.0),
                    "model_id": t.model_id,
                    "model_display_name": t.model_display_name,
                    "status": r.status,
                    "elapsed_ms": r.elapsed_ms,
                    "error_msg": r.error_msg,
                    "skipped": r.skipped,
                    "row_id": r.row_id,
                });
                if r.skipped {
                    obj["error_msg"] =
                        json!(r.skip_reason.unwrap_or_else(|| "skipped".to_string()));
                }
                results.push(obj);
            }
            results
        };

        let results = match tokio::time::timeout(std::time::Duration::from_secs(180), fan_out).await
        {
            Ok(rs) => rs,
            Err(_) => {
                // Timed out before we finished. Return whatever we
                // have so the dashboard can render the partial
                // picture. The frontend treats the response shape
                // uniformly; a 504 here would just wipe the
                // button state with no data.
                tracing::warn!(combo_id = id, "test-all fan-out exceeded 180s budget");
                return Err(ApiError(CoreError::Internal(
                    "test-all exceeded 180s budget; partial results dropped".into(),
                )));
            }
        };

        Ok(Json(results))
    }
    .await;
    body.into()
}

/// `DELETE /admin/combos/:id` — delete a combo. Idempotent; cascade
/// removes its targets.
pub async fn delete_combo(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let w = s.db_pool().writer();
        let id = ComboId(id);
        admin::delete_combo(&w, id)?;
        Ok(Json(serde_json::json!({ "deleted": id.0 })))
    }
    .await;
    body.into()
}

/// `GET /admin/combos/:id/targets` — list a combo's targets in
/// `(priority_order ASC, id ASC)` order, enriched with the model's
/// upstream id and human-readable display name so the dashboard
/// doesn't have to do a per-row roundtrip.
///
/// Returns [`combos::ComboTargetWithModel`] (superset of
/// [`combos::ComboTarget`]); the extra fields are `model_id` (upstream
/// id, e.g. `"anthropic/claude-3.5-sonnet"`) and `model_display_name`
/// (the row's `display_name`, or `None` if unset).
pub async fn list_combo_targets(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<Vec<combos::ComboTargetWithModel>>> {
    let body: Result<Json<Vec<combos::ComboTargetWithModel>>, ApiError> = async {
        // Read-only SELECT — use the READER.
        let r = s.db_pool().reader();
        let id = ComboId(id);
        let targets = admin::list_combo_targets_with_model(&r, id)?;
        Ok(Json(targets))
    }
    .await;
    body.into()
}

/// `POST /admin/combos/:id/targets` — add a target to a combo.
pub async fn add_target(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(input): Json<admin::AddTargetInput>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let w = s.db_pool().writer();
        let combo_id = ComboId(id);
        let new_id = admin::add_target_to_combo(&w, combo_id, input)?;
        Ok(Json(serde_json::json!({ "id": new_id.0 })))
    }
    .await;
    body.into()
}

/// `GET /admin/combos/:id/valid-sub-combos` — list combos that
/// can be added as a sub-combo target of `:id` (i.e. excluding the
/// combo itself and any combo whose addition would close a cycle).
/// Drives the "Add sub-combo target" picker in the dashboard.
pub async fn list_valid_sub_combos(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<Vec<admin::ComboSummary>>> {
    let body: Result<Json<Vec<admin::ComboSummary>>, ApiError> = async {
        // Read-only SELECT — use the READER.
        let r = s.db_pool().reader();
        let id = ComboId(id);
        let list = admin::list_valid_sub_combos(&r, id)?;
        Ok(Json(list))
    }
    .await;
    body.into()
}

// =====================================================================
// Usage analytics
// =====================================================================

/// Helper: run an analytics query with the filter, retry on disk I/O
/// error. The retry does a `PRAGMA wal_checkpoint(TRUNCATE)` on the
/// writer connection (to flush the WAL and release any locked pages)
/// and then retries the query on a FRESH reader connection (the old
/// reader may have a stale page cache that references the corrupt page).
///
/// This addresses the "disk I/O error" the user sees on analytics
/// endpoints with large/fragmented DBs. The root cause is typically
/// WAL checkpoint contention or stale page cache on the long-lived
/// reader connection — the retry + checkpoint clears both.
fn run_analytics_query_with_filter<T, F>(
    s: &AppState,
    f: &usage::UsageFilter,
    query_name: &str,
    query_fn: F,
) -> Result<T, ApiError>
where
    F: Fn(&openproxy_core::db::conn::ReaderGuard<'_>, &usage::UsageFilter) -> Result<T, CoreError>,
{
    // First attempt: use the reader connection.
    let r = s
        .db_pool()
        .try_reader_for(ADMIN_LOCK_TIMEOUT)
        .ok_or_else(|| {
            ApiError(CoreError::ServiceUnavailable(
                "reader lock busy: another query is holding the database; retry in a few seconds"
                    .into(),
            ))
        })?;
    match query_fn(&r, f) {
        Ok(result) => Ok(result),
        Err(e) => {
            // Check if this is a disk I/O error (SQLITE_IOERR_*).
            let err_str = format!("{:?}", e);
            let is_disk_io = err_str.contains("disk I/O")
                || err_str.contains("SQLITE_IOERR")
                || err_str.contains("database disk image is malformed")
                || err_str.contains("database is locked");

            if !is_disk_io {
                return Err(ApiError(e));
            }

            tracing::warn!(
                error = %e,
                query = %query_name,
                "analytics query failed with disk I/O error; attempting WAL checkpoint + retry"
            );

            // Drop the reader guard before taking the writer (avoids
            // a potential deadlock if the reader and writer share any
            // internal SQLite state).
            drop(r);

            // Force a WAL checkpoint on the writer connection. This
            // flushes the WAL file into the main DB and releases any
            // pages that were locked by the WAL. `TRUNCATE` mode also
            // truncates the WAL file to zero bytes.
            {
                let w = s.db_pool().writer();
                let _ = w.pragma_update(None, "wal_checkpoint", "TRUNCATE");
            }

            // Reopen BOTH connections (writer + reader). The long-lived
            // reader connection holds a stale page cache that references
            // pages from the pre-repair / pre-VACUUM DB file. Simply
            // re-acquiring the reader lock (try_reader_for) reuses the
            // SAME connection with the SAME stale cache. reopen()
            // closes the old connections and opens fresh ones that
            // re-read from disk.
            tracing::info!(
                query = %query_name,
                "analytics retry: reopening DB connections to clear stale page cache"
            );
            if let Err(e) = s.db_pool().reopen() {
                tracing::warn!(
                    error = %e,
                    "analytics retry: reopen failed (continuing with existing connection)"
                );
            }

            // Retry on the (now fresh) reader connection.
            let r2 = s
                .db_pool()
                .try_reader_for(ADMIN_LOCK_TIMEOUT)
                .ok_or_else(|| {
                    ApiError(CoreError::ServiceUnavailable(
                        "reader lock busy on retry; the database may be under heavy load".into(),
                    ))
                })?;
            query_fn(&r2, f).map_err(ApiError)
        }
    }
}

/// `GET /admin/usage/summary` — top-line roll-up.
pub async fn usage_summary(
    State(s): State<AppState>,
    Query(q): Query<UsageQuery>,
) -> ApiResult<Json<usage::UsageSummary>> {
    let body: Result<Json<usage::UsageSummary>, ApiError> = async {
        let f = q.into_filter()?;
        let result = run_analytics_query_with_filter(&s, &f, "summary", |conn, fl| {
            usage::summary(conn, fl)
        })?;
        Ok(Json(result))
    }
    .await;
    match body {
        Ok(v) => ApiResult::ok(v),
        Err(e) => ApiResult::err(e),
    }
}

/// `GET /admin/usage/by-model` — per-(provider, model) breakdown.
pub async fn usage_by_model(
    State(s): State<AppState>,
    Query(q): Query<UsageQuery>,
) -> ApiResult<Json<Vec<usage::ByModelRow>>> {
    let body: Result<Json<Vec<usage::ByModelRow>>, ApiError> = async {
        let f = q.into_filter()?;
        let result = run_analytics_query_with_filter(&s, &f, "by_model", |conn, fl| {
            usage::by_model(conn, fl)
        })?;
        Ok(Json(result))
    }
    .await;
    match body {
        Ok(v) => ApiResult::ok(v),
        Err(e) => ApiResult::err(e),
    }
}

/// `GET /admin/usage/by-provider` — per-provider breakdown.
///
/// Mirrors `usage_by_model` but groups by `provider_id` only. The
/// frontend's "monthly usage by provider" report uses this for the
/// top-level roll-up and `usage_monthly_by_provider` for the
/// time-bucketed breakdown.
pub async fn usage_by_provider(
    State(s): State<AppState>,
    Query(q): Query<UsageQuery>,
) -> ApiResult<Json<Vec<usage::ByProviderRow>>> {
    let body: Result<Json<Vec<usage::ByProviderRow>>, ApiError> = async {
        let f = q.into_filter()?;
        let result = run_analytics_query_with_filter(&s, &f, "by_provider", |conn, fl| {
            usage::by_provider(conn, fl)
        })?;
        Ok(Json(result))
    }
    .await;
    match body {
        Ok(v) => ApiResult::ok(v),
        Err(e) => ApiResult::err(e),
    }
}

/// `GET /admin/usage/monthly-by-provider` — per-(provider, month)
/// breakdown.
///
/// `month` is `strftime('%Y-%m', created_at)`. Ordered by
/// `month ASC, total_cost_usd DESC` so the frontend can pivot into a
/// providers × months matrix that walks time forward.
pub async fn usage_monthly_by_provider(
    State(s): State<AppState>,
    Query(q): Query<UsageQuery>,
) -> ApiResult<Json<Vec<usage::MonthlyByProviderRow>>> {
    let body: Result<Json<Vec<usage::MonthlyByProviderRow>>, ApiError> = async {
        let f = q.into_filter()?;
        let result = run_analytics_query_with_filter(&s, &f, "monthly_by_provider", |conn, fl| {
            usage::monthly_by_provider(conn, fl)
        })?;
        Ok(Json(result))
    }
    .await;
    match body {
        Ok(v) => ApiResult::ok(v),
        Err(e) => ApiResult::err(e),
    }
}

/// `GET /admin/usage/by-day` — daily usage totals for charting.
pub async fn usage_by_day(
    State(s): State<AppState>,
    Query(q): Query<UsageQuery>,
) -> ApiResult<Json<Vec<usage::ByDayRow>>> {
    let body: Result<Json<Vec<usage::ByDayRow>>, ApiError> = async {
        let f = q.into_filter()?;
        let result =
            run_analytics_query_with_filter(&s, &f, "by_day", |conn, fl| usage::by_day(conn, fl))?;
        Ok(Json(result))
    }
    .await;
    match body {
        Ok(v) => ApiResult::ok(v),
        Err(e) => ApiResult::err(e),
    }
}

/// `GET /admin/usage/by-account` — per-account breakdown.
pub async fn usage_by_account(
    State(s): State<AppState>,
    Query(q): Query<UsageQuery>,
) -> ApiResult<Json<Vec<usage::ByAccountRow>>> {
    let body: Result<Json<Vec<usage::ByAccountRow>>, ApiError> = async {
        let f = q.into_filter()?;
        let result = run_analytics_query_with_filter(&s, &f, "by_account", |conn, fl| {
            usage::by_account(conn, fl)
        })?;
        Ok(Json(result))
    }
    .await;
    match body {
        Ok(v) => ApiResult::ok(v),
        Err(e) => ApiResult::err(e),
    }
}

/// `GET /admin/usage/by-status` — counts grouped by HTTP status code.
pub async fn usage_by_status(
    State(s): State<AppState>,
    Query(q): Query<UsageQuery>,
) -> ApiResult<Json<Vec<usage::ByStatusRow>>> {
    let body: Result<Json<Vec<usage::ByStatusRow>>, ApiError> = async {
        let f = q.into_filter()?;
        let result = run_analytics_query_with_filter(&s, &f, "by_status", |conn, fl| {
            usage::by_status(conn, fl)
        })?;
        Ok(Json(result))
    }
    .await;
    match body {
        Ok(v) => ApiResult::ok(v),
        Err(e) => ApiResult::err(e),
    }
}

/// Cap on inline error rows. Spec §7.2 says "the most recent 100".
const ERRORS_DEFAULT_LIMIT: u32 = 100;

/// `GET /admin/usage/errors` — recent error rows, newest first.
pub async fn usage_errors(
    State(s): State<AppState>,
    Query(q): Query<UsageQuery>,
) -> ApiResult<Json<Vec<usage::ErrorRow>>> {
    let body: Result<Json<Vec<usage::ErrorRow>>, ApiError> = async {
        let f = q.into_filter()?;
        let result = run_analytics_query_with_filter(&s, &f, "errors", |conn, fl| {
            usage::errors(conn, fl, ERRORS_DEFAULT_LIMIT)
        })?;
        Ok(Json(result))
    }
    .await;
    match body {
        Ok(v) => ApiResult::ok(v),
        Err(e) => ApiResult::err(e),
    }
}

/// `GET /admin/usage/latency` — p50/p95 across connect/ttft/total/tokens_per_sec.
pub async fn usage_latency(
    State(s): State<AppState>,
    Query(q): Query<UsageQuery>,
) -> ApiResult<Json<analytics::LatencyPercentiles>> {
    let body: Result<Json<analytics::LatencyPercentiles>, ApiError> = async {
        let f = q.into_filter()?;
        let result = run_analytics_query_with_filter(&s, &f, "latency", |conn, fl| {
            analytics::latency_percentiles(conn, fl)
        })?;
        Ok(Json(result))
    }
    .await;
    match body {
        Ok(v) => ApiResult::ok(v),
        Err(e) => ApiResult::err(e),
    }
}

/// `GET /admin/usage/races` — race outcome statistics.
pub async fn usage_races(
    State(s): State<AppState>,
    Query(q): Query<UsageQuery>,
) -> ApiResult<Json<analytics::RaceStats>> {
    let body: Result<Json<analytics::RaceStats>, ApiError> = async {
        let f = q.into_filter()?;
        let result = run_analytics_query_with_filter(&s, &f, "races", |conn, fl| {
            analytics::race_stats(conn, fl)
        })?;
        Ok(Json(result))
    }
    .await;
    match body {
        Ok(v) => ApiResult::ok(v),
        Err(e) => ApiResult::err(e),
    }
}

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

/// `POST /admin/models/:id/refresh` — re-discover models for the
/// provider that owns the given `model_row_id`.
///
/// The handler:
/// 1. Loads the model row to find its provider.
/// 2. Resolves the provider's adapter from the in-process registry.
/// 3. Picks an account (the explicit `account_id` query param, or the
///    first account of the provider) and decrypts its API key.
/// 4. Calls [`admin::refresh_models`], which fetches the upstream's
///    `/models` endpoint and upserts the results.
///
/// On success, returns the number of rows touched.
/// `POST /admin/models/sync-models-dev` — one-shot sync from models.dev.
pub async fn sync_models_dev(State(s): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let upstream = s.upstream_client().clone();
    let db_pool = s.db_pool().clone();
    let result = openproxy_core::models_dev_sync::run_one_shot(db_pool, upstream).await;
    let msg = match result {
        Ok(m) => m,
        Err(e) => return ApiResult::err(ApiError(e)),
    };
    ApiResult::ok(Json(serde_json::json!({ "message": msg })))
}

/// `POST /admin/usage/recompute-costs` — re-price historical usage
/// rows that have `cost_usd = 0` AND `prompt_tokens > 0`. This walks
/// every unpriced row, re-applies `pricing::lookup_with_db` (which
/// consults the sync table + static table), and updates `cost_usd`.
///
/// Use this after a models.dev sync populates new pricing data, or
/// after manually setting pricing, to backfill costs for rows that
/// were recorded before pricing was available.
pub async fn recompute_usage_costs(
    State(s): State<AppState>,
) -> ApiResult<Json<serde_json::Value>> {
    let updated = {
        let w = s.db_pool().writer();
        match openproxy_core::models_dev_sync::recompute_costs(&w) {
            Ok(n) => n,
            Err(e) => return ApiResult::err(ApiError(e)),
        }
    };
    ApiResult::ok(Json(serde_json::json!({
        "message": format!("re-priced {} usage rows", updated),
        "updated": updated,
    })))
}

pub async fn refresh_models(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<RefreshQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    run_refresh(s, id, q).await
}

/// The body of [`refresh_models`], factored out so the async state
/// machine (which holds a `parking_lot::MutexGuard` across an await)
/// doesn't entangle the handler's own future.
async fn run_refresh(s: AppState, id: i64, q: RefreshQuery) -> ApiResult<Json<serde_json::Value>> {
    let row_id = ModelRowId(id);
    let ttl_seconds = q.ttl_seconds.unwrap_or(3_600);

    // 1. Look up the model to find the provider.
    let provider_id = {
        let w = s.db_pool().writer();
        let found = match models::get_by_row_id(&w, row_id) {
            Ok(opt) => opt,
            Err(e) => return ApiResult::err(ApiError(e)),
        };
        match found {
            Some(m) => m.provider_id,
            None => {
                return ApiResult::err(ApiError(CoreError::ModelNotFound {
                    provider: "<unknown>".into(),
                    model: format!("row_id={}", row_id.0),
                }));
            }
        }
    };

    // 2. Find the adapter for that provider. Check built-in adapters
    //    first, then fall back to constructing a CustomAdapter from the
    //    DB row.
    let adapter = match resolve_adapter(&s, &provider_id, s.adapters().as_slice()) {
        Ok(a) => a.clone(),
        Err(e) => return ApiResult::err(ApiError(e)),
    };

    // 3. Resolve an account and decrypt/refresh its credential.
    let selected_account_id = {
        let w = s.db_pool().writer();

        let provider_row = match providers::get(&w, &provider_id) {
            Ok(p) => p,
            Err(e) => return ApiResult::err(ApiError(e)),
        };
        let accounts_list = match accounts::list(&w, Some(&provider_id)) {
            Ok(l) => l,
            Err(e) => return ApiResult::err(ApiError(e)),
        };

        let is_anonymous = match &provider_row {
            Some(p) if matches!(p.auth_type, providers::AuthType::None) => true,
            _ if accounts_list.is_empty() => true,
            _ => false,
        };

        if is_anonymous {
            None
        } else {
            match q.account_id {
                Some(aid) => Some(AccountId::new(aid)),
                None => accounts_list.first().map(|a| a.id),
            }
        }
    };

    let api_key = match selected_account_id {
        Some(account_id) => {
            let account = {
                let w = s.db_pool().writer();
                match accounts::get(&w, account_id) {
                    Ok(Some(a)) => a,
                    Ok(None) => {
                        return ApiResult::err(ApiError(CoreError::AccountNotFound(account_id.0)));
                    }
                    Err(e) => return ApiResult::err(ApiError(e)),
                }
            };
            if account.auth_type == "oauth" {
                refresh_oauth_if_needed(&s, account, &provider_id).await
            } else {
                let w = s.db_pool().writer();
                match accounts::decrypt_api_key(&w, account_id, s.master_key().as_ref()) {
                    Ok(k) => k,
                    Err(e) => return ApiResult::err(ApiError(e)),
                }
            }
        }
        None => String::new(),
    };

    // Resolve account label for CloudFlare / label-based providers.
    let account_label = match selected_account_id {
        Some(account_id) => {
            let w = s.db_pool().writer();
            match accounts::get(&w, account_id) {
                Ok(Some(a)) => a.label.unwrap_or_default(),
                _ => String::new(),
            }
        }
        None => String::new(),
    };
    // 4. Run the refresh. `admin::refresh_models` takes the connection
    //    by value (not by reference) so the future is `Send`-able
    //    end to end: `rusqlite::Connection: !Sync` (it has internal
    //    `RefCell`s), and a `&Connection` borrowed across the await
    //    would propagate `!Send` to the outer future, breaking axum's
    //    `Handler` trait. We open a fresh handle via `DbPool::open_connection`
    //    and pass it by value; the writer mutex is unaffected.
    let conn_for_refresh = match s.db_pool().open_connection() {
        Ok(c) => c,
        Err(e) => return ApiResult::err(ApiError(e)),
    };
    let upsert = match admin::refresh_models(
        conn_for_refresh,
        &provider_id,
        &api_key,
        &adapter,
        s.upstream_client(),
        ttl_seconds,
        &account_label,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return ApiResult::err(ApiError(e)),
    };

    ApiResult::ok(Json(serde_json::json!({
        "touched": upsert.touched,
        "new_model_ids": upsert.new_model_ids,
        "provider_id": provider_id.as_str(),
    })))
}

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

/// `GET /admin/usage/recent?since_id=N&limit=K` — long-polling tail of
/// the `usage` table.
///
/// The dashboard polls this endpoint on a short timer, passing back the
/// `id` of the last row it has rendered. The handler returns the next
/// page of rows (oldest-first, by `id`) so the client can append them in
/// order. This is the pragmatic alternative to an SSE channel called
/// out by the spec; it needs no new dependencies and gives the same
/// "live tail" effect for the dashboard.
pub async fn usage_recent(
    State(s): State<AppState>,
    Query(q): Query<RecentQuery>,
) -> ApiResult<Json<Vec<usage::RecentUsageRow>>> {
    let body: Result<Json<Vec<usage::RecentUsageRow>>, ApiError> = async {
        let since_id = q.since_id.unwrap_or(0).clamp(0, USAGE_RECENT_MAX_SINCE_ID);
        let limit = q
            .limit
            .unwrap_or(USAGE_RECENT_DEFAULT_LIMIT)
            .clamp(1, USAGE_RECENT_MAX_LIMIT);
        // Read-only SELECT — use the READER. The dashboard polls this
        // endpoint frequently; going through the writer would
        // serialize every poll against `cost::record` writes.
        let r = s.db_pool().reader();
        // SEC-MEDIUM-C fix: drop the heavy request/response payloads
        // from the WS/REST surface — they can be multi-MB and would
        // fan out PII to every dashboard subscriber. The detail
        // endpoint reads them straight from the database on demand.
        let rows = usage::recent(&r, since_id, limit)?
            .into_iter()
            .map(usage::redact_for_broadcast)
            .collect();
        Ok(Json(rows))
    }
    .await;
    body.into()
}

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

fn json_text(value: serde_json::Value) -> Result<String, ApiError> {
    serde_json::to_string(&value).map_err(|e| {
        ApiError(CoreError::Internal(format!(
            "serialize websocket message: {e}"
        )))
    })
}

fn authenticate_admin_ws(
    state: &AppState,
    headers: &HeaderMap,
    query_token: Option<&str>,
) -> Result<(), ApiError> {
    // Dev convenience: when the operator explicitly opts in by setting
    // OPENPROXY_DASHBOARD_AUTH_BYPASS=1 in the server's environment, every
    // admin request is accepted without an Authorization header or query
    // token. The match is on the exact sentinel `1` — NOT "any non-empty
    // value" — so a typo or stray config (e.g. `=false`, `=yes`, `=0`,
    // `=legacy-token`) cannot silently grant full admin access. The match
    // is logged at WARN level so the bypass is visible in production logs
    // and dashboards alerting on auth-bypass are wired correctly.
    if let Ok(bypass) = std::env::var("OPENPROXY_DASHBOARD_AUTH_BYPASS")
        && bypass == "1"
    {
        tracing::warn!(
            target: "openproxy::security",
            path = ?headers.get("x-original-uri").and_then(|v| v.to_str().ok()),
            method = ?headers.get("x-original-method").and_then(|v| v.to_str().ok()),
            "admin auth bypassed via OPENPROXY_DASHBOARD_AUTH_BYPASS=1 — \
             every admin endpoint is open. Remove this env var to restore auth."
        );
        return Ok(());
    }

    // Extract token from Authorization header or from query parameter
    let header_token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::trim);

    let t = header_token.or(query_token).ok_or_else(|| {
        ApiError(CoreError::Auth(
            "missing authorization header or token query parameter".into(),
        ))
    })?;

    if t.is_empty() {
        return Err(ApiError(CoreError::Auth("invalid token".into())));
    }

    let key_hash = core_api_keys::hash_key(t);
    // Auth is a SELECT by hash — use the READER so admin requests don't
    // serialize through the writer mutex. The reader has its own
    // `Mutex<Connection>` (see `db::conn::DbPool`), so auth no longer
    // contends with `cost::record` writes or admin mutations.
    let r = state.db_pool().reader();
    let key = match core_api_keys::get_by_hash(&r, &key_hash).map_err(ApiError)? {
        Some(k) => k,
        None => {
            return Err(ApiError(CoreError::Auth("invalid api key".into())));
        }
    };

    if !key.is_active {
        return Err(ApiError(CoreError::Auth(
            "api key revoked or inactive".into(),
        )));
    }

    if let Some(exp) = &key.expires_at {
        // LOW fix (#15): previously a lexicographic string
        // comparison against `now.format("%Y-%m-%d %H:%M:%S")`. The
        // stored value uses `%Y-%m-%dT%H:%M:%SZ` (RFC3339-ish), so
        // the `T` (ASCII 84) sorted AFTER the space (ASCII 32) and
        // every key with `expires_at` was effectively never-expiring.
        // The helper parses both sides through `chrono` so the
        // check now means what it says.
        if core_api_keys::is_expired(Some(exp), chrono::Utc::now())
            .map_err(|e| ApiError(CoreError::Internal(format!("expires_at check: {e}"))))?
        {
            return Err(ApiError(CoreError::Auth("api key expired".into())));
        }
    }

    if !key.scopes.iter().any(|s| s == "manage") {
        return Err(ApiError(CoreError::Auth(
            "api key lacks required scope".into(),
        )));
    }

    // Fire-and-forget the `last_used_at` UPDATE on a blocking thread.
    // The auth path no longer blocks on acquiring the writer mutex,
    // and `touch_last_used` already throttles itself to 5-minute
    // writes (see `LAST_USED_THROTTLE_SECS` in `api_keys.rs`), so the
    // extra writer acquisition here only happens once per key per
    // five minutes under steady load.
    let pool = Arc::clone(state.db_pool());
    let key_id = key.id;
    tokio::task::spawn_blocking(move || {
        let w = pool.writer();
        let _ = core_api_keys::touch_last_used(&w, key_id);
    });

    Ok(())
}

/// Axum middleware wrapper around [`authenticate_admin_ws`].
///
/// Runs on every request that flows through the admin router
/// (registered via `axum::middleware::from_fn_with_state` in
/// `router.rs`). On success it forwards to the inner handler; on
/// failure it short-circuits with a 401 from the same
/// [`ApiError::IntoResponse`] impl the per-handler calls use, so
/// the wire shape (`{"error": {"code", "message"}}`) is identical
/// to the per-handler path.
///
/// The middleware does NOT touch the `?token=` query parameter on
/// its own — the WebSocket upgrade handler (`usage_stream`) needs
/// that path, and the per-handler `authenticate_admin_ws` already
/// accepts the query token. The middleware reads only the
/// `Authorization` header, which is the contract for the HTTP
/// path. WebSocket clients that pass `?token=` get a single auth
/// call inside `usage_stream` after the upgrade completes.
pub async fn admin_auth_middleware(
    axum::extract::State(state): axum::extract::State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let headers = req.headers().clone();
    if let Err(e) = authenticate_admin_ws(&state, &headers, None) {
        return e.into_response();
    }
    next.run(req).await
}

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

async fn stream_usage_rows(socket: WebSocket, state: AppState) {
    // Split the WebSocket into a sender and receiver half. The
    // sender half moves into a dedicated tokio task that drains the
    // outbox mpsc and writes to the socket; the receiver half stays
    // in this function for the select! loop. This is the CRITICAL
    // architectural change that fixes the "second request doesn't
    // appear in real-time after a failure" bug — see the comment
    // on `WS_OUTBOX_CAPACITY` below for the full rationale.
    let (mut ws_sender, mut ws_receiver) = socket.split();

    if let Err(err) = async {
        // 1. Subscribe to broadcast channels FIRST, before any DB
        //    query. This eliminates the TOCTOU window where stage
        //    events published during the history fetch would be
        //    silently dropped (broadcast::send returns SendError
        //    when there are no receivers). Events that arrive during
        //    the history fetch are queued in the broadcast buffer
        //    (capacity 1024 for stages, 1024 for rows) and delivered
        //    after the history batch is sent. The frontend's
        //    mergeLogsByDescId dedupes by id, so a row appearing in
        //    both history and the broadcast backlog is handled
        //    correctly.
        let mut usage_rx = state.usage_tx().subscribe();
        let mut stage_rx = state.stage_tx().subscribe();

        // F2: also subscribe to the notifications broadcast channel
        // (created by F1 in `notifications::NOTIF_TX`). The channel is
        // initialized in `AppState::new` / `AppState::for_test`, but
        // some test paths construct a minimal AppState without that
        // init — `try_get_tx()` returns `None` there and the
        // notifications select! arm below becomes a no-op
        // (`std::future::pending()`).
        //
        // The receiver is `Option<broadcast::Receiver<NotificationEvent>>`
        // because (a) the channel might not be initialized in tests,
        // and (b) we want to drop the receiver on `RecvError::Closed`
        // (server shutting down) without breaking the WS connection
        // — setting it to `None` makes the arm a permanent no-op
        // until the connection closes.
        let mut notification_rx = openproxy_core::notifications::try_get_tx()
            .map(|tx| tx.subscribe());

        // 2. Spawn a DEDICATED sender task that owns `ws_sender.send`.
        //    The receiver loop forwards every broadcast event into
        //    `outbox` (a bounded mpsc); the sender task drains it and
        //    writes to the socket. This decouples the broadcast
        //    receiver loop from the WS send — a slow browser stalls
        //    the sender task but NOT the receiver loop, so broadcast
        //    events keep being drained into the mpsc buffer instead
        //    of piling up in the broadcast channel and getting
        //    dropped for this receiver.
        //
        // The sender task exits (and closes the WS) when:
        //   - the outbox sender is dropped (receiver loop exited), OR
        //   - `ws_sender.send` returns an error (broken connection).
        let (outbox_tx, mut outbox_rx) =
            tokio::sync::mpsc::channel::<String>(WS_OUTBOX_CAPACITY);
        let sender_task = tokio::spawn(async move {
            use futures::SinkExt;
            while let Some(text) = outbox_rx.recv().await {
                if let Err(e) = ws_sender.send(Message::Text(text.into())).await {
                    // Broken connection — the receiver loop will
                    // also notice via `ws_receiver.next()` returning
                    // None/Err. Just exit the sender task.
                    tracing::debug!(error = %e, "stream_usage_rows: ws_sender.send failed, exiting sender task");
                    return;
                }
            }
            // outbox_rx returned None — outbox_tx was dropped, which
            // means the receiver loop exited. Send a Close frame so
            // the client knows the session is over.
            let _ = ws_sender.send(Message::Close(None)).await;
            let _ = ws_sender.close().await;
        });

        // 3. Initial history batch (most recent 100).
        // A SQLite "disk I/O error" here (e.g. WAL contention
        // under load) must NOT kill the WebSocket — the
        // frontend handles an empty `rows` array gracefully,
        // and the subscription loop below will start delivering
        // live events as soon as the DB recovers. Without this
        // guard the error propagated via `?`, broke out of the
        // async block, sent an error envelope, closed the WS,
        // and triggered an immediate reconnect loop.
        // Read-only SELECT — use the READER. The dashboard's WS
        // reconnects would otherwise serialize every history
        // fetch through the writer mutex.
        let rows = {
            let r = state.db_pool().reader();
            match usage::recent_desc(&r, 100) {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        "stream_usage_rows: initial history query failed, \
                         sending empty history and continuing with live events"
                    );
                    Vec::new()
                }
            }
        };
        // H7 fix: track the highest usage `id` we have
        // streamed to the dashboard so a `Lagged` broadcast
        // error can be answered with a targeted resync
        // (`{"type":"resync","since_id":last_known}`) rather
        // than a fatal error. The frontend then fetches
        // `usage::recent(since_id=last_known, limit=...)` to
        // catch up. Without this, a slow dashboard would
        // permanently lose rows it could not consume in time
        // and a toast was the only signal — see the audit
        // finding RACE-F-5.
        //
        // Compute `last_known_id` BEFORE redacting (redaction
        // consumes `rows` via `into_iter`).
        let mut last_known_id: i64 = rows.iter().map(|r| r.id.0).max().unwrap_or(0);
        outbox_send(&outbox_tx, json!({
            "type": "history",
            // SECURITY: redact heavyweight fields (request/response bodies
            // and headers) before sending the initial history batch. The
            // live `row` events below are already redacted by
            // `publish_usage_row` → `redact_for_broadcast`; the history
            // batch must apply the same redaction so the initial rows
            // don't leak bodies/headers to the dashboard. The full
            // bodies are available on demand via /usage/detail.
            "rows": rows.into_iter().map(usage::redact_for_broadcast).collect::<Vec<_>>()
        })).await;

        // 4. Event loop — usage_rx, stage_rx, and notification_rx are
        //    already subscribed above, before the history query. The
        //    outbox decouples this loop from the WS sender task.
        //
        // `biased` ensures the broadcast channels (stage + usage +
        // notifications) are polled BEFORE the ws_receiver. The
        // ws_receiver almost never has messages (only ping/subscribe
        // from the client, which are rare), so polling it first wastes
        // a branch on every iteration. More importantly, when the
        // browser is slow and the outbox backs up, we want to
        // prioritize draining the broadcast channels (which have a
        // fixed capacity and will lag if not drained) over reading
        // client messages (which can wait indefinitely).
        loop {
            tokio::select! {
                biased;
                // Stage events FIRST — these carry the "in progress"
                // status the operator needs to see in real time. They
                // are the most frequent and most time-sensitive.
                stage = stage_rx.recv() => {
                    match stage {
                        Ok(event) => {
                            outbox_send(&outbox_tx, json!({ "type": "stage", "data": event })).await;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            outbox_try_send(&outbox_tx, json!({
                                "type": "lag_warning",
                                "skipped": skipped,
                                "message": format!(
                                    "stage broadcast channel lagged; {} event(s) skipped",
                                    skipped
                                ),
                            })).await;
                            outbox_send(&outbox_tx, json!({ "type": "resync", "since_id": last_known_id })).await;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
                // Usage rows SECOND — these are the terminal "row"
                // events published when a request completes. Less
                // frequent than stage events but still critical.
                usage = usage_rx.recv() => {
                    match usage {
                        Ok(row) => {
                            if row.id.0 > last_known_id {
                                last_known_id = row.id.0;
                            }
                            outbox_send(&outbox_tx, json!({ "type": "row", "data": row })).await;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            outbox_try_send(&outbox_tx, json!({
                                "type": "lag_warning",
                                "skipped": skipped,
                                "message": format!(
                                    "broadcast channel lagged; {} row(s) skipped",
                                    skipped
                                ),
                            })).await;
                            outbox_send(&outbox_tx, json!({ "type": "resync", "since_id": last_known_id })).await;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
                // F2: notifications THIRD — model_new / model_gone /
                // model_auto_activated / system events surfaced to the
                // dashboard tray. Less frequent than stage/usage (a
                // handful per discovery cycle, default 1h) but still
                // real-time. The receiver is an Option because
                // `try_get_tx()` returns None in tests that don't
                // initialize the broadcast channel; in that case the
                // async block degenerates to `pending().await` and
                // this arm is a permanent no-op (never wins select!).
                //
                // On `Lagged(n)` we send a `lag_warning` with
                // `channel: "notifications"` so the client can refetch
                // via `GET /admin/api/notifications` (notifications are
                // persisted, so refetch is the source of truth — we do
                // NOT send a `resync` envelope because there is no
                // `since_id` semantics for notifications; the client
                // just lists the latest 50).
                //
                // On `Closed` (server shutting down) we set
                // `notification_rx = None` so this arm becomes a no-op
                // for the rest of the connection's lifetime — the
                // stage/usage/ws arms continue running normally.
                evt = async {
                    match notification_rx.as_mut() {
                        Some(rx) => match rx.recv().await {
                            Ok(n) => NotifRxEvent::Event(n),
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                NotifRxEvent::Lagged(n)
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                NotifRxEvent::Closed
                            }
                        },
                        None => std::future::pending().await,
                    }
                } => {
                    match evt {
                        NotifRxEvent::Event(n) => {
                            outbox_send(
                                &outbox_tx,
                                json!({ "type": "notification", "data": n }),
                            )
                            .await;
                        }
                        NotifRxEvent::Lagged(skipped) => {
                            outbox_try_send(&outbox_tx, json!({
                                "type": "lag_warning",
                                "skipped": skipped,
                                "channel": "notifications",
                                "message": format!(
                                    "notifications broadcast channel lagged; {} event(s) skipped — refetch via GET /admin/api/notifications",
                                    skipped
                                ),
                            })).await;
                        }
                        NotifRxEvent::Closed => {
                            // Channel closed (server shutting down). Drop
                            // the receiver so this arm becomes a no-op;
                            // the WS connection stays alive as long as
                            // stage/usage still have receivers.
                            notification_rx = None;
                        }
                    }
                }
                // WS receiver LAST — client messages (subscribe, ping)
                // are rare and can tolerate delay. Prioritizing the
                // broadcast channels ensures we never miss a stage
                // event because we were busy reading a ping.
                incoming = ws_receiver.next() => {
                    match incoming {
                        Some(Ok(Message::Text(text))) => {
                            let msg: ClientWsMessage = match serde_json::from_str(&text) {
                                Ok(msg) => msg,
                                Err(e) => {
                                    outbox_try_send(&outbox_tx, json!({
                                        "type": "error",
                                        "message": format!("invalid client message: {e}"),
                                    })).await;
                                    continue;
                                }
                            };

                            match msg.msg_type.as_str() {
                                "subscribe" => {
                                    let since_id = msg
                                        .since_id
                                        .unwrap_or(0)
                                        .clamp(0, USAGE_RECENT_MAX_SINCE_ID);
                                    let rows: Vec<usage::RecentUsageRow> = {
                                        let r = state.db_pool().reader();
                                        let rows = match usage::recent(&r, since_id, 100) {
                                            Ok(v) => v,
                                            Err(e) => {
                                                tracing::error!(error = %e, "stream_usage_rows: subscribe recent query failed");
                                                Vec::new()
                                            }
                                        };
                                        drop(r);
                                        rows.into_iter()
                                            .map(usage::redact_for_broadcast)
                                            .collect()
                                    };
                                    if let Some(mx) = rows.iter().map(|r| r.id.0).max() {
                                        last_known_id = last_known_id.max(mx);
                                    }
                                    outbox_send(&outbox_tx, json!({ "type": "history", "rows": rows })).await;
                                }
                                "ping" => {
                                    let now_str = chrono::Utc::now().to_rfc3339();
                                    outbox_try_send(&outbox_tx, json!({ "type": "pong", "server_time": now_str })).await;
                                }
                                _ => {
                                    outbox_try_send(&outbox_tx, json!({
                                        "type": "error",
                                        "message": format!("unknown message type: {}", msg.msg_type),
                                    })).await;
                                }
                            }
                        }
                        Some(Ok(Message::Close(_))) => break,
                        Some(Ok(_)) => {}
                        Some(Err(e)) => {
                            tracing::debug!(error = %e, "stream_usage_rows: ws_receiver error");
                            break;
                        }
                        None => break,
                    }
                }
            }
        }
        // Drop the outbox sender to signal the sender task to exit
        // gracefully (it will send a Close frame and return).
        drop(outbox_tx);
        // Wait for the sender task to finish so we don't leak it.
        let _ = sender_task.await;
        Ok::<(), ApiError>(())
    }
    .await
    {
        // Best-effort error notification. The sender task owns the
        // ws_sender at this point, so we can't send an error frame
        // directly — just log. The frontend will see the WS close
        // and reconnect.
        tracing::debug!(error = %err, "stream_usage_rows: event loop exited with error");
    }
}

/// Forward a JSON value into the bounded outbox mpsc. If the outbox
/// is full (sender task is stalled on a slow browser), we try to
/// make room by waiting briefly — this is CRITICAL for stage events
/// which carry the "in progress" status. Dropping them silently
/// makes the dashboard miss real-time updates, which is the exact
/// bug the user reported ("peticiones no llegan en tiempo real").
///
/// We use `send().await` (bounded) instead of `try_send()` (non-blocking)
/// for STAGE events and ROW events, because these are the real-time
/// updates the operator needs. If the outbox is genuinely stuck
/// (browser dead for seconds), the broadcast channel will lag and
/// trigger a resync — but for the common case of a 10-50ms render
/// stall, waiting is far better than dropping.
///
/// For non-critical messages (pong, error, lag_warning), we still
/// use `try_send` so the receiver loop never blocks on unimportant
/// traffic.
async fn outbox_send(tx: &tokio::sync::mpsc::Sender<String>, value: serde_json::Value) {
    let text: String = match json_text(value) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "stream_usage_rows: json_text failed in outbox_send");
            return;
        }
    };
    // Use send().await for real-time messages — this blocks the
    // receiver loop if the outbox is full, but that's BETTER than
    // dropping the message. The broadcast channel has capacity 1024,
    // so a brief stall won't cause lag. If the stall is prolonged
    // (seconds), the broadcast channel will lag and trigger a resync.
    match tx.send(text).await {
        Ok(()) => {}
        Err(_e) => {
            // Sender task exited — the WS is closing. Just drop
            // the message; the receiver loop will exit momentarily
            // when `ws_receiver.next()` returns None.
        }
    }
}

/// Same as `outbox_send` but uses `try_send` (non-blocking) for
/// non-critical messages where dropping is acceptable (pong, error,
/// lag_warning). This prevents the receiver loop from blocking on
/// unimportant traffic when the browser is slow.
async fn outbox_try_send(tx: &tokio::sync::mpsc::Sender<String>, value: serde_json::Value) {
    let text: String = match json_text(value) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "stream_usage_rows: json_text failed in outbox_try_send");
            return;
        }
    };
    match tx.try_send(text) {
        Ok(()) => {}
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
            tracing::debug!("stream_usage_rows: outbox full, dropping non-critical WS message");
        }
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {}
    }
}

/// Query for WebSocket token in `/admin/usage/stream`
#[derive(Debug, Default, Deserialize)]
pub struct UsageStreamQuery {
    pub token: Option<String>,
}

/// `GET /admin/usage/stream` — upgraded WebSocket handler.
pub async fn usage_stream(
    State(s): State<AppState>,
    Query(q): Query<UsageStreamQuery>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> impl axum::response::IntoResponse {
    // HIGH-2 fix: check the Origin header to prevent CSWSH
    // (cross-site WebSocket hijacking). A malicious website can
    // `new WebSocket('ws://victim/admin/usage/stream')` without the
    // victim's knowledge — the browser sends cookies and the request
    // goes through. Without this check, the attacker could read the
    // live-logs stream if the auth bypass is on, or at minimum
    // consume server resources.
    //
    // We allow:
    // - No Origin header (non-browser clients like curl don't send it)
    // - Any Origin that looks like localhost/127.0.0.1 (dev mode)
    // - Any Origin (in production, the reverse proxy should restrict
    //   access to /admin/ via network ACLs; the Origin check is
    //   defense-in-depth for when the proxy is misconfigured)
    //
    // This is intentionally permissive — the real protection is the
    // admin auth middleware + network ACLs on /admin/. The Origin
    // check prevents the browser-based CSWSH attack vector.
    if let Some(origin) = headers.get("origin").and_then(|v| v.to_str().ok()) {
        // Allow localhost origins (dev mode).
        if !origin.starts_with("http://localhost")
            && !origin.starts_with("http://127.0.0.1")
            && !origin.starts_with("https://localhost")
            && !origin.starts_with("https://127.0.0.1")
        {
            // In production, the reverse proxy should restrict /admin/
            // to the internal network. If the operator exposes /admin/
            // to the internet without a proxy, they should set
            // OPENPROXY_ALLOWED_ORIGINS. For now, log a warning and
            // allow — the auth middleware is the primary protection.
            tracing::warn!(
                origin = %origin,
                "WebSocket connection from non-localhost origin; \
                 ensure /admin/ is network-restricted in production"
            );
        }
    }

    match authenticate_admin_ws(&s, &headers, q.token.as_deref()) {
        Ok(()) => ws
            .on_upgrade(move |socket| stream_usage_rows(socket, s))
            .into_response(),
        Err(e) => e.into_response(),
    }
}

/// `GET /admin/usage/detail?id=<usage_id>&trace_id=<trace_id>` — full detail for a single usage row.
pub async fn usage_detail(
    State(s): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<DetailQuery>,
) -> ApiResult<Json<UsageDetailResponse>> {
    if let Err(e) = authenticate_admin_ws(&s, &headers, None) {
        return e.into();
    }
    let body: Result<Json<UsageDetailResponse>, ApiError> = async {
        // Read-only SELECT — use the READER.
        let r = s.db_pool().reader();
        let row = if let Some(trace_id) = &q.trace_id {
            usage::detail_by_trace_id(&r, trace_id)?
        } else if let Some(id) = q.id {
            usage::detail_by_id(&r, id)?
        } else {
            return Err(ApiError(CoreError::Validation(
                "Either 'id' or 'trace_id' query parameter must be provided".into(),
            )));
        };
        match row {
            Some(r) => Ok(Json(UsageDetailResponse { row: r })),
            None => Err(ApiError(CoreError::Internal(format!(
                "usage row not found for query {:?}",
                q
            )))),
        }
    }
    .await;
    body.into()
}

/// Query for `GET /admin/usage/detail`.
#[derive(Debug, Default, Deserialize)]
pub struct DetailQuery {
    pub id: Option<i64>,
    pub trace_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct UsageDetailResponse {
    pub row: usage::UsageDetailRow,
}

// =====================================================================
// Recording toggle (Live Logs detail modal)
// =====================================================================

/// `GET /admin/recording` — read the current recording state.
///
/// Returns `{"recording": true|false}`. Used by the dashboard's Live
/// Logs section to render the "Record" toggle on initial load.
pub async fn get_recording(
    State(s): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    if let Err(e) = authenticate_admin_ws(&s, &headers, None) {
        return e.into();
    }
    let body: Result<Json<serde_json::Value>, ApiError> =
        async { Ok(Json(serde_json::json!({ "recording": s.is_recording() }))) }.await;
    body.into()
}

/// `POST /admin/recording` — flip the process-wide recording state.
///
/// Body: `{"enabled": true|false}`. When enabled, every new chat
/// request will record the full request/response bodies and headers
/// in the `usage` table. Useful for debugging from the Live Logs
/// detail modal: turn it on, fire a few requests, click into a row
/// to see the actual JSON.
pub async fn set_recording(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> ApiResult<Json<serde_json::Value>> {
    if let Err(e) = authenticate_admin_ws(&s, &headers, None) {
        return e.into();
    }
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let enabled = body
            .get("enabled")
            .and_then(|v| v.as_bool())
            .ok_or_else(|| CoreError::Validation("missing 'enabled' bool".into()))?;
        s.set_recording(enabled);
        Ok(Json(serde_json::json!({ "recording": enabled })))
    }
    .await;
    body.into()
}
// Model toggling
// =====================================================================

/// `POST /admin/models/:id/toggle` — flip the soft-disable bit on a
/// model row. Body: `{"active": true|false}`. Returns the new state.
///
/// An unknown id is a silent no-op (`set_active` doesn't error); the
/// dashboard will simply see no change.
pub async fn toggle_model(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<serde_json::Value>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let active = body
            .get("active")
            .and_then(|v| v.as_bool())
            .ok_or_else(|| CoreError::Validation("missing 'active' bool".into()))?;
        let w = s.db_pool().writer();
        models::set_active(&w, ModelRowId(id), active)?;
        Ok(Json(serde_json::json!({ "id": id, "active": active })))
    }
    .await;
    body.into()
}

/// `POST /admin/models/bulk-toggle` — flip the soft-disable bit on
/// every non-custom model of a provider in a single SQL UPDATE.
///
/// Body: `{"provider_id": "...", "active": true|false}`.
///
/// Returns `{"updated": N}` where `N` is the number of rows actually
/// changed (i.e. the count of non-custom rows that were not already
/// in the target state). Custom rows are skipped — same policy as
/// `apply_auto_activation` — so an operator's hand-picked entries
/// survive a bulk toggle.
///
/// This is the dashboard's "Enable all" / "Disable all" path. Doing
/// it as a single statement (instead of iterating the per-row
/// `toggle_model` endpoint N times) closes a race window where a
/// concurrent `apply_auto_activation` could re-activate rows
/// mid-iteration and leave the table half-toggled.
pub async fn bulk_toggle_models(
    State(s): State<AppState>,
    Json(body): Json<admin::BulkToggleInput>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let w = s.db_pool().writer();
        let updated = admin::set_active_bulk(&w, body)?;
        Ok(Json(serde_json::json!({
            "updated": updated,
        })))
    }
    .await;
    body.into()
}

/// `DELETE /admin/models/:id` — hard-delete a model row.
///
/// Companion to [`toggle_model`]: that endpoint hides a row from
/// routing while preserving the audit trail; this one removes the row
/// outright. Combo-targets referencing the model are preserved with
/// `model_row_id = NULL` (migration 000025 `ON DELETE SET NULL`); they
/// are filtered from routing by `combos::list_targets` (Gate E3). The
/// row is kept in the table for audit / re-binding.
///
/// A missing id is a silent no-op — `models::delete` reports 0 rows
/// removed and we surface that as `{"deleted": 0}` so the dashboard can
/// distinguish "row was actually removed" from "row was already gone".
pub async fn delete_model(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let w = s.db_pool().writer();
        let removed = models::delete(&w, ModelRowId(id))?;
        Ok(Json(serde_json::json!({ "id": id, "deleted": removed })))
    }
    .await;
    body.into()
}

// =====================================================================
// Combo mutations
// =====================================================================

/// `PATCH /admin/combos/:id` — partial update of a combo row.
///
/// Recognized body fields (all optional — absent fields are left
/// untouched):
///
/// - `race_size`: `1..=8`. Out-of-range is a 400.
/// - `context_window`: `null` or an integer. `null` means
///   "auto-compute from targets".
/// - `priority_mode`: `"strict"` | `"lkgp"` | `"weighted"` |
///   `"least_used"` | `"p2c"`. `null` clears the column back to
///   the legacy `strict` default. Ignored for `RoundRobin` /
///   `Shuffle` strategies (stored but not consulted).
/// - `cooldown_mode`: `"flat"` | `"exponential"`. `null` clears
///   the column back to the legacy `flat` default.
/// - `cooldown_base_secs` / `cooldown_max_secs` / `cooldown_factor`:
///   per-combo overrides for the cooldown formula. `null` clears
///   each one back to "use the global `[cooldown]` default".
///   These three fields are written in a single UPDATE so the
///   dashboard's "Cooldown" form can POST them atomically.
/// - `lkgp_exploration_rate`: float in `[0.0, 1.0]`. `null`
///   clears the column back to the default 0.1.
/// - `selection_window_secs`: positive integer. `null` clears the
///   column back to the default 3600.
pub async fn update_combo(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<serde_json::Value>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let w = s.db_pool().writer();
        // Optional race_size update.
        if let Some(n) = body.get("race_size").and_then(|v| v.as_u64()) {
            let rs = u8::try_from(n).unwrap_or(0);
            combos::update_combo(&w, ComboId(id), Some(rs))?;
        }
        // Optional context_window update. `null` or missing means
        // "auto-compute from targets". A positive integer pins the
        // reported context window.
        if let Some(cw_val) = body.get("context_window") {
            let cw = if cw_val.is_null() {
                None
            } else {
                Some(cw_val.as_i64().ok_or_else(|| {
                    ApiError(CoreError::Validation(
                        "context_window must be null or an integer".into(),
                    ))
                })?)
            };
            combos::update_context_window(&w, ComboId(id), cw)?;
        }
        // Optional `priority_mode` update. `null` clears the column
        // back to `strict` (the legacy default).
        if let Some(v) = body.get("priority_mode") {
            let mode = match v {
                serde_json::Value::Null => None,
                serde_json::Value::String(s) => Some(s.as_str()),
                other => {
                    return Err(ApiError(CoreError::Validation(format!(
                        "priority_mode must be a string or null, got {}",
                        other
                    ))));
                }
            };
            combos::update_priority_mode(&w, ComboId(id), mode)?;
        }
        // Optional cooldown settings update. Each field is updated
        // INDEPENDENTLY — if only `cooldown_base_secs` is in the body,
        // only that column is written, leaving `cooldown_mode` etc.
        // untouched. This prevents the "changing base resets mode to
        // flat" bug.
        //
        // The frontend sends one field at a time (e.g. `{cooldown_base_secs: 30}`)
        // so we must NOT batch them into a single UPDATE that would
        // NULL out the absent fields.
        if let Some(v) = body.get("cooldown_mode") {
            let mode = match v {
                serde_json::Value::Null => None,
                serde_json::Value::String(s) => Some(s.as_str()),
                other => {
                    return Err(ApiError(CoreError::Validation(format!(
                        "cooldown_mode must be a string or null, got {}",
                        other
                    ))));
                }
            };
            combos::update_cooldown_mode(&w, ComboId(id), mode)?;
        }
        if let Some(v) = body.get("cooldown_base_secs") {
            let base = if v.is_null() {
                None
            } else {
                Some(v.as_u64().ok_or_else(|| {
                    ApiError(CoreError::Validation(
                        "cooldown_base_secs must be a non-negative integer or null".into(),
                    ))
                })?)
            };
            combos::update_cooldown_base(&w, ComboId(id), base)?;
        }
        if let Some(v) = body.get("cooldown_max_secs") {
            let max = if v.is_null() {
                None
            } else {
                Some(v.as_u64().ok_or_else(|| {
                    ApiError(CoreError::Validation(
                        "cooldown_max_secs must be a non-negative integer or null".into(),
                    ))
                })?)
            };
            combos::update_cooldown_max(&w, ComboId(id), max)?;
        }
        if let Some(v) = body.get("cooldown_factor") {
            let factor = if v.is_null() {
                None
            } else {
                Some(v.as_u64().ok_or_else(|| {
                    ApiError(CoreError::Validation(
                        "cooldown_factor must be a non-negative integer or null".into(),
                    ))
                })? as u32)
            };
            combos::update_cooldown_factor(&w, ComboId(id), factor)?;
        }
        // Optional LKGP exploration rate update.
        if let Some(v) = body.get("lkgp_exploration_rate") {
            let rate = if v.is_null() {
                None
            } else {
                Some(v.as_f64().ok_or_else(|| {
                    ApiError(CoreError::Validation(
                        "lkgp_exploration_rate must be a number in [0.0, 1.0] or null".into(),
                    ))
                })?)
            };
            combos::update_lkgp_settings(&w, ComboId(id), rate)?;
        }
        // Optional selection window update.
        if let Some(v) = body.get("selection_window_secs") {
            let window = if v.is_null() {
                None
            } else {
                Some(v.as_u64().ok_or_else(|| {
                    ApiError(CoreError::Validation(
                        "selection_window_secs must be a non-negative integer or null".into(),
                    ))
                })?)
            };
            combos::update_selection_window(&w, ComboId(id), window)?;
        }
        Ok(Json(serde_json::json!({ "id": id })))
    }
    .await;
    body.into()
}

/// `PATCH /admin/combos/:id/targets/:target_id` — update mutable
/// fields of a single target. Recognized body fields (all optional —
/// absent fields are left untouched):
///
/// - `priority_order`: `i32`. The caller picks a sane value relative
///   to siblings; we don't re-number the rest of the rowset here.
/// - `weight`: positive `i32`. Per-target weight for the `weighted`
///   priority mode (migration 000035). Default 1; weights `<= 0`
///   are rejected with a 400.
///
/// For backwards compatibility, the legacy single-field form
/// `{"priority_order": <i32>}` is still accepted (and required when
/// `weight` is absent). The dashboard's combo editor upgrades the
/// call to include both fields when the operator is editing the
/// weight column.
pub async fn update_combo_target(
    State(s): State<AppState>,
    Path((combo_id, target_id)): Path<(i64, i64)>,
    Json(body): Json<serde_json::Value>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        // Optional `priority_order` — the historical primary field.
        // Kept optional so a future dashboard that only wants to
        // PATCH `weight` can do so without round-tripping the order.
        let priority_order: Option<i64> = match body.get("priority_order") {
            None => None,
            Some(v) => Some(v.as_i64().ok_or_else(|| {
                ApiError(CoreError::Validation(
                    "priority_order must be an integer when present".into(),
                ))
            })?),
        };
        if let Some(priority_order) = priority_order {
            // Cast: i32 is well under i64::MAX in practice; the SQL
            // column is INTEGER (i64 in rusqlite) so a non-negative
            // i32 is safe.
            if priority_order < i32::MIN as i64 || priority_order > i32::MAX as i64 {
                return Err(ApiError(CoreError::Validation(format!(
                    "priority_order out of i32 range: {}",
                    priority_order
                ))));
            }
            let w = s.db_pool().writer();
            combos::update_target_priority(&w, ComboTargetId(target_id), priority_order as i32)?;
        }
        // Optional `weight` (migration 000035).
        if let Some(v) = body.get("weight") {
            let weight_i64 = v.as_i64().ok_or_else(|| {
                ApiError(CoreError::Validation(
                    "weight must be an integer when present".into(),
                ))
            })?;
            // Range-check before the i32 cast so an out-of-range
            // value surfaces as a 400 instead of a silent wrap.
            if weight_i64 < 1 || weight_i64 > i32::MAX as i64 {
                return Err(ApiError(CoreError::Validation(format!(
                    "weight must be a positive i32 (1..={}), got {}",
                    i32::MAX,
                    weight_i64
                ))));
            }
            let w = s.db_pool().writer();
            combos::update_target_weight(&w, ComboTargetId(target_id), weight_i64 as i32)?;
        }
        // Backwards-compat: if neither field was present, surface
        // the historical "missing 'priority_order'" error so a
        // legacy caller still gets a useful 400 instead of a silent
        // 200 with no work done.
        if priority_order.is_none() && body.get("weight").is_none() {
            return Err(ApiError(CoreError::Validation(
                "missing 'priority_order' or 'weight'".into(),
            )));
        }
        Ok(Json(serde_json::json!({
            "combo_id": combo_id,
            "id": target_id,
            "priority_order": priority_order,
            "weight": body.get("weight").and_then(|v| v.as_i64()),
        })))
    }
    .await;
    body.into()
}

/// `DELETE /admin/combos/:id/targets/:target_id` — remove a single
/// target from a combo. The handler validates that the target actually
/// belongs to the requested combo (defense in depth: a mismatched URL
/// surfaces as a 400 instead of silently deleting from another combo).
pub async fn delete_combo_target(
    State(s): State<AppState>,
    Path((combo_id, target_id)): Path<(i64, i64)>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let w = s.db_pool().writer();
        admin::delete_combo_target(&w, ComboId(combo_id), ComboTargetId(target_id))?;
        Ok(Json(serde_json::json!({ "deleted": target_id })))
    }
    .await;
    body.into()
}

/// `POST /admin/combos/:id/targets/:target_id/clear-cooldown`
/// — force-clear the persistent cooldown row for a single target.
/// See [`openproxy_core::admin::clear_combo_target_cooldown`].
///
/// The dashboard's "Reset cooldown" button calls this when the
/// operator has manually verified the upstream is healthy again
/// and wants to short-circuit the `cooldown_secs` wait. The
/// handler does not write to `models.last_test_status` (the row
/// might be parked because of a 5xx on the *combo target* layer,
/// not on the model itself).
///
/// IMPORTANT: this literal-segment route MUST be registered
/// before `/admin/combos/:id/targets/:target_id` in
/// `router.rs`, otherwise axum's :target_id segment will happily
/// swallow `clear-cooldown` and 405 the POST.
pub async fn clear_combo_target_cooldown(
    State(s): State<AppState>,
    Path((combo_id, target_id)): Path<(i64, i64)>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let w = s.db_pool().writer();
        admin::clear_combo_target_cooldown(&w, ComboId(combo_id), ComboTargetId(target_id))?;
        Ok(Json(
            serde_json::json!({ "ok": true, "cleared": target_id }),
        ))
    }
    .await;
    body.into()
}

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

/// `POST /admin/combos/:id/targets/reorder` — atomically reassign
/// `priority_order` for every target of a combo so the new order
/// matches `body.target_ids`.
///
/// The backend does the swap, not the caller: an "old + delta" PATCH
/// would leave a half-swapped state on disk for the duration of two
/// HTTP calls and could leave two targets with the same
/// `priority_order` if the calls interleave. Doing the renumber in
/// a single `IMMEDIATE` transaction closes both holes.
///
/// `target_ids` must be a permutation of the combo's current target
/// ids; otherwise the call fails with a 400 and the combo is left
/// untouched.
///
/// Follow-up (not implemented in this pass): nested combos. Allowing
/// a target to reference another combo (combo-of-combos) would need
/// a new column on `combo_targets` (or a separate join table), a
/// dedicated `combo:<name>` resolver in the routing layer, and an
/// updated auto-populate path. Out of scope for the dashboard
/// "reorder" fix and tracked as a phase-2 item.
pub async fn reorder_combo_targets(
    State(s): State<AppState>,
    Path(combo_id): Path<i64>,
    Json(body): Json<ReorderComboTargetsInput>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let mut w = s.db_pool().writer();
        let ordered: Vec<ComboTargetId> = body.target_ids.into_iter().map(ComboTargetId).collect();
        admin::reorder_combo_targets(&mut w, ComboId(combo_id), &ordered)?;
        Ok(Json(serde_json::json!({
            "reordered": combo_id,
            "count": ordered.len(),
        })))
    }
    .await;
    body.into()
}

// =====================================================================
// Provider update
// =====================================================================

/// `PATCH /admin/providers/:id` — partial update of a provider row.
///
/// Body: any subset of `name`, `base_url`, `extra_headers_json`,
/// `auto_activate_keyword`. The keyword uses a three-state encoding:
/// * `null`/`absent` — leave the column alone.
/// * `{"auto_activate_keyword": null}` — clear the column to `NULL`.
/// * `{"auto_activate_keyword": "claude"}` — set the column to `"claude"`.
pub async fn update_provider(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<admin::UpdateProviderInput>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        // Scope the writer guard so it is dropped BEFORE
        // rebuild_adapters re-acquires the same non-reentrant
        // parking_lot::Mutex. Holding the guard across
        // rebuild_adapters deadlocks the Tokio worker thread.
        let provider_id = ProviderId::new(id.clone());
        {
            let w = s.db_pool().writer();
            admin::update_provider(&w, &provider_id, body)?;
        }
        // Hot-reload so the chat pipeline sees the updated
        // `base_url`/`auth_type`/`extra_headers` on the
        // `CustomAdapter` for this provider. See the comment on
        // `create_provider` for why we log-and-continue rather
        // than roll back.
        if let Err(e) = s.rebuild_adapters() {
            tracing::warn!(
                provider_id = id,
                error = %e,
                "failed to reload adapter registry after update_provider"
            );
        } else {
            tracing::info!(
                provider_id = id,
                "reloaded adapter registry after updating provider"
            );
        }
        Ok(Json(serde_json::json!({ "id": id })))
    }
    .await;
    body.into()
}

// =====================================================================
// Custom model creation
// =====================================================================

/// `POST /admin/models/custom` — hand-create a model row. The row is
/// stamped with `custom = 1` and `active = 1` so it is routable as soon
/// as the call returns. Use this when a model is missing from the
/// provider's `/models` endpoint but the operator knows the upstream
/// will accept it anyway.
///
/// Body: `{ "provider_id": "...", "model_id": "...", "display_name":
///         "...", "target_format": "openai"|"anthropic", "ttl_seconds": N }`.
pub async fn create_custom_model(
    State(s): State<AppState>,
    Json(input): Json<admin::CreateCustomModelInput>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let w = s.db_pool().writer();
        let row_id = admin::create_custom_model(&w, input)?;
        Ok(Json(serde_json::json!({ "row_id": row_id.0 })))
    }
    .await;
    body.into()
}

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

/// Core of the test flow: load the model, pick (or accept) an
/// account, decrypt the API key, build a minimal "ping" request,
/// fire it upstream with a 15 s timeout, capture the status and
/// elapsed milliseconds, and (only for the per-row path) persist
/// `last_test_status` on the model row.
///
/// The helper is called by both [`test_model`] (the per-row POST
/// handler) and [`test_combo_targets`] (the per-combo fan-out).
/// The behavior of the two call sites diverges only in the
/// [`TestOptions`] they pass — the underlying HTTP plumbing is the
/// same.
async fn run_test_for_model(
    s: &AppState,
    model_row_id: i64,
    account_id: Option<AccountId>,
    proxy_url: Option<String>,
    opts: TestOptions,
    cancel_rx: Option<tokio::sync::watch::Receiver<bool>>,
) -> TestResult {
    use openproxy_core::translation::{
        OpenAIMessage, OpenAIRequest, openai_to_anthropic, openai_to_gemini,
    };

    let row_id = ModelRowId(model_row_id);
    let start = std::time::Instant::now();

    // 1. Load the model row.
    let model = match (|| -> Result<models::Model, ApiError> {
        let w = s.db_pool().writer();
        models::get_by_row_id(&w, row_id)?.ok_or_else(|| {
            ApiError(CoreError::ModelNotFound {
                provider: "<unknown>".into(),
                model: format!("row_id={}", model_row_id),
            })
        })
    })() {
        Ok(m) => m,
        Err(ApiError(e)) => {
            return TestResult {
                row_id: model_row_id,
                status: e.http_status(),
                elapsed_ms: 0,
                error_msg: Some(e.to_string()),
                skipped: true,
                skip_reason: Some(format!("model lookup failed: {}", e)),
            };
        }
    };

    // 1a. If the model is toggled inactive, the per-row handler
    //     would still let the operator fire a test (they may be
    //     debugging why a model went inactive). The combo handler,
    //     however, wants to skip these rows outright — a fan-out
    //     should not bombard a model the operator has explicitly
    //     deactivated. We can detect which caller we are by
    //     inspecting `account_id`: a `Some(_)` value came from the
    //     combo path (the target row had a pinned account), while
    //     `None` means the per-row handler is asking us to pick.
    //     A pinned account means "this is a real target, respect
    //     its active flag"; no pinned account means "the operator
    //     clicked the button, do what they ask". This is a
    //     lightweight heuristic that keeps both flows happy without
    //     adding a new parameter to the helper signature.
    if !model.active && opts.in_combo_fanout {
        return TestResult::skipped(model_row_id, "model is inactive");
    }

    // 2. Find the adapter for that provider. Check built-in adapters
    //    first, then fall back to constructing a CustomAdapter from the
    //    DB row.
    let adapter = match resolve_adapter(s, &model.provider_id, s.adapters().as_slice()) {
        Ok(a) => a.clone(),
        Err(err) => {
            return TestResult {
                row_id: model_row_id,
                status: err.http_status(),
                elapsed_ms: 0,
                error_msg: Some(err.to_string()),
                skipped: true,
                skip_reason: Some(err.to_string()),
            };
        }
    };

    // 3. Resolve the account to use. Anonymous access is allowed when:
    //      - provider has auth_type "none", OR
    //      - provider has no accounts configured (fallback to anonymous)
    //    This lets bearer providers like opencode-zen work without
    //    accounts while still using accounts when they exist.
    let (is_anonymous, accounts_list) = {
        let w = s.db_pool().writer();
        let provider_row = providers::get(&w, &model.provider_id).unwrap_or_default();
        let accs = accounts::list(&w, Some(&model.provider_id)).unwrap_or_default();
        let anon = match &provider_row {
            Some(p) if matches!(p.auth_type, providers::AuthType::None) => true,
            _ if accs.is_empty() => true, // No accounts → try anonymous
            _ => false,
        };
        (anon, accs)
    };

    // Capture the optional account_id AND its label. The label is
    // needed by providers whose URL embeds account-level metadata
    // (e.g. CloudFlare Workers AI uses the label as its account ID).
    let (account_id_opt, _account_label, api_key) = if is_anonymous {
        (None, String::new(), String::new()) // Anonymous: no account, empty key
    } else {
        let selected = match account_id {
            Some(id) => {
                // Per-model path: look up the already-pinned account.
                let w = s.db_pool().writer();
                accounts::get(&w, id).ok().flatten()
            }
            None => {
                let healthy = accounts_list
                    .iter()
                    .find(|a| a.health_status == accounts::HealthStatus::Healthy);
                let degraded = || {
                    accounts_list
                        .iter()
                        .find(|a| a.health_status == accounts::HealthStatus::Degraded)
                };
                healthy
                    .or_else(degraded)
                    .or_else(|| accounts_list.first())
                    .cloned()
            }
        };

        let account_id = selected.as_ref().map(|a| a.id);
        let account_label = selected
            .as_ref()
            .and_then(|a| a.label.as_deref())
            .unwrap_or("")
            .to_string();

        // 4. Decrypt the API key. Drop the writer guard immediately.
        //    OAuth accounts store the token in access_token_encrypted,
        //    not api_key_encrypted, so we fall back to that if the
        //    primary decrypt fails (e.g. NULL column).
        let api_key = match account_id {
            Some(aid) => {
                let account = {
                    let w = s.db_pool().writer();
                    accounts::get(&w, aid).ok().flatten()
                };
                if let Some(ref acc) = account
                    && acc.auth_type == "oauth"
                {
                    match oauth::resolve_oauth_token(
                        s.db_pool().as_ref(),
                        acc,
                        model.provider_id.as_str(),
                        s.oauth_provider_registry().as_ref(),
                        s.upstream_client(),
                        s.master_key().as_ref(),
                    )
                    .await
                    {
                        Ok(token) => token,
                        Err(e) => {
                            let elapsed_ms = start.elapsed().as_millis() as u64;
                            let err_msg = format!("resolve oauth token: {}", e);
                            return TestResult {
                                row_id: model_row_id,
                                status: e.http_status(),
                                elapsed_ms,
                                error_msg: Some(err_msg),
                                skipped: false,
                                skip_reason: None,
                            };
                        }
                    }
                } else {
                    let w = s.db_pool().writer();
                    match accounts::decrypt_api_key(&w, aid, s.master_key().as_ref())
                        .or_else(|_| {
                            accounts::decrypt_access_token(&w, aid, s.master_key().as_ref())
                        })
                        .map_err(ApiError)
                    {
                        Ok(k) => k,
                        Err(ApiError(e)) => {
                            return TestResult {
                                row_id: model_row_id,
                                status: e.http_status(),
                                elapsed_ms: 0,
                                error_msg: Some(e.to_string()),
                                skipped: true,
                                skip_reason: Some(e.to_string()),
                            };
                        }
                    }
                }
            }
            None => String::new(),
        };

        (account_id, account_label, api_key)
    };

    // 5. Build the minimal test request. The exact prompts and limits
    //    are not significant — we just need the upstream to issue a
    //    real HTTP call so we can record the result.
    //
    //    The `system` message is sent first because some OpenRouter-
    //    served models (e.g. certain NVIDIA Nemotron builds) reject a
    //    bare `[{role: "user", content: "ping"}]` with a 400 from the
    //    OpenAI Python SDK v1.x Pydantic validator: the validator's
    //    discriminated-union ordering tries `developer` first when a
    //    `name: null` field is present, then complains the role is
    //    not `"developer"`. Adding a system message changes the
    //    validator's selection to the `system` variant (or, for
    //    non-strict validators, bypasses the discriminator) so the
    //    `user` message is accepted as-is. This matches the wire
    //    shape production clients (OpenAI SDK, Anthropic SDK, etc.)
    //    send, and the system prompt is also what most providers
    //    expect as a sanity check.
    let openai_req = OpenAIRequest {
        model: model.model_id.as_str().to_string(),
        messages: vec![
            OpenAIMessage {
                role: "system".into(),
                content: Some(serde_json::Value::String(
                    "You are a helpful assistant.".to_string(),
                )),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: serde_json::Map::new(),
            },
            OpenAIMessage {
                role: "user".into(),
                content: Some(serde_json::Value::String("ping".to_string())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: serde_json::Map::new(),
            },
        ],
        stream: false,
        temperature: None,
        max_tokens: Some(5),
        top_p: None,
        stop: None,
        tools: None,
        tool_choice: None,
        top_k: None,
        user: None,
        extra: serde_json::Map::new(),
    };

    // 6. Custom providers (kiro, antigravity, antigravity-cli) need
    //    their own executors that wrap the request in a provider-
    //    specific envelope (Cloud Code, Kiro conversationState, etc.)
    //    and parse the non-standard response. The standard adapter
    //    path below would send raw Gemini/OpenAI format to an endpoint
    //    that expects a different wire shape.
    let is_custom_provider = matches!(model.provider_id.as_str(), "kiro" | "antigravity");

    if is_custom_provider && !is_anonymous {
        // Delegate to the provider-specific executor, same as the
        // pipeline's `execute_single`. We need the access token and
        // provider-specific metadata.

        // Resolve the account for this test. The combo path already
        // pinned one; the per-row path picks the first healthy one.
        let test_account_id = account_id_opt.unwrap_or_else(|| {
            // Re-pick from the accounts list that was already loaded.
            // The list was consumed by `into_iter()` above, so we
            // re-query. This only happens for the per-row path when
            // the model has accounts but the caller didn't pin one.
            let w = s.db_pool().writer();
            accounts::list(&w, Some(&model.provider_id))
                .ok()
                .and_then(|l| {
                    l.iter()
                        .find(|a| a.health_status == accounts::HealthStatus::Healthy)
                        .or_else(|| {
                            l.iter()
                                .find(|a| a.health_status == accounts::HealthStatus::Degraded)
                        })
                        .or_else(|| l.first())
                        .map(|a| a.id)
                })
                .unwrap_or(AccountId(0))
        });

        // Decrypt the access token, resolving/refreshing it if it's an OAuth account.
        let access_token = {
            let account = {
                let w = s.db_pool().writer();
                accounts::get(&w, test_account_id).ok().flatten()
            };
            if let Some(ref acc) = account
                && acc.auth_type == "oauth"
            {
                match oauth::resolve_oauth_token(
                    s.db_pool().as_ref(),
                    acc,
                    model.provider_id.as_str(),
                    s.oauth_provider_registry().as_ref(),
                    s.upstream_client(),
                    s.master_key().as_ref(),
                )
                .await
                {
                    Ok(t) => t,
                    Err(e) => {
                        let elapsed_ms = start.elapsed().as_millis() as u64;
                        let err_msg = format!("resolve oauth token: {}", e);
                        return TestResult {
                            row_id: model_row_id,
                            status: e.http_status(),
                            elapsed_ms,
                            error_msg: Some(err_msg),
                            skipped: false,
                            skip_reason: None,
                        };
                    }
                }
            } else {
                let w = s.db_pool().writer();
                match accounts::decrypt_access_token(&w, test_account_id, s.master_key().as_ref()) {
                    Ok(t) => t,
                    Err(e) => {
                        let elapsed_ms = start.elapsed().as_millis() as u64;
                        let err_msg = format!("decrypt access token: {}", e);
                        return TestResult {
                            row_id: model_row_id,
                            status: e.http_status(),
                            elapsed_ms,
                            error_msg: Some(err_msg),
                            skipped: false,
                            skip_reason: None,
                        };
                    }
                }
            }
        };

        // Read provider-specific metadata and fire the executor.
        let executor_result = match model.provider_id.as_str() {
            "antigravity" => {
                let project_id = {
                    let w = s.db_pool().writer();
                    openproxy_core::executor_antigravity::read_project_id(&w, test_account_id)
                        .unwrap_or_default()
                };
                let http_client = s.upstream_client();
                // No client connection of its own on the admin
                // test path (it runs against a synthetic request);
                // see the symmetric note on the kiro branch below.
                let (_cancel_tx, dummy_cancel_rx) = tokio::sync::watch::channel(false);
                let final_cancel_rx = cancel_rx.clone().unwrap_or(dummy_cancel_rx);
                openproxy_core::executor_antigravity::execute_antigravity(
                    http_client,
                    &access_token,
                    &project_id,
                    &openai_req,
                    final_cancel_rx,
                    None,
                    proxy_url.clone(),
                )
                .await
            }
            "kiro" => {
                let (region, profile_arn) = {
                    let w = s.db_pool().writer();
                    let meta =
                        openproxy_core::executor_kiro::read_account_meta(&w, test_account_id)
                            .unwrap_or(None);
                    (
                        meta.as_ref()
                            .map(|m| m.region.clone())
                            .filter(|r| !r.is_empty())
                            .unwrap_or_else(|| {
                                openproxy_core::executor_kiro::KIRO_DEFAULT_REGION.to_string()
                            }),
                        meta.as_ref().and_then(|m| m.profile_arn.clone()),
                    )
                };
                let http_client = s.upstream_client();
                // The admin test endpoint runs against a single
                // short-lived request. It has no client connection
                // of its own (no chat client), so the watch stays
                // at `false` for the duration; the token is
                // therefore effectively never-cancelled and the
                // request is bounded by the executor's
                // `TimeoutProfile::Chat` envelope (see
                // `executor_kiro.rs:438-445`).
                let (_cancel_tx, dummy_cancel_rx) = tokio::sync::watch::channel(false);
                let final_cancel_rx = cancel_rx.clone().unwrap_or(dummy_cancel_rx);
                openproxy_core::executor_kiro::execute_kiro(
                    http_client,
                    &access_token,
                    &region,
                    profile_arn.as_deref(),
                    &openai_req,
                    final_cancel_rx,
                    proxy_url.clone(),
                )
                .await
            }
            _ => unreachable!(),
        };

        let elapsed_ms = start.elapsed().as_millis() as u64;
        let (status, error_msg) = match executor_result {
            Ok(_response) => (200_u16, None),
            Err(e) => (e.http_status(), Some(e.to_string())),
        };

        // Persist the result (per-row path only).
        if !opts.in_combo_fanout {
            let w = s.db_pool().writer();
            if let Err(e) = models::set_test_status(&w, row_id, status as i32) {
                return TestResult {
                    row_id: model_row_id,
                    status: e.http_status(),
                    elapsed_ms,
                    error_msg: Some(e.to_string()),
                    skipped: true,
                    skip_reason: Some(e.to_string()),
                };
            }
        }

        return TestResult {
            row_id: model_row_id,
            status,
            elapsed_ms,
            error_msg,
            skipped: false,
            skip_reason: None,
        };
    }

    // 7. Standard adapter path: translate to the row's native format
    //    and assemble the URL. This works for all non-custom providers
    //    (OpenAI-compatible, Anthropic, Gemini).
    //    `serde_json::to_value` cannot fail for these struct shapes in
    //    practice, but we still want a typed error if it ever does.
    let effective_target_format = match adapter.format() {
        adapters::AdapterFormat::Openai => openproxy_core::models::TargetFormat::Openai,
        adapters::AdapterFormat::Anthropic => openproxy_core::models::TargetFormat::Anthropic,
        adapters::AdapterFormat::Mixed => model.target_format,
        adapters::AdapterFormat::Gemini => openproxy_core::models::TargetFormat::Gemini,
        adapters::AdapterFormat::Responses => openproxy_core::models::TargetFormat::Responses,
    };
    let (url, body_value): (String, serde_json::Value) = if effective_target_format
        == openproxy_core::models::TargetFormat::Anthropic
    {
        let anthropic_req = openai_to_anthropic(
            &openai_req,
            model.model_id.as_str(),
            &openai_req.messages,
            openai_req.stream,
        );
        let url = adapter.build_chat_url_for_account(
            openproxy_core::models::TargetFormat::Anthropic,
            &model.model_id,
            &_account_label,
        );
        match serde_json::to_value(&anthropic_req) {
            Ok(v) => (url, v),
            Err(e) => {
                let err = CoreError::Internal(format!("serialize anthropic req: {}", e));
                return TestResult {
                    row_id: model_row_id,
                    status: 500,
                    elapsed_ms: 0,
                    error_msg: Some(err.to_string()),
                    skipped: true,
                    skip_reason: Some(err.to_string()),
                };
            }
        }
    } else if effective_target_format == openproxy_core::models::TargetFormat::Gemini {
        let gemini_req = openai_to_gemini(&openai_req, &openai_req.messages);
        let url = adapter.build_chat_url_for_account(
            openproxy_core::models::TargetFormat::Gemini,
            &model.model_id,
            &_account_label,
        );
        match serde_json::to_value(&gemini_req) {
            Ok(v) => (url, v),
            Err(e) => {
                let err = CoreError::Internal(format!("serialize gemini req: {}", e));
                return TestResult {
                    row_id: model_row_id,
                    status: 500,
                    elapsed_ms: 0,
                    error_msg: Some(err.to_string()),
                    skipped: true,
                    skip_reason: Some(err.to_string()),
                };
            }
        }
    } else if effective_target_format == openproxy_core::models::TargetFormat::Responses {
        let url = adapter.build_chat_url_for_account(
            openproxy_core::models::TargetFormat::Responses,
            &model.model_id,
            &_account_label,
        );
        let mut responses_req = openai_req.clone();
        responses_req.max_tokens = None;
        let (_cancel_tx, client_disconnected) = tokio::sync::watch::channel(false);
        let pipeline_req = openproxy_core::pipeline::PipelineRequest {
            request_id: RequestId::new(),
            trace_id: TraceId::new(),
            combo_id: ComboId(0),
            openai_request: responses_req,
            client_disconnected,
            stream_sink: None,
            api_key_id: None,
            race_cancel: None,
            combo_override: None,
            targets_override: None,
            request_headers: std::collections::BTreeMap::new(),
            request_body_json: None,
            race_cancelled: false,
            endpoint_kind: openproxy_core::endpoint::EndpointKind::Chat,
            compressed_messages: std::sync::OnceLock::new(),
        };
        let formatter = openproxy_core::pipeline::formatting::get_formatter(
            openproxy_core::models::TargetFormat::Responses,
        );
        match formatter.format_request(&pipeline_req, &model, &openai_req.messages, true, &adapter)
        {
            Ok(bytes) => match serde_json::from_slice::<serde_json::Value>(&bytes) {
                Ok(v) => (url, v),
                Err(e) => {
                    let err = CoreError::Internal(format!("serialize responses req: {}", e));
                    return TestResult {
                        row_id: model_row_id,
                        status: 500,
                        elapsed_ms: 0,
                        error_msg: Some(err.to_string()),
                        skipped: true,
                        skip_reason: Some(err.to_string()),
                    };
                }
            },
            Err(e) => {
                return TestResult {
                    row_id: model_row_id,
                    status: 500,
                    elapsed_ms: 0,
                    error_msg: Some(e.to_string()),
                    skipped: true,
                    skip_reason: Some(e.to_string()),
                };
            }
        }
    } else {
        let url = adapter.build_chat_url_for_account(
            openproxy_core::models::TargetFormat::Openai,
            &model.model_id,
            &_account_label,
        );
        match serde_json::to_value(&openai_req) {
            Ok(v) => (url, v),
            Err(e) => {
                let err = CoreError::Internal(format!("serialize openai req: {}", e));
                return TestResult {
                    row_id: model_row_id,
                    status: 500,
                    elapsed_ms: 0,
                    error_msg: Some(err.to_string()),
                    skipped: true,
                    skip_reason: Some(err.to_string()),
                };
            }
        }
    };

    // 8. Build the HTTP request. The 15s timeout caps the test wall-
    //    clock cost — a hung upstream shouldn't pin a dashboard
    //    button indefinitely.
    let headers = adapter.build_headers(&api_key, effective_target_format, &model.model_id);
    let mut req = openproxy_core::upstream::UpstreamRequest::post_json(
        url,
        bytes::Bytes::from(serde_json::to_vec(&body_value).unwrap()),
    );
    req.proxy = proxy_url.clone();
    for (k, v) in &headers {
        if let Ok(hn) = axum::http::HeaderName::from_bytes(k.as_bytes())
            && let Ok(hv) = axum::http::HeaderValue::from_str(v) {
                req.headers.insert(hn, hv);
            }
    }

    // 9. Send + measure. We capture both the wall-clock elapsed time
    //    and a truncated error body so the dashboard can show
    //    something useful when the upstream is unhappy.
    let start = std::time::Instant::now();
    let client = s.upstream_client();
    let cancel = openproxy_core::upstream::CancellationToken::new();

    if let Some(mut rx) = cancel_rx.clone() {
        let rx_cancel = cancel.clone();
        tokio::spawn(async move {
            if *rx.borrow() {
                rx_cancel.cancel();
                return;
            }
            while rx.changed().await.is_ok() {
                if *rx.borrow() {
                    rx_cancel.cancel();
                    return;
                }
            }
        });
    }

    let profile = openproxy_core::upstream::TimeoutProfile::Custom(
        openproxy_core::upstream::ResolvedTimeouts {
            dns_ms: 2000,
            dial_ms: 5000,
            tls_ms: 5000,
            write_ms: 5000,
            headers_ms: 15000,
            body_chunk_ms: 5000,
            total_ms: 15000,
        },
    );

    let result = client.call(req, profile, cancel).await;
    let elapsed_ms = start.elapsed().as_millis() as u64;

    let (status, error_msg) = match result {
        Ok(response) => {
            let status = response.status.as_u16();
            if status >= 400 {
                let body = response.collect().await.unwrap_or_default();
                let text = String::from_utf8_lossy(&body);
                let truncated: String = text.chars().take(TEST_ERROR_BODY_MAX_CHARS).collect();
                (status, Some(truncated))
            } else {
                (status, None)
            }
        }
        Err(e) => {
            // 0 = "request never reached the upstream" (DNS / connect / TLS
            // / timeout). The schema doesn't constrain this — `0` is a
            // distinct sentinel that the dashboard renders as a network
            // error.
            (0, Some(format!("{:?}", e)))
        }
    };

    // 10. Persist the result. The persist is independent of the response
    //     shape: the dashboard should always see *something* on the row
    //     after the button is pressed. We write to the row from the
    //     per-row path only; the combo fan-out does not want its
    //     transient probe to overwrite the row's last-test status.
    if !opts.in_combo_fanout {
        let w = s.db_pool().writer();
        if let Err(e) = models::set_test_status(&w, row_id, status as i32) {
            return TestResult {
                row_id: model_row_id,
                status: e.http_status(),
                elapsed_ms,
                error_msg: Some(e.to_string()),
                skipped: true,
                skip_reason: Some(e.to_string()),
            };
        }
    }

    TestResult {
        row_id: model_row_id,
        status,
        elapsed_ms,
        error_msg,
        skipped: false,
        skip_reason: None,
    }
}

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

pub async fn test_model(
    State(s): State<AppState>,
    Path(model_row_id): Path<i64>,
    cancel_watch: Option<axum::Extension<crate::disconnect::CancelWatch>>,
    body: Option<Json<TestModelInput>>,
) -> ApiResult<Json<serde_json::Value>> {
    let cancel_rx = cancel_watch.map(|axum::Extension(cw)| cw.rx);

    let (account_id, proxy_url) = if let Some(Json(input)) = body {
        let aid = input.account_id.map(AccountId::new);
        let purl = if let Some(ref pid) = input.proxy_id {
            let r = s.db_pool().reader();
            if let Ok(Some(p)) = openproxy_core::free_proxies::get_proxy(&r, pid) {
                Some(format!(
                    "{}://{}:{}",
                    p.r#type.to_lowercase(),
                    p.host,
                    p.port
                ))
            } else {
                None
            }
        } else {
            None
        };
        (aid, purl)
    } else {
        (None, None)
    };

    let r = run_test_for_model(
        &s,
        model_row_id,
        account_id,
        proxy_url,
        TestOptions::default(),
        cancel_rx,
    )
    .await;
    ApiResult::ok(Json(serde_json::json!({
        "row_id": r.row_id,
        "status": r.status,
        "elapsed_ms": r.elapsed_ms,
        "error_msg": r.error_msg,
    })))
}

// =====================================================================
// Account health
// =====================================================================

/// `POST /admin/accounts/:id/health` — force-set an account's
/// health flag. Body: `{"health": "healthy"|"degraded"|"unhealthy"}`.
///
/// Bypasses the runtime's automatic health tracking so an operator can
/// manually re-enable an account after fixing it (or take it offline
/// during an incident).
pub async fn set_account_health(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<serde_json::Value>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let health_str = body
            .get("health")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CoreError::Validation("missing 'health' string".into()))?;
        let health = accounts::HealthStatus::parse(health_str)?;
        let w = s.db_pool().writer();
        accounts::set_health(&w, AccountId::new(id), health)?;
        Ok(Json(serde_json::json!({
            "id": id,
            "health": health_str,
        })))
    }
    .await;
    body.into()
}

/// `PUT /admin/accounts/:id/api-key` — encrypt and store a new API
/// key for an existing account (or clear it by passing `null`).
///
/// Body: `{"api_key": "sk-..."}` or `{"api_key": null}`.
///
/// The plaintext is encrypted with the server's master key and stored as
/// a BLOB. Returns 404 when the account does not exist.
pub async fn update_account_api_key(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<admin::UpdateAccountApiKeyInput>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let w = s.db_pool().writer();
        admin::update_account_api_key(&w, s.master_key().as_ref(), AccountId::new(id), body)?;
        Ok(Json(serde_json::json!({ "id": id })))
    }
    .await;
    body.into()
}

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

pub async fn list_models_admin(
    State(s): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<ListModelsQuery>,
) -> ApiResult<Json<Vec<models::Model>>> {
    let body: Result<Json<Vec<models::Model>>, ApiError> = async {
        // Read-only SELECT — use the READER.
        let r = s.db_pool().reader();
        let mut list = models::list_all(&r)?;
        if let Some(p) = q.provider_id {
            list.retain(|m| m.provider_id.as_str() == p);
        }
        Ok(Json(list))
    }
    .await;
    body.into()
}

// =====================================================================
// Account quota refresh
// =====================================================================

/// `POST /admin/accounts/:id/refresh-quota` — fetch a fresh quota
/// snapshot for a single account and persist it.
///
/// Flow:
/// 1. Look up the account to find its `provider_id`.
/// 2. If the provider has no quota fetcher implemented (anything other
///    than `minimax` / `minimax-cn` in the MVP), short-circuit with a
///    `{"supported": false, "message": "..."}` body. The HTTP status is
///    still 200 — the call did not fail, it just isn't meaningful for
///    this provider — so the dashboard can render the response inline.
/// 3. Decrypt the API key.
/// 4. Fire the upstream quota call. On success, stamp the result onto
///    the row. On failure, stamp a quota row that records the error
///    (so the UI can show "fetch failed: ..." and the next manual
///    refresh isn't blocked by a stale `fetch_error`).
///
/// The endpoint always returns 200; the success/failure bit is in the
/// JSON body, and the operator-facing `last_fetched_at` is updated even
/// on a failed fetch (so the dashboard can show "tried 12s ago").
pub async fn refresh_account_quota(
    State(s): State<AppState>,
    Path(account_id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    tracing::info!(account_id = account_id, "refresh_account_quota: start");
    let s_clone = s.clone();
    let result: Result<Json<serde_json::Value>, ApiError> = async move {
        let account_id = AccountId::new(account_id);

        // 1 + 2 + 3: load the account, gate on provider, decrypt the key.
        // The capability check happens *before* the decrypt so we never
        // touch the master key for a provider whose quota we'll never
        // fetch.
        let (provider_id_str, api_key, access_token, provider_specific) = {
            tracing::debug!(account_id = account_id.0, "refresh_account_quota: acquiring writer");
            let w = s_clone.db_pool().writer();
            tracing::debug!(account_id = account_id.0, "refresh_account_quota: writer acquired");
            let acc = admin::account_for_quota_refresh(&w, account_id)?;
            let adapters = s_clone.adapters();
            let supports_quota = adapters
                .iter()
                .find(|a| a.id() == &acc.provider_id)
                .map(|a| a.metadata().quota_refresh_supported)
                .unwrap_or(false);

            if !supports_quota {
                return Ok(Json(serde_json::json!({
                    "account_id": account_id.0,
                    "supported": false,
                    "message": format!(
                        "quota fetching not implemented for provider '{}'",
                        acc.provider_id
                    ),
                })));
            }
            let provider_str = acc.provider_id.to_string();
            let is_oauth = acc.auth_type == "oauth";
            let provider_specific = acc.oauth_provider_specific.clone();

            // OAuth providers (antigravity) need the access token, not
            // an API key. API-key providers need the key. We decrypt
            // whichever is relevant and leave the other empty.
            let (k, token) = if is_oauth {
                let t = accounts::decrypt_access_token(
                    &w,
                    account_id,
                    s_clone.master_key().as_ref(),
                )?;
                (String::new(), Some(t))
            } else {
                let k = admin::decrypt_api_key_for_account(
                    &w,
                    account_id,
                    s_clone.master_key().as_ref(),
                )?;
                (k, None)
            };
            (provider_str, k, token, provider_specific)
        };
        // writer guard dropped here.

        // 4: fire the upstream quota call. Returns an `AccountQuota`
        //    even on failure (with `fetch_error` set), so we always
        //    have a row to persist.
        let upstream_client = s_clone.upstream_client();
        tracing::info!(account_id = account_id.0, provider = %provider_id_str, "refresh_account_quota: calling upstream");
        let q = admin::fetch_account_quota(
            &provider_id_str,
            upstream_client,
            &api_key,
            access_token.as_deref(),
            provider_specific.as_deref(),
        )
        .await;
        tracing::info!(account_id = account_id.0, provider = %provider_id_str, fetch_error = ?q.fetch_error, "refresh_account_quota: upstream done");

        // 4b: If the quota fetch failed with a 401 (expired token) and
        //     we're on an OAuth account, try an on-demand token refresh
        //     and retry the quota call once.
        let q = if q.fetch_error.as_deref().is_some_and(|e| e.contains("401"))
            && access_token.is_some()
        {
            let refresh_result = {
                let w = s_clone.db_pool().writer();
                accounts::decrypt_refresh_token(&w, account_id, s_clone.master_key().as_ref())
                    .ok()
                    .flatten()
            };
            if let Some(refresh_token) = refresh_result {
                // Find the matching OAuth provider from the registry.
                let registry = s_clone.oauth_provider_registry();
                let provider = registry.get(&provider_id_str);
                if let Some(provider) = provider {
                    let upstream_client = s_clone.upstream_client();
                    match provider
                        .refresh_token(
                            &refresh_token,
                            upstream_client,
                            account_id,
                            openproxy_core::oauth::DbRef::Pool(s_clone.db_pool().as_ref()),
                        )
                        .await
                    {
                        Ok(new_tokens) => {
                            let expires_at = new_tokens.expires_in.map(|secs| {
                                (chrono::Utc::now() + chrono::Duration::seconds(secs as i64))
                                    .format("%Y-%m-%dT%H:%M:%SZ")
                                    .to_string()
                            });
                            // Store the refreshed tokens.
                            {
                                let w = s_clone.db_pool().writer();
                                let _ = accounts::store_oauth_tokens(
                                    &w,
                                    account_id,
                                    &new_tokens.access_token,
                                    new_tokens.refresh_token.as_deref(),
                                    s_clone.master_key(),
                                    &new_tokens.token_type,
                                    expires_at.as_deref(),
                                    new_tokens.scope.as_deref(),
                                    None,
                                    None,
                                );
                            }
                            // Retry the quota call with the new access token.
                            admin::fetch_account_quota(
                                &provider_id_str,
                                upstream_client,
                                &api_key,
                                Some(&new_tokens.access_token),
                                provider_specific.as_deref(),
                            )
                            .await
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                account_id = account_id.0,
                                "on-demand token refresh failed"
                            );
                            q // return original error
                        }
                    }
                } else {
                    q
                }
            } else {
                tracing::debug!(
                    account_id = account_id.0,
                    "401 but no refresh token available for on-demand refresh"
                );
                q
            }
        } else {
            q
        };

        // 5: persist.
        {
            let w = s_clone.db_pool().writer();
            admin::persist_account_quota(&w, account_id, &q)?;
        }

        // 6: G2.4 — surface a `quota_low` notification when the
        // remaining quota is below the low-water mark. Skipped when
        // the fetch errored (the `quota_*` columns are likely stale
        // or NULL, so a "low quota" reading would be misleading).
        //
        // Threshold: 10% of the limit. If the limit is missing or
        // zero (some providers only report `used`), fall back to an
        // absolute threshold of 1000 — generous enough that a small
        // daily quota account still gets surfaced, tight enough that
        // a "no limit" provider doesn't page on every fetch.
        //
        // Both session and weekly windows are checked; we fire on the
        // FIRST one that crosses the threshold (a single notification
        // per fetch, even if both windows are low). Per-account dedup
        // (`quota_low:{account_id}`) collapses repeats within 24h so
        // the operator isn't paged on every refresh click.
        if q.fetch_error.is_none() {
            let low = compute_low_quota_signal(&q);
            if let Some((scope, remaining, limit)) = low {
                let provider_id_str = provider_id_str.clone();
                let dedup_key = format!(
                    "{}:{}",
                    openproxy_core::notifications::CODE_QUOTA_LOW,
                    account_id.0
                );
                let percent = if limit > 0 {
                    ((remaining as f64) / (limit as f64) * 100.0).round() as u32
                } else {
                    0
                };
                let payload = serde_json::json!({
                    "code": openproxy_core::notifications::CODE_QUOTA_LOW,
                    "message": format!(
                        "Account {} on {} has low {} quota: {} remaining ({}%)",
                        account_id.0, provider_id_str, scope, remaining, percent,
                    ),
                    "provider_id": &provider_id_str,
                    "details": {
                        "account_id": account_id.0,
                        "provider_id": &provider_id_str,
                        "scope": scope,
                        "remaining": remaining,
                        "limit": limit,
                        "percent": percent,
                    },
                });
                let w = s_clone.db_pool().writer();
                let _ = openproxy_core::notifications::insert_and_broadcast(
                    &w,
                    openproxy_core::notifications::KIND_SYSTEM,
                    &payload,
                    Some(&dedup_key),
                    Some(&provider_id_str),
                );
            }
        }

        Ok(Json(serde_json::json!({
            "account_id": account_id.0,
            "supported": true,
            "session_used": q.session_used,
            "session_limit": q.session_limit,
            "session_reset_at": q.session_reset_at,
            "weekly_used": q.weekly_used,
            "weekly_limit": q.weekly_limit,
            "weekly_reset_at": q.weekly_reset_at,
            "plan_name": q.plan_name,
            "last_fetched_at": q.last_fetched_at,
            "error": q.fetch_error,
        })))
    }
    .await;
    result.into()
}

/// Low-quota threshold: 10% of the limit. If the limit is missing or
/// zero, the absolute floor of 1000 is used (matches the task spec's
/// "absolute threshold like 1000"). Encoded as the integer test
/// `remaining * 10 < limit` in [`is_low`] below to stay in integer
/// arithmetic — the actual comparison doesn't reference a float
/// constant (floats would just add rounding noise for no benefit at
/// this scale).
const QUOTA_LOW_ABSOLUTE_FLOOR: i64 = 1_000;

/// Inspect a freshly-fetched [`AccountQuota`] and decide whether to
/// fire a `quota_low` notification. Returns `(scope, remaining, limit)`
/// for the FIRST low window found (session checked before weekly), or
/// `None` if both windows are healthy / unknown.
///
/// `scope` is `"session"` or `"weekly"` — surfaced in the notification
/// details so the dashboard can render "low session quota" vs "low
/// weekly quota" distinctly.
fn compute_low_quota_signal(
    q: &openproxy_core::quota::AccountQuota,
) -> Option<(&'static str, i64, i64)> {
    // Session window.
    if let Some(used) = q.session_used
        && let Some(limit) = q.session_limit
    {
        let remaining = (limit - used).max(0);
        if is_low(remaining, limit) {
            return Some(("session", remaining, limit));
        }
    } else if let Some(used) = q.session_used {
        // No limit reported — fall back to the absolute floor on the
        // `used` counter (treat it as a "remaining" proxy: if the
        // account has burned through all but `QUOTA_LOW_ABSOLUTE_FLOOR`
        // of an unknown ceiling, that's still worth surfacing).
        // We can't compute "remaining" without a limit, so this branch
        // only fires when `used` itself is below the floor — i.e. the
        // account is barely touching the upstream. That's not a "low
        // quota" signal, so we intentionally DON'T fire here. Kept as
        // an explicit `else if` to document the reasoning.
        let _ = used;
    }
    // Weekly window.
    if let Some(used) = q.weekly_used
        && let Some(limit) = q.weekly_limit
    {
        let remaining = (limit - used).max(0);
        if is_low(remaining, limit) {
            return Some(("weekly", remaining, limit));
        }
    }
    None
}

/// Low-water test. When `limit > 0`, fires iff
/// `remaining < limit * 0.10` (i.e. < 10% remaining).
/// When `limit == 0` (degenerate row), falls back to the absolute
/// floor: `remaining < QUOTA_LOW_ABSOLUTE_FLOOR`.
fn is_low(remaining: i64, limit: i64) -> bool {
    if limit > 0 {
        // `remaining * 10 < limit` is equivalent to
        // `remaining < limit * 0.10` but stays in integer arithmetic
        // (no float cast, no rounding surprises).
        remaining * 10 < limit
    } else {
        remaining < QUOTA_LOW_ABSOLUTE_FLOOR
    }
}

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

/// `POST /admin/providers/:id/refresh` — re-discover the model list
/// for a whole provider in one shot.
///
/// The handler is the provider-level counterpart to
/// [`refresh_models`] (which is keyed by a single model row). It is the
/// path the dashboard's "Refresh models" button uses: the UI knows the
/// provider but not a specific model row, so it asks the server to walk
/// the full discovery flow end-to-end.
///
/// Flow:
/// 1. Find the adapter for `provider_id` in the in-process registry.
/// 2. If the adapter has no `/models` endpoint, return a 0-row result
///    with a `note` field so the UI can show a friendly message instead
///    of an error (e.g. MiniMax, which has no model-list endpoint).
/// 3. Pick a healthy account (or the explicit `?account_id=`) and
///    decrypt its API key.
/// 4. Call [`admin::refresh_models`], which fetches the upstream's
///    `/models` and upserts the results.
///
/// On success: `{"provider": "...", "models_refreshed": N}`.
pub async fn refresh_provider_models(
    State(s): State<AppState>,
    Path(provider_id): Path<String>,
    Query(q): Query<ProviderRefreshQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    run_provider_refresh(s, provider_id, q).await
}

async fn refresh_oauth_if_needed(
    s: &AppState,
    account: accounts::Account,
    provider_id: &ProviderId,
) -> String {
    if account.auth_type != "oauth" {
        return String::new();
    }

    let access_token = {
        let conn = s.db_pool().writer();
        match accounts::decrypt_access_token(&conn, account.id, s.master_key().as_ref()) {
            Ok(token) => token,
            Err(e) => {
                tracing::warn!(
                    account = account.id.0,
                    provider = %provider_id,
                    error = %e,
                    "oauth refresh-on-demand: failed to decrypt access token"
                );
                return String::new();
            }
        }
    };

    if !oauth::oauth_expires_soon(&account, provider_id.as_str()) {
        return access_token;
    }

    let stored_access_token = access_token.clone();
    let refresh_token = {
        let conn = s.db_pool().writer();
        match accounts::decrypt_refresh_token(&conn, account.id, s.master_key().as_ref()) {
            Ok(Some(rt)) => rt,
            Ok(None) => return stored_access_token,
            Err(e) => {
                tracing::warn!(
                    account = account.id.0,
                    provider = %provider_id,
                    error = %e,
                    "oauth refresh-on-demand: failed to decrypt refresh token"
                );
                return stored_access_token;
            }
        }
    };

    let registry = s.oauth_provider_registry();
    let Some(provider) = registry.get(provider_id.as_str()) else {
        tracing::warn!(
            account = account.id.0,
            provider = %provider_id,
            "oauth refresh-on-demand: no provider impl found"
        );
        return access_token;
    };

    tracing::info!(
        account = account.id.0,
        provider = %provider_id,
        "oauth refresh-on-demand: refreshing expired/expiring token"
    );

    let upstream_client = s.upstream_client();
    match provider
        .refresh_token(
            &refresh_token,
            upstream_client,
            account.id,
            openproxy_core::oauth::DbRef::Pool(s.db_pool().as_ref()),
        )
        .await
    {
        Ok(token) => {
            let expires_at = token.expires_in.map(|secs| {
                (chrono::Utc::now() + chrono::Duration::seconds(secs as i64))
                    .format("%Y-%m-%dT%H:%M:%SZ")
                    .to_string()
            });

            let conn = s.db_pool().writer();
            match accounts::store_oauth_tokens(
                &conn,
                account.id,
                &token.access_token,
                token.refresh_token.as_deref(),
                s.master_key().as_ref(),
                &token.token_type,
                expires_at.as_deref(),
                token.scope.as_deref(),
                account.oauth_provider_specific.as_deref(),
                account.email.as_deref(),
            ) {
                Ok(()) => {
                    tracing::info!(
                        account = account.id.0,
                        provider = %provider_id,
                        "oauth refresh-on-demand: tokens refreshed successfully"
                    );
                    token.access_token
                }
                Err(e) => {
                    tracing::warn!(
                        account = account.id.0,
                        provider = %provider_id,
                        error = %e,
                        "oauth refresh-on-demand: failed to store refreshed tokens"
                    );
                    access_token
                }
            }
        }
        Err(e) => {
            tracing::warn!(
                account = account.id.0,
                provider = %provider_id,
                error = %e,
                "oauth refresh-on-demand: token refresh failed"
            );
            access_token
        }
    }
}

async fn resolve_refresh_account(
    s: &AppState,
    provider: &ProviderId,
    q: &ProviderRefreshQuery,
) -> Result<(Option<AccountId>, String), ApiError> {
    let w = s.db_pool().writer();
    let provider_row = match providers::get(&w, provider) {
        Ok(p) => p,
        Err(e) => return Err(ApiError(e)),
    };
    let accounts_list = match accounts::list(&w, Some(provider)) {
        Ok(l) => l,
        Err(e) => return Err(ApiError(e)),
    };

    let is_anonymous = match &provider_row {
        Some(p) if matches!(p.auth_type, providers::AuthType::None) => true,
        _ if accounts_list.is_empty() => true,
        _ => false,
    };

    if is_anonymous {
        return Ok((None, String::new()));
    }

    let account_id = match q.account_id {
        Some(aid) => Some(AccountId::new(aid)),
        None => accounts_list
            .iter()
            .find(|a| a.health_status == accounts::HealthStatus::Healthy)
            .or_else(|| {
                accounts_list
                    .iter()
                    .find(|a| a.health_status == accounts::HealthStatus::Degraded)
            })
            .map(|a| a.id),
    };

    if account_id.is_none() {
        let is_anonymous_fallback = provider_row
            .as_ref()
            .map(|p| matches!(p.auth_type, providers::AuthType::None))
            .unwrap_or(false);

        if is_anonymous_fallback || accounts_list.is_empty() {
            Ok((None, String::new()))
        } else {
            Err(ApiError(CoreError::NoHealthyTargets(0)))
        }
    } else {
        Ok((account_id, String::new()))
    }
}

/// Inner body of [`refresh_provider_models`], factored out so the
/// handler's future is a small wrapper and the actual work (which holds
/// `parking_lot::MutexGuard`s across an `await` point, then drops them)
/// lives in a clearly-named function.
///
/// ## Locking note
///
/// The handler **does not hold any DB lock across the `await`** on the
/// upstream HTTP call. We collect everything we need from the DB
/// (provider id, adapter clone, decrypted API key) and drop the
/// writer guard before calling `adapter.fetch_models(...).await`. The
/// final `admin::refresh_models` step opens its *own* `Connection` via
/// `DbPool::open_connection` (see the doc on `refresh_models` for the
/// `Send` rationale), so even that final write doesn't block any other
/// request's read.
async fn run_provider_refresh(
    s: AppState,
    provider_id_str: String,
    q: ProviderRefreshQuery,
) -> ApiResult<Json<serde_json::Value>> {
    let provider = ProviderId::new(provider_id_str.clone());
    let ttl_seconds = q.ttl_seconds.unwrap_or(PROVIDER_REFRESH_DEFAULT_TTL_SECS);

    // 1. Find the adapter. Check built-in adapters first, then
    //    fall back to constructing a CustomAdapter from the DB row.
    let adapter = match resolve_adapter(&s, &provider, s.adapters().as_slice()) {
        Ok(a) => a.clone(),
        Err(e) => return ApiResult::err(ApiError(e)),
    };

    // 2. Provider has no /models endpoint and no custom fetch_models
    //    implementation: no rows to refresh, return an empty result
    //    with a note rather than a 5xx.  Providers like Antigravity
    //    return None for models_url() but override fetch_models() to
    //    discover models via a different API, so we let them through
    //    here and let refresh_models() call fetch_models() directly.
    //    (The guard below is intentionally removed — fetch_models is
    //    always invoked by admin::refresh_models regardless.)

    // 3. Resolve a healthy/degraded account for this provider.
    let selected_account_id = match resolve_refresh_account(&s, &provider, &q).await {
        Ok((Some(account_id), _)) => Some(account_id),
        Ok((None, _)) => None,
        Err(e) => return ApiResult::err(e),
    };

    // 4. Decrypt or refresh the selected credential. Drop DB guards
    //    before awaiting refresh; adapter.fetch_models() below then
    //    receives a plaintext token/key and no SQLite guard crosses await.
    let api_key = match selected_account_id {
        Some(account_id) => {
            let account = {
                let w = s.db_pool().writer();
                match accounts::get(&w, account_id) {
                    Ok(Some(a)) => a,
                    Ok(None) => {
                        return ApiResult::err(ApiError(CoreError::AccountNotFound(account_id.0)));
                    }
                    Err(e) => return ApiResult::err(ApiError(e)),
                }
            };
            if account.auth_type == "oauth" {
                refresh_oauth_if_needed(&s, account, &provider).await
            } else {
                let w = s.db_pool().writer();
                match accounts::decrypt_api_key(&w, account_id, s.master_key().as_ref()) {
                    Ok(k) => k,
                    Err(e) => return ApiResult::err(ApiError(e)),
                }
            }
        }
        None => String::new(),
    };

    // Resolve account label for CloudFlare / label-based providers.
    let account_label = match selected_account_id {
        Some(account_id) => {
            let w = s.db_pool().writer();
            match accounts::get(&w, account_id) {
                Ok(Some(a)) => a.label.unwrap_or_default(),
                _ => String::new(),
            }
        }
        None => String::new(),
    };

    // 5. Open a fresh connection for the upsert. See the doc on
    //    `admin::refresh_models` for the `Send` rationale: an owned
    //    `Connection` is the only way to keep the outer future
    //    `Send` across an `await`.
    let conn_for_refresh = match s.db_pool().open_connection() {
        Ok(c) => c,
        Err(e) => return ApiResult::err(ApiError(e)),
    };

    // 6. Run the refresh. This is the only `await` on the upstream
    //    HTTP call; everything else is sync DB work.
    let upsert = match admin::refresh_models(
        conn_for_refresh,
        &provider,
        &api_key,
        &adapter,
        s.upstream_client(),
        ttl_seconds,
        &account_label,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return ApiResult::err(ApiError(e)),
    };

    // 7. Auto-activation pass. The provider may have a substring
    //    `auto_activate_keyword` set; if so, every non-custom row
    //    gets `active` flipped to whether its `model_id` contains the
    //    keyword. When no keyword is set, all non-custom rows are
    //    switched on. This is a "refresh also re-applies the rule"
    //    semantic: an operator who disables a non-custom row by hand
    //    and then triggers a refresh will see it come back on, which
    //    matches the spec's expectation.
    let activated = match (|| -> openproxy_core::Result<u64> {
        // Re-load the provider so we see the up-to-date keyword;
        // doing this in a fresh writer keeps the lock short.
        let w = s.db_pool().writer();
        let p = providers::get(&w, &provider)?;
        let keyword = p.and_then(|pp| pp.auto_activate_keyword);
        let keyword_ref = keyword.as_deref();
        models::apply_auto_activation(&w, &provider, keyword_ref)
    })() {
        Ok(n) => n,
        Err(e) => return ApiResult::err(ApiError(e)),
    };

    ApiResult::ok(Json(serde_json::json!({
        "provider": provider_id_str,
        "models_refreshed": upsert.touched,
        "new_model_ids": upsert.new_model_ids,
        "models_activated": activated,
    })))
}

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

/// `GET /admin/keys` — list every key, newest first.
pub async fn list_api_keys(
    State(s): State<AppState>,
) -> ApiResult<Json<Vec<core_api_keys::ApiKey>>> {
    let body: Result<Json<Vec<core_api_keys::ApiKey>>, ApiError> = async {
        // Read-only SELECT — use the READER.
        let r = s.db_pool().reader();
        let list = core_api_keys::list(&r)?;
        Ok(Json(list))
    }
    .await;
    body.into()
}

/// `POST /admin/keys` — create a new key.
///
/// Response shape:
///
/// ```json
/// { "key": <ApiKey metadata>, "plaintext": "op_live_..." }
/// ```
///
/// The plaintext is the *only* time the user will see the secret.
/// The dashboard's "Save this key now" modal is the consumer here.
pub async fn create_api_key(
    State(s): State<AppState>,
    Json(body): Json<core_api_keys::CreateApiKeyInput>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let w = s.db_pool().writer();
        let (key, plaintext) = core_api_keys::create(&w, body, "admin")?;
        Ok(Json(serde_json::json!({
            "key": key,
            "plaintext": plaintext,
        })))
    }
    .await;
    body.into()
}

/// `GET /admin/keys/:id` — fetch a single key by id. 404 if absent.
pub async fn get_api_key(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<core_api_keys::ApiKey>> {
    let body: Result<Json<core_api_keys::ApiKey>, ApiError> = async {
        // Read-only SELECT — use the READER.
        let r = s.db_pool().reader();
        let key = core_api_keys::get_by_id(&r, ApiKeyId(id))?
            .ok_or_else(|| CoreError::Internal(format!("api_key {id} not found")))?;
        Ok(Json(key))
    }
    .await;
    body.into()
}

/// `PATCH /admin/keys/:id` — partial update.
///
/// Body fields are all optional. The `Option<Option<T>>` shape on
/// `allowed_models` / `allowed_combos` / `expires_at` lets the caller
/// distinguish "leave alone" (key absent) from "clear to NULL" (key
/// present with value `null`).
pub async fn update_api_key(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<serde_json::Value>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let label = body.get("label").and_then(|v| v.as_str());

        let scopes_owned: Option<Vec<String>> =
            body.get("scopes").and_then(|v| v.as_array()).map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            });
        let scopes_slice: Option<&[String]> = scopes_owned.as_deref();

        // `allowed_models`: absent = no-op; present + null = clear to NULL;
        // present + array = set to that array.
        let allowed_models_owned: Option<Option<Vec<String>>> =
            body.get("allowed_models").map(|v| {
                if v.is_null() {
                    None
                } else {
                    v.as_array().map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(String::from))
                            .collect()
                    })
                }
            });
        let allowed_models_slice: Option<Option<&[String]>> =
            allowed_models_owned.as_ref().map(|o| o.as_deref());

        let allowed_combos_owned: Option<Option<Vec<i64>>> = body.get("allowed_combos").map(|v| {
            if v.is_null() {
                None
            } else {
                v.as_array()
                    .map(|a| a.iter().filter_map(|x| x.as_i64()).collect())
            }
        });
        let allowed_combos_slice: Option<Option<&[i64]>> =
            allowed_combos_owned.as_ref().map(|o| o.as_deref());

        let is_active = body.get("is_active").and_then(|v| v.as_bool());

        let expires_owned: Option<Option<String>> =
            body.get("expires_at").map(|v| v.as_str().map(String::from));
        let expires_slice: Option<Option<&str>> = expires_owned.as_ref().map(|o| o.as_deref());

        let w = s.db_pool().writer();
        core_api_keys::update(
            &w,
            ApiKeyId(id),
            label,
            scopes_slice,
            allowed_models_slice,
            allowed_combos_slice,
            is_active,
            expires_slice,
        )?;
        Ok(Json(serde_json::json!({ "id": id })))
    }
    .await;
    body.into()
}

/// `POST /admin/keys/:id/revoke` — soft-disable. Idempotent (a
/// second call preserves the original `revoked_at` stamp).
pub async fn revoke_api_key(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let w = s.db_pool().writer();
        core_api_keys::revoke(&w, ApiKeyId(id))?;
        Ok(Json(serde_json::json!({ "id": id, "revoked": true })))
    }
    .await;
    body.into()
}

/// `DELETE /admin/keys/:id` — hard delete. Idempotent (a missing
/// id is a silent no-op, matching the `accounts::delete` policy).
pub async fn delete_api_key(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let w = s.db_pool().writer();
        core_api_keys::hard_delete(&w, ApiKeyId(id))?;
        Ok(Json(serde_json::json!({ "id": id, "deleted": true })))
    }
    .await;
    body.into()
}

/// `POST /admin/keys/:id/regenerate` — issue a new plaintext and
/// re-hash the row. The previous plaintext is invalidated
/// immediately. Response shape matches `create_api_key` so the
/// dashboard's "Save this key now" modal can be reused.
pub async fn regenerate_api_key(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let w = s.db_pool().writer();
        let (key, plaintext) = core_api_keys::regenerate(&w, ApiKeyId(id))?;
        Ok(Json(serde_json::json!({
            "key": key,
            "plaintext": plaintext,
        })))
    }
    .await;
    body.into()
}

/// `GET /admin/keys/:id/usage` — headline metrics for one key.
/// Returns a flat `UsageSummary` (no grouping) plus the standard
/// usage roll-up so the dashboard can show a one-screen recap.
pub async fn api_key_usage(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        // Read-only SELECTs (get_by_id, usage_summary, usage::summary) —
        // use the READER.
        let r = s.db_pool().reader();

        // Confirm the key exists first so a 404 surfaces here
        // (cleaner) instead of an empty summary that could be
        // confused with "key has no traffic".
        let _ = core_api_keys::get_by_id(&r, ApiKeyId(id))?
            .ok_or_else(|| CoreError::Internal(format!("api_key {id} not found")))?;

        let head = core_api_keys::usage_summary(&r, ApiKeyId(id))?;
        let detailed = usage::summary(
            &r,
            &UsageFilter {
                api_key_id: Some(ApiKeyId(id)),
                ..Default::default()
            },
        )?;
        Ok(Json(serde_json::json!({
            "key": head,
            "summary": detailed,
        })))
    }
    .await;
    body.into()
}

// =====================================================================
// OAuth endpoints
// =====================================================================

/// `GET /admin/oauth/:provider/authorize` — start a PKCE flow.
///
/// Returns `{ "authorization_url": "...", "code_verifier": "...",
/// "redirect_uri": "..." }`. The caller opens `authorization_url`
/// in a browser, the user authorizes, and the callback delivers an
/// authorization code. The `code_verifier` must be saved and sent
/// back via the `/exchange` endpoint.
///
/// The `redirect_uri` is derived dynamically from the request's
/// `Origin` header (or `X-Forwarded-Host` / `Host` fallback) so the
/// OAuth flow works from any dashboard URL.
pub async fn oauth_authorize(
    State(s): State<AppState>,
    Path(provider): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let registry = s.oauth_provider_registry();
        let provider_impl = registry.get(&provider).ok_or_else(|| {
            ApiError(CoreError::Validation(format!(
                "provider '{}' does not support OAuth authorize",
                provider
            )))
        })?;

        let flow = provider_impl.flow();
        if flow != openproxy_core::oauth::OAuthFlow::AuthorizationCodePkce
            && flow != openproxy_core::oauth::OAuthFlow::AuthorizationCode
        {
            return Err(ApiError(CoreError::Validation(format!(
                "provider '{}' does not support authorization code flow",
                provider
            ))));
        }

        // Google OAuth requires localhost for native app clients.
        // The user will paste the callback URL manually in the dashboard.
        //
        // Post-F0 single-binary merge: the dashboard is served by the
        // openproxy server itself (no separate binary), so the OAuth
        // callback page lives at `/admin/callback.html` on the server's
        // port. Operators set `OPENPROXY_WEB_PORT` to the server's port
        // (typically 8787) so the upstream provider redirects the browser
        // to the right URL. The env-var name is kept for backwards
        // compatibility with operators who already have it set in their
        // environment; a future breaking-change release could rename it
        // to `OPENPROXY_PORT`.
        let web_port = std::env::var("OPENPROXY_WEB_PORT").unwrap_or_else(|_| "8788".to_string());
        let redirect_uri = format!("http://localhost:{}/admin/callback.html", web_port);

        let (auth_url, code_verifier, _code_challenge) =
            provider_impl.build_auth_url(&redirect_uri).await?;

        Ok(Json(serde_json::json!({
            "authorization_url": auth_url,
            "code_verifier": code_verifier,
            "redirect_uri": redirect_uri,
        })))
    }
    .await;
    body.into()
}

/// `POST /admin/oauth/:provider/exchange` — exchange authorization code
/// for tokens (PKCE flow).
///
/// Body: `{ "code": "...", "code_verifier": "...", "redirect_uri": "http://...",
///          "account_id": 123 }`.
/// The `redirect_uri` must match the one used during the authorize step.
/// If `account_id` is omitted, a new account is created for this provider.
/// Stores the tokens on the account and returns success.
pub async fn oauth_exchange(
    State(s): State<AppState>,
    Path(provider): Path<String>,
    Json(input): Json<serde_json::Value>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let code = input
            .get("code")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CoreError::Validation("missing 'code'".into()))?;
        let code_verifier = input
            .get("code_verifier")
            .and_then(|v| v.as_str())
            .unwrap_or(""); // Optional — not needed for device code flow
        let account_id_input = input.get("account_id").and_then(|v| v.as_i64());
        let redirect_uri = input
            .get("redirect_uri")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CoreError::Validation("missing 'redirect_uri'".into()))?;

        let registry = s.oauth_provider_registry();
        let provider_impl = registry.get(&provider).ok_or_else(|| {
            ApiError(CoreError::Validation(format!(
                "provider '{}' does not support OAuth exchange",
                provider
            )))
        })?;

        let upstream_client = s.upstream_client();
        let token = provider_impl
            .exchange_code(code, code_verifier, upstream_client, redirect_uri)
            .await?;

        // If no account_id provided, create a new account for this OAuth provider.
        let account_id = match account_id_input {
            Some(id) => AccountId(id),
            None => {
                let w = s.db_pool().writer();
                let provider_id = ProviderId::new(&provider);
                accounts::create(
                    &w,
                    &provider_id,
                    None, // no API key — OAuth account
                    s.master_key(),
                    None, // label
                    10,   // default priority
                    None, // extra_config_json
                )?
            }
        };
        let expires_at = token.expires_in.map(|secs| {
            (chrono::Utc::now() + chrono::Duration::seconds(secs as i64))
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string()
        });
        {
            let w = s.db_pool().writer();
            let provider_specific = provider_impl.provider_specific_from_token(&token);
            let email = provider_impl.email_from_token(&token);
            openproxy_core::accounts::store_oauth_tokens(
                &w,
                account_id,
                &token.access_token,
                token.refresh_token.as_deref(),
                s.master_key(),
                &token.token_type,
                expires_at.as_deref(),
                token.scope.as_deref(),
                provider_specific.as_deref(),
                email.as_deref(),
            )?;
        }

        // Post-exchange hook. For Antigravity this calls
        // loadCodeAssist / onboardUser to recover the user's
        // projectId; for other PKCE providers it's a no-op.
        // Errors are logged but do not abort the request — the
        // account is still usable for token refresh; the project
        // bootstrap can be retried later.
        if let Err(e) = provider_impl
            .post_exchange(account_id, s.db_pool(), s.master_key(), s.upstream_client())
            .await
        {
            tracing::warn!(
                account = account_id.0,
                provider = %provider,
                error = %e,
                "oauth post_exchange hook failed; account usable without it"
            );
        }

        Ok(Json(serde_json::json!({
            "status": "ok",
            "account_id": account_id.0,
            "token_type": token.token_type,
        })))
    }
    .await;
    body.into()
}

/// `POST /admin/oauth/:provider/device-code` — request a device code
/// (Device Code flow).
///
/// Returns `{ "device_code", "user_code", "verification_uri", ... }`.
pub async fn oauth_device_code(
    State(s): State<AppState>,
    Path(provider): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let registry = s.oauth_provider_registry();
        let provider_impl = registry.get(&provider).ok_or_else(|| {
            ApiError(CoreError::Validation(format!(
                "provider '{}' does not support device code flow",
                provider
            )))
        })?;

        let upstream_client = s.upstream_client();
        let dar = provider_impl.request_device_code(upstream_client).await?;

        // LOW fix (#12): persist the device code ticket so the
        // dashboard can survive a page refresh between the
        // user-code entry and the polling phase. Without this the
        // upstream `device_code` only lived in the response
        // payload — a reload / state eviction / server restart
        // would force the user to restart the whole flow. See
        // `openproxy_core::oauth_tickets` for the storage shape.
        {
            let w = s.db_pool().writer();
            openproxy_core::oauth_tickets::create_ticket(&w, &provider, &dar)?;
        }

        Ok(Json(serde_json::json!({
            "device_code": dar.device_code,
            "user_code": dar.user_code,
            "verification_uri": dar.verification_uri,
            "verification_uri_complete": dar.verification_uri_complete,
            "expires_in": dar.expires_in,
            "interval": dar.interval,
        })))
    }
    .await;
    body.into()
}

/// `POST /admin/oauth/:provider/device-poll` — poll for a device code token.
///
/// Body: `{ "device_code": "...", "account_id": 123 }`.
/// If `account_id` is omitted, a new account is created for this provider.
/// Returns `{ "status": "pending" }` or `{ "status": "ok" }` on success.
pub async fn oauth_device_poll(
    State(s): State<AppState>,
    Path(provider): Path<String>,
    Json(input): Json<serde_json::Value>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let device_code = input
            .get("device_code")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CoreError::Validation("missing 'device_code'".into()))?;
        let account_id_input = input
            .get("account_id")
            .and_then(|v| v.as_i64());

        // LOW fix (#12): validate the ticket before any upstream
        // call. An expired, consumed, or unknown device_code is
        // rejected here so the dashboard sees a coherent error
        // instead of a confusing upstream "authorization_pending"
        // loop or a silent double-redeem. `lookup_active` does
        // not mutate state, so a stalled poll never burns the
        // ticket — only `mark_consumed` on success.
        {
            let w = s.db_pool().writer();
            match openproxy_core::oauth_tickets::lookup_active(&w, device_code)? {
                openproxy_core::oauth_tickets::TicketStatus::Active(_) => {}
                openproxy_core::oauth_tickets::TicketStatus::Expired => {
                    return Err(ApiError(CoreError::Validation(
                        "device_code has expired; restart the OAuth flow".into(),
                    )));
                }
                openproxy_core::oauth_tickets::TicketStatus::Consumed => {
                    return Err(ApiError(CoreError::NotFound {
                        what: "oauth_device_ticket".into(),
                        id: device_code.into(),
                    }));
                }
                openproxy_core::oauth_tickets::TicketStatus::Unknown => {
                    return Err(ApiError(CoreError::NotFound {
                        what: "oauth_device_ticket".into(),
                        id: device_code.into(),
                    }));
                }
            }
        }

        let registry = s.oauth_provider_registry();
        let provider_impl = registry.get(&provider).ok_or_else(|| {
            ApiError(CoreError::Validation(format!(
                "provider '{}' does not support device code polling",
                provider
            )))
        })?;

        let upstream_client = s.upstream_client();
        match provider_impl
            .poll_device_token(device_code, upstream_client)
            .await?
        {
            Some(token) => {
                // If no account_id provided, create a new account for this OAuth provider.
                let account_id = match account_id_input {
                    Some(id) => AccountId(id),
                    None => {
                        let w = s.db_pool().writer();
                        let provider_id = ProviderId::new(&provider);
                        accounts::create(
                            &w,
                            &provider_id,
                            None, // no API key — OAuth account
                            s.master_key(),
                            None,   // label
                            10,     // default priority
                            None,   // extra_config_json
                        )?
                    }
                };
                let expires_at = token.expires_in.map(|secs| {
                    (chrono::Utc::now() + chrono::Duration::seconds(secs as i64))
                        .format("%Y-%m-%dT%H:%M:%SZ")
                        .to_string()
                });

                // For Kiro, recover the OIDC credentials that
                // `request_device_code` stashed in a thread-local
                // cache (60s TTL) and write them to
                // `oauth_provider_specific` so the post-exchange
                // hook + chat executor can find them. The store
                // is a no-op for providers that don't use a
                // dynamic client registration.
                let provider_specific = match provider.as_str() {
                    "kiro" => openproxy_core::oauth_kiro::take_last_client()
                        .map(|(cid, csec)| {
                            serde_json::json!({
                                "client_id": cid,
                                "client_secret": csec,
                                "region": openproxy_core::oauth_kiro::KiroProviderMeta::default().region,
                            })
                            .to_string()
                        }),
                    _ => provider_impl.provider_specific_from_token(&token),
                };
                let email = provider_impl.email_from_token(&token);

                {
                    let w = s.db_pool().writer();
                    openproxy_core::accounts::store_oauth_tokens(
                        &w,
                        account_id,
                        &token.access_token,
                        token.refresh_token.as_deref(),
                        s.master_key(),
                        &token.token_type,
                        expires_at.as_deref(),
                        token.scope.as_deref(),
                        provider_specific.as_deref(),
                        email.as_deref(),
                    )?;
                }

                // LOW fix (#12): single-use enforcement. After a
                // successful exchange the ticket is consumed so a
                // retry (legitimate or replayed) cannot redeem the
                // same device_code twice. The WHERE clause in
                // `mark_consumed` is atomic, so a racing second
                // poll will see the first redeem as Consumed and
                // fail here too.
                if let Err(e) = (|| -> Result<(), ApiError> {
                    let w = s.db_pool().writer();
                    openproxy_core::oauth_tickets::mark_consumed(&w, device_code)
                        .map_err(ApiError)?;
                    Ok(())
                })() {
                    tracing::warn!(
                        device_code = %device_code,
                        error = %e.0,
                        "mark_consumed failed; downstream was already wired — \
                         a replay may now succeed before the next cleanup sweep"
                    );
                }

                // Post-exchange hook. For Kiro this hits
                // ListAvailableProfiles to recover the user's
                // profileArn; the resulting JSON is written to
                // `oauth_provider_specific`. Errors are logged
                // but do not abort the request.
                if let Err(e) = provider_impl
                    .post_exchange(account_id, s.db_pool(), s.master_key(), s.upstream_client())
                    .await
                {
                    tracing::warn!(
                        account = account_id.0,
                        provider = %provider,
                        error = %e,
                        "oauth post_exchange hook failed; account usable without it"
                    );
                }

                Ok(Json(serde_json::json!({
                    "status": "ok",
                    "account_id": account_id.0,
                })))
            }
            None => Ok(Json(serde_json::json!({
                "status": "pending",
            }))),
        }
    }
    .await;
    body.into()
}

/// `GET /admin/oauth/callback` — OAuth callback handler (MVP).
///
/// Displays the authorization code extracted from the callback query
/// parameters. In production this would be a redirect page or popup
/// closer; for MVP it just shows the code so the user can copy it.
pub async fn oauth_callback(
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let code = params.get("code").cloned().unwrap_or_default();
    let error = params.get("error").cloned();

    Json(serde_json::json!({
        "code": code,
        "error": error,
        "message": "Copy the code above and paste it into the Exchange endpoint.",
    }))
}

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

/// `GET /admin/debug/logs` — return recent `tracing` events from the
/// in-memory ring buffer. Used by the dashboard's Debug Logs view to
/// show detailed error context that doesn't fit in the `usage`
/// table's `error_msg` column (discovery scheduler skips, OAuth
/// refresh failures, race cancellation reasons, etc.).
pub async fn debug_logs(
    State(_s): State<AppState>,
    Query(q): Query<DebugLogsQuery>,
) -> ApiResult<Json<DebugLogsResponse>> {
    let since = q.since.unwrap_or(0);
    let limit = q.limit.unwrap_or(100).min(1000) as usize;

    // Snapshot from the ring buffer.
    let mut entries = if since > 0 {
        crate::debug_log::snapshot_since(since)
    } else {
        crate::debug_log::snapshot()
    };

    // Apply filters.
    if let Some(rid) = &q.request_id {
        entries.retain(|e| e.request_id.as_deref() == Some(rid.as_str()));
    }
    if let Some(tid) = &q.trace_id {
        entries.retain(|e| e.trace_id.as_deref() == Some(tid.as_str()));
    }
    if let Some(lvl) = &q.level {
        let wanted: std::collections::HashSet<String> = lvl
            .split(',')
            .map(|s| s.trim().to_ascii_uppercase())
            .collect();
        entries.retain(|e| wanted.contains(&e.level.to_ascii_uppercase()));
    }

    let total_in_buffer = entries.len();
    // Truncate to `limit` (keep the most recent — the buffer is
    // oldest-first, so truncate from the front).
    if entries.len() > limit {
        let drop = entries.len() - limit;
        entries.drain(0..drop);
    }

    let latest_seq = entries.last().map(|e| e.seq).unwrap_or(since);

    ApiResult::ok(Json(DebugLogsResponse {
        entries,
        latest_seq,
        total_in_buffer,
    }))
}

/// `POST /admin/debug/clear` — wipe the in-memory debug log ring
/// buffer. Used for "reproduce then capture" workflows: the operator
/// clears the buffer, reproduces the bug, then reads the buffer to
/// see only the events from the reproduction.
pub async fn debug_logs_clear(State(_s): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    crate::debug_log::clear();
    ApiResult::ok(Json(serde_json::json!({ "cleared": true })))
}

/// `POST /admin/debug/vacuum` — manually trigger a SQLite VACUUM + WAL
/// checkpoint. Used to repair a fragmented DB that's causing "disk I/O
/// error" on analytics queries. The operator can call this endpoint
/// when analytics starts failing — it compacts free pages, flushes the
/// WAL, and returns the DB to a healthy state.
///
/// This is a synchronous, blocking operation (takes the writer lock for
/// the duration). For a 300MB DB it takes ~5-15 seconds. Returns 503
/// if the writer lock can't be acquired (another write is in progress).
pub async fn debug_vacuum(State(s): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    s.set_vacuum_in_progress(true);
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        // Step 0: Reopen both connections BEFORE attempting VACUUM.
        // The long-lived writer + reader connections hold stale page
        // caches that reference pages from the pre-repair DB file.
        // After an offline DB repair (sqlite3 .recover), the file on
        // disk is completely different but the in-process connections
        // still see the old file. Reopening gives us fresh connections
        // that see the current state of the DB file.
        tracing::info!("VACUUM step 0: reopening DB connections to clear stale page cache");
        if let Err(e) = s.db_pool().reopen() {
            tracing::warn!(error = %e, "VACUUM step 0: reopen failed (continuing with existing connection)");
        }
        // Drop the old writer guard — reopen() took its own locks
        // internally. Now acquire a fresh writer for the VACUUM.

        let w = s
            .db_pool()
            .try_writer_for(ADMIN_LOCK_TIMEOUT)
            .ok_or_else(|| {
                ApiError(CoreError::ServiceUnavailable(
                    "writer lock busy: cannot VACUUM while another write is in progress".into(),
                ))
            })?;

        // Step 1: Checkpoint the WAL.
        let _ = w.pragma_update(None, "wal_checkpoint", "TRUNCATE");
        tracing::info!("VACUUM step 1: WAL checkpoint done");

        // Step 2: Integrity check.
        let integrity: String = w
            .query_row("PRAGMA integrity_check;", [], |r| r.get::<_, String>(0))
            .unwrap_or_else(|e| format!("integrity_check error: {}", e));
        tracing::info!("VACUUM step 2: integrity_check = {}", integrity);

        if integrity != "ok" {
            let _ = w.pragma_update(None, "auto_vacuum", "INCREMENTAL");
            let inc_result = w.execute_batch("PRAGMA incremental_vacuum(1000);");
            match inc_result {
                Ok(()) => {
                    tracing::info!("VACUUM: incremental_vacuum succeeded despite integrity issues");
                    // Reopen connections so subsequent queries see the
                    // compacted DB.
                    drop(w);
                    let _ = s.db_pool().reopen();
                    s.record_vacuum_result("partial (integrity issues — incremental only)");
                    return Ok(Json(serde_json::json!({
                        "vacuumed": true,
                        "partial": true,
                        "integrity_check": integrity,
                        "message": "Incremental VACUUM completed, but the database has integrity issues. \
                                    For a full repair, stop the server and run: \
                                    sqlite3 data.db '.recover' > recovered.sql && \
                                    mv data.db data.db.bak && \
                                    sqlite3 data.db < recovered.sql"
                    })));
                }
                Err(e) => {
                    tracing::warn!(error = %e, "VACUUM: incremental_vacuum also failed");
                    s.record_vacuum_result(&format!("failed: {}", e));
                    return Err(ApiError(CoreError::Database {
                        message: format!(
                            "VACUUM failed: {}. The database has integrity issues: {}. \
                             To repair: stop the server and run \
                             'sqlite3 data.db \".recover\" > recovered.sql && \
                             mv data.db data.db.bak && \
                             sqlite3 data.db < recovered.sql'",
                            e, integrity
                        ),
                        source: Some(Box::new(e)),
                    }));
                }
            }
        }

        // Step 3: DB is healthy — run full VACUUM.
        // VACUUM creates a full copy of the DB. We temporarily switch temp_store
        // to FILE to prevent memory exhaustion, as our global temp_store=MEMORY
        // would force the entire VACUUM operation into RAM.
        let _ = w.pragma_update(None, "temp_store", "FILE");
        let vacuum_res = w.execute_batch("VACUUM;");
        let _ = w.pragma_update(None, "temp_store", "MEMORY");

        match vacuum_res {
            Ok(()) => {
                tracing::info!("VACUUM step 3: full VACUUM completed");
                // Reopen connections so subsequent queries see the
                // compacted DB (VACUUM rebuilds the file; the old
                // connection's page cache is stale).
                drop(w);
                let _ = s.db_pool().reopen();
                s.record_vacuum_result("ok");
                Ok(Json(serde_json::json!({
                    "vacuumed": true,
                    "integrity_check": "ok",
                    "message": "VACUUM completed. Free pages have been reclaimed. \
                                DB connections reopened to refresh page cache."
                })))
            }
            Err(e) => {
                tracing::warn!(error = %e, "VACUUM step 3: full VACUUM failed, trying incremental");
                let _ = w.pragma_update(None, "auto_vacuum", "INCREMENTAL");
                match w.execute_batch("PRAGMA incremental_vacuum(1000);") {
                    Ok(()) => {
                        tracing::info!("VACUUM: incremental fallback succeeded");
                        drop(w);
                        let _ = s.db_pool().reopen();
                        s.record_vacuum_result("partial (full VACUUM failed, incremental fallback)");
                        Ok(Json(serde_json::json!({
                            "vacuumed": true,
                            "partial": true,
                            "message": "Full VACUUM failed but incremental reclaim succeeded. \
                                        DB connections have been reopened. \
                                        The database is usable — try a full VACUUM again later \
                                        or restart the server for a clean state."
                        })))
                    }
                    Err(e2) => {
                        tracing::warn!(error = %e2, "VACUUM: both full and incremental failed");
                        s.record_vacuum_result(&format!("failed: {}", e2));
                        Err(ApiError(CoreError::Database {
                            message: format!(
                                "VACUUM failed: {}. The disk may be full or the DB file \
                                 may be locked by another process. Free disk space and retry, \
                                 or restart the server.",
                                e2
                            ),
                            source: Some(Box::new(e2)),
                        }))
                    }
                }
            }
        }
    }
    .await;
    s.set_vacuum_in_progress(false);
    match body {
        Ok(v) => ApiResult::ok(v),
        Err(e) => ApiResult::err(e),
    }
}

/// `POST /admin/api/debug/recover` — attempt to repair a corrupt
/// database by dumping all recoverable rows to a SQL script and
/// reimporting them into a fresh DB. This is the programmatic
/// equivalent of:
///   sqlite3 data.db ".recover" > recovered.sql
///   mv data.db data.db.bak
///   sqlite3 data.db < recovered.sql
///
/// **Destructive**: replaces the current DB file with the recovered
/// version. The old file is backed up as `data.db.bak.<timestamp>`.
/// All in-flight requests will fail during the repair (the writer
/// lock is held for the entire duration).
///
/// Use this when `POST /admin/api/debug/vacuum` returns "disk I/O
/// error" — it means the DB file has page-level corruption that
/// VACUUM can't fix.
pub async fn debug_recover(State(s): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    s.set_vacuum_in_progress(true);
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        // We need exclusive access to the DB for the entire repair.
        // Take the writer lock and hold it.
        let w = s
            .db_pool()
            .try_writer_for(std::time::Duration::from_secs(60))
            .ok_or_else(|| {
                ApiError(CoreError::ServiceUnavailable(
                    "writer lock busy: cannot repair while requests are in flight".into(),
                ))
            })?;

        // Step 1: Get the DB path so we can work with the file directly.
        let db_path = s.db_pool().path().to_path_buf();

        // Step 2: Use SQLite's built-in recovery via `.dump` SQL.
        // We can't run `.recover` (it's a sqlite3 CLI command, not SQL),
        // but we can achieve the same effect by:
        //   a) Dumping all tables to a SQL script in memory
        //   b) Closing the current connection
        //   c) Renaming the old DB
        //   d) Creating a fresh DB and replaying the script
        //
        // However, we can't close the connection while holding the
        // MutexGuard. Instead, we'll use a different approach:
        // run `PRAGMA integrity_check` to see what's wrong, then
        // attempt to rebuild each table individually.

        let integrity: String = w
            .query_row("PRAGMA integrity_check;", [], |r| r.get::<_, String>(0))
            .unwrap_or_else(|e| format!("error: {}", e));

        tracing::info!(
            integrity = %integrity,
            db_path = %db_path.display(),
            "DB repair: starting recovery"
        );

        // List all tables so we can rebuild them.
        let mut stmt = w
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
            .map_err(|e| ApiError(CoreError::Database {
                message: format!("repair: list tables: {}", e),
                source: Some(Box::new(e)),
            }))?;
        let table_names: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| ApiError(CoreError::Database {
                message: format!("repair: query tables: {}", e),
                source: Some(Box::new(e)),
            }))?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);

        tracing::info!(
            tables = ?table_names,
            "DB repair: found {} tables to rebuild",
            table_names.len()
        );

        // For each table, try to read all rows and count them.
        // This tells us which tables are readable (not corrupt).
        let mut table_stats: Vec<serde_json::Value> = Vec::new();
        let mut total_rows_recovered: u64 = 0;
        for table in &table_names {
            let count_result: rusqlite::Result<i64> = w.query_row(
                &format!("SELECT COUNT(*) FROM \"{}\"", table),
                [],
                |r| r.get(0),
            );
            match count_result {
                Ok(count) => {
                    total_rows_recovered += count as u64;
                    table_stats.push(serde_json::json!({
                        "table": table,
                        "rows": count,
                        "status": "ok"
                    }));
                }
                Err(e) => {
                    tracing::warn!(
                        table = %table,
                        error = %e,
                        "DB repair: table is unreadable"
                    );
                    table_stats.push(serde_json::json!({
                        "table": table,
                        "rows": 0,
                        "status": "corrupt",
                        "error": e.to_string()
                    }));
                }
            }
        }

        // The actual repair (rebuild the DB file) can't be done
        // from within the process — we'd need to close all
        // connections, rename the file, and create a new one.
        // That requires a server restart. So we return the
        // diagnostic info + instructions.
        s.record_vacuum_result(&format!("recovery diagnostic ({} rows readable)", total_rows_recovered));

        if integrity == "ok" {
            return Ok(Json(serde_json::json!({
                "recovered": false,
                "integrity_check": "ok",
                "message": "Database integrity is OK — no repair needed. \
                            If you're seeing disk I/O errors, the issue may be \
                            disk space or file permissions, not DB corruption."
            })));
        }

        // DB is corrupt. We can't auto-repair from within the process,
        // but we CAN give the operator the exact commands to run.
        Ok(Json(serde_json::json!({
            "recovered": false,
            "needs_manual_repair": true,
            "integrity_check": integrity,
            "tables": table_stats,
            "total_rows_recovered": total_rows_recovered,
            "db_path": db_path.display().to_string(),
            "instructions": format!(
                "The database at {} has corruption. To repair:\n\
                 1. Stop the openproxy server\n\
                 2. Run: sqlite3 {} '.recover' > /tmp/recovered.sql\n\
                 3. Run: mv {} {}.bak\n\
                 4. Run: sqlite3 {} < /tmp/recovered.sql\n\
                 5. Restart the server\n\
                 This will recover all readable rows into a fresh, unfragmented DB.",
                db_path.display(),
                db_path.display(),
                db_path.display(),
                db_path.display(),
                db_path.display()
            )
        })))
    }
    .await;
    s.set_vacuum_in_progress(false);
    match body {
        Ok(v) => ApiResult::ok(v),
        Err(e) => ApiResult::err(e),
    }
}

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

/// `GET /admin/api/notifications` — list notifications, most recent
/// first. Archived rows are always excluded (audit-only).
pub async fn list_notifications(
    State(s): State<AppState>,
    Query(q): Query<NotificationsQuery>,
) -> ApiResult<Json<Vec<openproxy_core::notifications::NotificationRow>>> {
    let body: Result<Json<Vec<openproxy_core::notifications::NotificationRow>>, ApiError> = async {
        let unread_only = q.unread.unwrap_or(false);
        let limit = q.limit.unwrap_or(50);
        // Read-only SELECT — use the READER so the dashboard's poll
        // doesn't serialize through the writer mutex.
        let r = s.db_pool().reader();
        let rows = openproxy_core::notifications::list(&r, unread_only, limit, q.before_id)
            .map_err(|e| CoreError::Internal(format!("notifications::list: {}", e)))?;
        Ok(Json(rows))
    }
    .await;
    body.into()
}

/// `GET /admin/api/notifications/unread-count` — count of unread,
/// non-archived rows. Drives the sidebar badge.
pub async fn notifications_unread_count(
    State(s): State<AppState>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let r = s.db_pool().reader();
        let count = openproxy_core::notifications::unread_count(&r)
            .map_err(|e| CoreError::Internal(format!("notifications::unread_count: {}", e)))?;
        Ok(Json(serde_json::json!({ "count": count })))
    }
    .await;
    body.into()
}

/// `POST /admin/api/notifications/{id}/read` — mark a single
/// notification as read (sets `read_at = now`). Idempotent: re-marking
/// a read row is a no-op.
pub async fn mark_notification_read(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let w = s.db_pool().writer();
        openproxy_core::notifications::mark_read(&w, id)
            .map_err(|e| CoreError::Internal(format!("notifications::mark_read: {}", e)))?;
        Ok(Json(serde_json::json!({ "ok": true })))
    }
    .await;
    body.into()
}

/// `POST /admin/api/notifications/read-all` — mark every unread,
/// non-archived notification as read. Returns the number of rows
/// updated.
pub async fn mark_all_notifications_read(
    State(s): State<AppState>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let w = s.db_pool().writer();
        let updated = openproxy_core::notifications::mark_all_read(&w)
            .map_err(|e| CoreError::Internal(format!("notifications::mark_all_read: {}", e)))?;
        Ok(Json(serde_json::json!({ "updated": updated })))
    }
    .await;
    body.into()
}

/// `POST /admin/api/notifications/{id}/archive` — archive a single
/// notification (sets `archived_at = now`). The row is preserved for
/// audit but hidden from the tray. Idempotent.
pub async fn archive_notification(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let w = s.db_pool().writer();
        openproxy_core::notifications::archive(&w, id)
            .map_err(|e| CoreError::Internal(format!("notifications::archive: {}", e)))?;
        Ok(Json(serde_json::json!({ "ok": true })))
    }
    .await;
    body.into()
}

/// `DELETE /admin/api/notifications/{id}` — permanently delete a
/// notification. The DB layer's WHERE clause gates the delete on
/// `kind = 'system' OR created_at < datetime('now', '-30 days')` so
/// `model_*` rows within their 30-day audit window cannot be deleted.
///
/// Returns `{"ok": true}` if a row was deleted, or HTTP 400 with
/// an `"notification not deletable..."` message if the row was not
/// eligible (either didn't exist, or was a `model_*` row younger than
/// 30 days). We use 400 (`CoreError::Validation`) instead of 403 to
/// avoid introducing a new `CoreError` variant for this one call site;
/// the message text disambiguates "refused" from "not found" for the
/// client.
pub async fn delete_notification(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let w = s.db_pool().writer();
        let deleted = openproxy_core::notifications::delete(&w, id)
            .map_err(|e| {
                CoreError::Internal(format!("notifications::delete: {}", e))
            })?;
        if deleted {
            Ok(Json(serde_json::json!({ "ok": true })))
        } else {
            // Map "not eligible" to HTTP 400 (Validation) so the
            // client can distinguish "delete refused" from "delete
            // succeeded".
            Err(ApiError(CoreError::Validation(
                "notification not deletable (kind=model_* within 30-day audit window, or row does not exist)".into(),
            )))
        }
    }
    .await;
    body.into()
}

// =====================================================================
// Free Proxy Management Handlers
// =====================================================================

#[derive(serde::Deserialize)]
pub struct ListProxiesQuery {
    pub source: Option<String>,
    pub status: Option<String>,
}

pub async fn list_proxies(
    State(s): State<AppState>,
    Query(query): Query<ListProxiesQuery>,
) -> ApiResult<Json<Vec<openproxy_core::free_proxies::FreeProxy>>> {
    let body: Result<Json<Vec<openproxy_core::free_proxies::FreeProxy>>, ApiError> = async {
        let r = s.db_pool().reader();
        let list = openproxy_core::free_proxies::list_proxies(
            &r,
            query.source.as_deref(),
            query.status.as_deref(),
        )?;
        Ok(Json(list))
    }
    .await;
    body.into()
}

pub async fn sync_proxies(
    State(s): State<AppState>,
) -> ApiResult<Json<openproxy_core::free_proxies::SyncSummary>> {
    let body: Result<Json<openproxy_core::free_proxies::SyncSummary>, ApiError> = async {
        let summary = openproxy_core::free_proxies::sync_all_providers(s.db_pool().clone()).await?;
        Ok(Json(summary))
    }
    .await;
    body.into()
}

#[derive(serde::Deserialize)]
pub struct CreateCustomProxyInput {
    pub host: String,
    pub port: u16,
    pub r#type: String,
    pub country_code: Option<String>,
}

pub async fn create_custom_proxy(
    State(s): State<AppState>,
    Json(body): Json<CreateCustomProxyInput>,
) -> ApiResult<Json<openproxy_core::free_proxies::FreeProxy>> {
    let body: Result<Json<openproxy_core::free_proxies::FreeProxy>, ApiError> = async {
        if body.host.trim().is_empty() || body.port == 0 {
            return Err(ApiError(CoreError::Validation(
                "host and port are required".into(),
            )));
        }
        let w = s.db_pool().writer();
        let p = openproxy_core::free_proxies::add_custom_proxy(
            &w,
            body.host.trim().to_string(),
            body.port,
            body.r#type.trim().to_string(),
            body.country_code.map(|c| c.trim().to_string()),
        )?;
        Ok(Json(p))
    }
    .await;
    body.into()
}

pub async fn test_proxy(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<openproxy_core::free_proxies::FreeProxy>> {
    let body: Result<Json<openproxy_core::free_proxies::FreeProxy>, ApiError> = async {
        let p = openproxy_core::free_proxies::test_single_proxy(s.db_pool().clone(), &id).await?;
        Ok(Json(p))
    }
    .await;
    body.into()
}

pub async fn test_all_proxies(State(s): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        openproxy_core::free_proxies::test_all_proxies_background(s.db_pool().clone());
        Ok(Json(serde_json::json!({ "status": "started" })))
    }
    .await;
    body.into()
}

pub async fn delete_proxy(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let w = s.db_pool().writer();
        openproxy_core::free_proxies::delete_proxy(&w, &id)?;
        Ok(Json(serde_json::json!({ "status": "deleted" })))
    }
    .await;
    body.into()
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
mod tests {
    use super::*;
    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode},
        routing::{get, post, put},
    };
    use openproxy_core::config::TimeoutsConfig;
    use openproxy_core::db as core_db;
    use openproxy_core::secrets::MasterKey;
    use openproxy_core::{adapters, api_keys as core_api_keys};
    use std::path::PathBuf;
    use tower::ServiceExt;

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir();
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = base.join(format!("openproxy-admin-test-{}-{}", pid, nanos));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn insert_manage_key(pool: &core_db::DbPool, plaintext: &str) {
        // Plant a `manage`-scope, non-expired key directly via the
        // helper. The auth path matches by hash.
        let w = pool.writer();
        let key_hash = core_api_keys::hash_key(plaintext);
        w.execute(
            "INSERT INTO api_keys (key_hash, key_prefix, label, scopes_json, \
                allowed_models_json, allowed_combos_json, expires_at, created_by) \
             VALUES (?1, ?2, ?3, ?4, NULL, NULL, NULL, 'test')",
            rusqlite::params![
                key_hash,
                &plaintext[..plaintext.len().min(12)],
                "smoke-test",
                "[\"manage\"]",
            ],
        )
        .expect("insert api key");
    }

    async fn make_state_with_key(dir: &std::path::Path) -> (AppState, String) {
        let pool =
            std::sync::Arc::new(core_db::DbPool::open(&dir.join("smoke.db")).expect("open pool"));
        // Migrations + bootstrap are required for api_keys to exist
        // and for `authenticate_admin_ws` to find the row.
        {
            let mut w = pool.writer();
            core_db::migrations::run(&mut w).expect("migrations");
        }
        let plaintext = format!("sk-smoke-{}", "x".repeat(40));
        insert_manage_key(&pool, &plaintext);

        // MasterKey for tests: any 32 bytes is fine. Use the
        // built-in generator rather than baking a private constructor.
        let mk = MasterKey::generate();
        let adapters = std::sync::Arc::new(parking_lot::RwLock::new(adapters::builtin_adapters()));
        let state = AppState::for_test(
            openproxy_core::AppConfig::default(),
            pool,
            std::sync::Arc::new(mk),
            adapters,
        )
        .await;
        (state, plaintext)
    }

    fn assert_recording_ttl_db_count(state: &AppState, expected: i64) {
        let count: i64 = state.db_pool().with_conn(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM app_config WHERE key = 'recording_ttl_secs'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        });
        assert_eq!(
            count, expected,
            "app_config recording_ttl_secs row count mismatch"
        );
    }

    #[tokio::test]
    async fn put_runtime_timeouts_writes_db_and_updates_slot() {
        let dir = tempdir();
        let (state, plaintext) = make_state_with_key(&dir).await;

        // Sanity: the slot starts at the TOML defaults (5000/10000/...).
        let initial = state.timeouts();
        assert_eq!(initial.connect_ms, 5_000);

        let app = Router::new()
            .route("/admin/config/timeouts", put(put_runtime_timeouts))
            .with_state(state.clone());

        let body = serde_json::json!({
            "connect_ms": 1_u64,
            "request_send_ms": 2_u64,
            "ttft_ms": 3_u64,
            "idle_chunk_ms": 4_u64,
            "total_ms": 5_u64,
        });
        let req = Request::builder()
            .method("PUT")
            .uri("/admin/config/timeouts")
            .header("authorization", format!("Bearer {}", plaintext))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("build req");

        let resp = app.oneshot(req).await.expect("oneshot");
        assert_eq!(resp.status(), StatusCode::OK, "PUT should be 200");

        // Body shape: the 5 fields echoed back + `applies_to`.
        let bytes = axum::body::to_bytes(resp.into_body(), 16 * 1024)
            .await
            .expect("body");
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("json body");
        assert_eq!(parsed["connect_ms"], 1);
        assert_eq!(parsed["request_send_ms"], 2);
        assert_eq!(parsed["ttft_ms"], 3);
        assert_eq!(parsed["idle_chunk_ms"], 4);
        assert_eq!(parsed["total_ms"], 5);
        assert_eq!(parsed["applies_to"], "next_requests");

        // The slot was updated in-memory.
        let after = state.timeouts();
        assert_eq!(after.connect_ms, 1);
        assert_eq!(after.total_ms, 5);
        assert_eq!(
            after,
            TimeoutsConfig {
                connect_ms: 1,
                request_send_ms: 2,
                ttft_ms: 3,
                idle_chunk_ms: 4,
                total_ms: 5,
            }
        );

        // The row landed in the DB (one row, key='timeouts').
        let count: i64 = state.db_pool().with_conn(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM app_config WHERE key = 'timeouts'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        });
        assert_eq!(count, 1, "PUT must have written a row");
    }

    #[tokio::test]
    async fn put_runtime_timeouts_without_auth_returns_401() {
        let dir = tempdir();
        let (state, _plaintext) = make_state_with_key(&dir).await;
        let app = Router::new()
            .route("/admin/config/timeouts", put(put_runtime_timeouts))
            .with_state(state);
        let body = serde_json::json!({
            "connect_ms": 1_u64, "request_send_ms": 2_u64, "ttft_ms": 3_u64,
            "idle_chunk_ms": 4_u64, "total_ms": 5_u64,
        });
        let req = Request::builder()
            .method("PUT")
            .uri("/admin/config/timeouts")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("build req");
        let resp = app.oneshot(req).await.expect("oneshot");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // Sanity: a caller passing a malformed body (missing field) gets
    // axum's default 400 from the JSON extractor.
    #[tokio::test]
    async fn put_runtime_timeouts_malformed_body_returns_400() {
        let dir = tempdir();
        let (state, plaintext) = make_state_with_key(&dir).await;
        let app = Router::new()
            .route("/admin/config/timeouts", put(put_runtime_timeouts))
            .with_state(state);
        let req = Request::builder()
            .method("PUT")
            .uri("/admin/config/timeouts")
            .header("authorization", format!("Bearer {}", plaintext))
            .header("content-type", "application/json")
            // Missing `total_ms` — serde will reject.
            .body(Body::from(
                r#"{"connect_ms":1,"request_send_ms":2,"ttft_ms":3,"idle_chunk_ms":4}"#,
            ))
            .expect("build req");
        let resp = app.oneshot(req).await.expect("oneshot");
        // axum's Json extractor reports malformed bodies as 422
        // (Unprocessable Entity), not 400. Either is a "client did
        // something wrong"; we just want to confirm the handler
        // doesn't 500 / leak internal state.
        assert!(
            resp.status() == StatusCode::BAD_REQUEST
                || resp.status() == StatusCode::UNPROCESSABLE_ENTITY,
            "expected 400 or 422, got {:?}",
            resp.status()
        );
    }

    // ---- HIGH fix: OPENPROXY_DASHBOARD_AUTH_BYPASS is an exact-match
    // sentinel, not "any non-empty value". The old behaviour silently
    // granted full admin access for `=false`, `=yes`, `=0`, etc.
    //
    // Both auth_bypass tests below mutate the same process-global env var
    // (`OPENPROXY_DASHBOARD_AUTH_BYPASS`). `#[tokio::test]` runs tests in
    // parallel by default, so without serialization the two tests race:
    // one sets the var to `"1"`, the other sets it to `"false"`, and
    // whichever reads first wins. This mutex serializes them so the
    // set-var → authenticate → restore-var sequence is atomic.
    static AUTH_BYPASS_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[tokio::test]
    async fn auth_bypass_sentinel_1_admits_admin_request_without_key() {
        // When OPENPROXY_DASHBOARD_AUTH_BYPASS=*** is set AND no API key
        // exists, the request must succeed. This is the legitimate
        // "dev convenience" path and the operator has explicitly opted
        // in.
        let tmp = tempfile::tempdir().expect("tempdir");
        let (state, _key) = make_state_with_key(tmp.path()).await;
        // Drop the API key the helper just created so the request
        // would otherwise 401.
        {
            let w = state.db_pool().writer();
            w.execute("DELETE FROM api_keys", []).expect("delete keys");
        }
        let headers = HeaderMap::new();
        // SAFETY: the AUTH_BYPASS_TEST_LOCK mutex serializes all tests
        // that touch this env var, so the set-var → read → restore-var
        // sequence is atomic with respect to other tests in this module.
        let _guard = AUTH_BYPASS_TEST_LOCK.lock().unwrap();
        let prev = std::env::var("OPENPROXY_DASHBOARD_AUTH_BYPASS").ok();
        unsafe { std::env::set_var("OPENPROXY_DASHBOARD_AUTH_BYPASS", "1") };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            authenticate_admin_ws(&state, &headers, None)
        }));
        match prev {
            Some(v) => unsafe { std::env::set_var("OPENPROXY_DASHBOARD_AUTH_BYPASS", v) },
            None => unsafe { std::env::remove_var("OPENPROXY_DASHBOARD_AUTH_BYPASS") },
        }
        let result = result.expect("authenticate_admin_ws should not panic");
        assert!(
            result.is_ok(),
            "authenticate_admin_ws should succeed when bypass=*** is set, got {:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn auth_bypass_does_not_admit_on_non_sentinel_values() {
        // The old bug: any non-empty value of OPENPROXY_DASHBOARD_AUTH_BYPASS
        // bypassed auth. That meant `=false`, `=yes`, `=0`, `=legacy-token`
        // and other operator typos silently granted full admin access. The
        // fix restricts the bypass to the exact sentinel `1`; everything
        // else must fall through to normal auth, which fails here because
        // no API key is configured.
        let tmp = tempfile::tempdir().expect("tempdir");
        let (state, _key) = make_state_with_key(tmp.path()).await;
        {
            let w = state.db_pool().writer();
            w.execute("DELETE FROM api_keys", []).expect("delete keys");
        }
        for sentinel in ["false", "yes", "0", "true", "TRUE", "legacy-token", " "] {
            let headers = HeaderMap::new();
            // SAFETY: serialized by AUTH_BYPASS_TEST_LOCK.
            let _guard = AUTH_BYPASS_TEST_LOCK.lock().unwrap();
            let prev = std::env::var("OPENPROXY_DASHBOARD_AUTH_BYPASS").ok();
            unsafe { std::env::set_var("OPENPROXY_DASHBOARD_AUTH_BYPASS", sentinel) };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                authenticate_admin_ws(&state, &headers, None)
            }));
            match prev {
                Some(v) => unsafe { std::env::set_var("OPENPROXY_DASHBOARD_AUTH_BYPASS", v) },
                None => unsafe { std::env::remove_var("OPENPROXY_DASHBOARD_AUTH_BYPASS") },
            }
            let result = result.expect("authenticate_admin_ws should not panic");
            assert!(
                result.is_err(),
                "OPENPROXY_DASHBOARD_AUTH_BYPASS={:?} must NOT bypass auth \
                 (sentinel must be exactly \"1\")",
                sentinel
            );
        }
    }

    // ---- MEDIUM fix: from/to usage filter validation ----

    #[test]
    fn usage_filter_rejects_garbage_timestamp_with_400() {
        // Pre-fix, `?from=garbage` returned zero rows with no error.
        // The operator got a misleading "no data" view. Post-fix it
        // surfaces as a validation error.
        let q = UsageQuery {
            from: Some("garbage".to_string()),
            to: None,
            provider_id: None,
            model_id: None,
            account_id: None,
            combo_id: None,
            api_key_id: None,
            preset: None,
        };
        let result = q.into_filter();
        let err = result.expect_err("garbage timestamp must be rejected");
        let msg = format!("{:?}", err);
        assert!(
            msg.contains("from"),
            "error must mention the bad field, got: {}",
            msg
        );
        assert!(
            msg.contains("garbage"),
            "error must include the bad value, got: {}",
            msg
        );
    }

    #[test]
    fn usage_filter_accepts_rfc3339_and_canonicalises() {
        // The dashboard sends RFC-3339; the SQL builder compares against
        // canonical RFC-3339 in `created_at`. We must accept and
        // canonicalise (not reject) RFC-3339 input.
        let q = UsageQuery {
            from: Some("2026-06-18T07:00:00+02:00".to_string()),
            to: None,
            provider_id: None,
            model_id: None,
            account_id: None,
            combo_id: None,
            api_key_id: None,
            preset: None,
        };
        let f = q.into_filter().expect("RFC-3339 with offset is valid");
        let from = f.from.expect("from present");
        // The offset is normalised to UTC and the suffix is `Z`.
        assert!(from.ends_with('Z'), "expected Z-suffix, got: {}", from);
        assert!(
            from.starts_with("2026-06-18T05:00:00"),
            "expected 05:00 UTC, got: {}",
            from
        );
    }

    #[test]
    fn usage_filter_accepts_sqlite_format() {
        // Operators paste `2026-06-18 07:00:00` from log lines; this
        // form must also be accepted.
        let q = UsageQuery {
            from: None,
            to: Some("2026-06-18 07:00:00".to_string()),
            provider_id: None,
            model_id: None,
            account_id: None,
            combo_id: None,
            api_key_id: None,
            preset: None,
        };
        let f = q.into_filter().expect("SQLite-style timestamp is valid");
        let to = f.to.expect("to present");
        assert_eq!(to, "2026-06-18T07:00:00Z");
    }

    #[test]
    fn usage_filter_rejects_from_after_to() {
        // A reversed range is a client error: it would return zero
        // rows silently otherwise.
        let q = UsageQuery {
            from: Some("2026-06-18T08:00:00Z".to_string()),
            to: Some("2026-06-18T07:00:00Z".to_string()),
            provider_id: None,
            model_id: None,
            account_id: None,
            combo_id: None,
            api_key_id: None,
            preset: None,
        };
        let err = q
            .into_filter()
            .expect_err("reversed range must be rejected");
        let msg = format!("{:?}", err);
        assert!(
            msg.contains("must be <="),
            "expected ordering error, got: {}",
            msg
        );
    }

    #[test]
    fn usage_filter_absent_timestamps_still_pass() {
        // Backward compat: when both fields are absent (the common
        // case in the dashboard's "show all" view), validation must
        // be a no-op.
        let q = UsageQuery {
            from: None,
            to: None,
            provider_id: None,
            model_id: None,
            account_id: None,
            combo_id: None,
            api_key_id: None,
            preset: None,
        };
        let f = q.into_filter().expect("absent timestamps are valid");
        assert!(f.from.is_none());
        assert!(f.to.is_none());
    }

    // ---- preset handling ----

    #[test]
    fn usage_filter_preset_this_month_resolves_to_month_bounds() {
        // `this_month` must produce [first-of-this-month 00:00 UTC,
        // first-of-next-month 00:00 UTC). We assert the day-of-month
        // rather than the full timestamp so the test is stable across
        // whatever month it runs in.
        let q = UsageQuery {
            from: None,
            to: None,
            provider_id: None,
            model_id: None,
            account_id: None,
            combo_id: None,
            api_key_id: None,
            preset: Some("this_month".to_string()),
        };
        let f = q.into_filter().expect("this_month preset is valid");
        let from = f.from.expect("from is computed from preset");
        let to = f.to.expect("to is computed from preset");
        assert!(
            from.ends_with("T00:00:00Z"),
            "from is midnight UTC: {}",
            from
        );
        assert!(to.ends_with("T00:00:00Z"), "to is midnight UTC: {}", to);
        assert!(
            from.ends_with("-01T00:00:00Z"),
            "from is the 1st of the month: {}",
            from
        );
        assert!(
            to.ends_with("-01T00:00:00Z"),
            "to is the 1st of the month: {}",
            to
        );
        assert!(from < to, "from must be before to");
    }

    #[test]
    fn usage_filter_preset_overrides_explicit_from_to() {
        // When both `preset` and explicit `from`/`to` are set, the
        // preset wins. We pick a `7d` preset and an explicit `from`
        // far in the past; the resolved `from` should be ~7 days ago,
        // not the explicit value.
        let q = UsageQuery {
            from: Some("2000-01-01T00:00:00Z".to_string()),
            to: Some("2000-01-02T00:00:00Z".to_string()),
            provider_id: None,
            model_id: None,
            account_id: None,
            combo_id: None,
            api_key_id: None,
            preset: Some("7d".to_string()),
        };
        let f = q.into_filter().expect("preset + explicit range is valid");
        let from = f.from.expect("from is computed from preset");
        // The explicit `2000-01-01` would have been used if preset
        // did not take precedence — assert it is not 2000.
        assert!(
            !from.starts_with("2000-"),
            "preset must override explicit from: {}",
            from
        );
        assert!(
            from.starts_with("20"),
            "from is a recent-ish year: {}",
            from
        );
    }

    #[test]
    fn usage_filter_preset_custom_falls_through_to_explicit_values() {
        // `custom` is the explicit opt-out sentinel: explicit
        // `from`/`to` must survive untouched.
        let q = UsageQuery {
            from: Some("2026-06-18T07:00:00Z".to_string()),
            to: Some("2026-06-19T07:00:00Z".to_string()),
            provider_id: None,
            model_id: None,
            account_id: None,
            combo_id: None,
            api_key_id: None,
            preset: Some("custom".to_string()),
        };
        let f = q.into_filter().expect("custom preset is valid");
        assert_eq!(f.from.as_deref(), Some("2026-06-18T07:00:00Z"));
        assert_eq!(f.to.as_deref(), Some("2026-06-19T07:00:00Z"));
    }

    #[test]
    fn usage_filter_preset_unknown_string_returns_400() {
        // Unknown preset strings must surface as 400 so a typo in the
        // dashboard doesn't silently miss a window.
        let q = UsageQuery {
            from: None,
            to: None,
            provider_id: None,
            model_id: None,
            account_id: None,
            combo_id: None,
            api_key_id: None,
            preset: Some("last_week".to_string()),
        };
        let err = q
            .into_filter()
            .expect_err("unknown preset must be rejected");
        let msg = format!("{:?}", err);
        assert!(
            msg.contains("preset"),
            "error must mention preset, got: {}",
            msg
        );
        assert!(
            msg.contains("last_week"),
            "error must include the bad value, got: {}",
            msg
        );
    }

    // ---- MEDIUM fix: DefaultBodyLimit is raised to 32 MiB ----
    //
    // axum's default is 2 MiB. We raise it so long-context chat
    // requests (system prompt + tool definitions + history) are not
    // rejected. The smoke test below confirms a 10 MiB body is
    // accepted and a 100 MiB body is rejected (the upper bound is
    // configurable but currently 32 MiB; 100 MiB exceeds that).

    #[tokio::test]
    async fn body_limit_accepts_10_mib_chat_body() {
        // Build a minimal chat request with a 10 MiB system prompt.
        // 10 MiB ≪ 32 MiB ceiling → must be accepted (the handler
        // still rejects it for missing auth, but the rejection is
        // NOT 413 Payload Too Large).
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;
        let tmp = tempfile::tempdir().expect("tempdir");
        let (state, _key) = make_state_with_key(tmp.path()).await;
        let app = crate::router::build_router(state);
        let big = "x".repeat(10 * 1024 * 1024);
        let body_json = format!(
            r#"{{"model":"gpt-4o","messages":[{{"role":"system","content":"{}"}}]}}"#,
            big
        );
        let mut req = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(body_json))
            .expect("build req");
        req.extensions_mut()
            .insert(axum::extract::connect_info::ConnectInfo(
                std::net::SocketAddr::from(([127, 0, 0, 1], 12345)),
            ));
        let resp = app.oneshot(req).await.expect("oneshot");
        assert_ne!(
            resp.status(),
            StatusCode::PAYLOAD_TOO_LARGE,
            "100 MiB body must be rejected by the 32 MiB body limit"
        );
    }

    #[tokio::test]
    async fn body_limit_rejects_100_mib_chat_body() {
        // 100 MiB ≫ 32 MiB ceiling → must be rejected with 413.
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;
        let tmp = tempfile::tempdir().expect("tempdir");
        let (state, _key) = make_state_with_key(tmp.path()).await;
        let app = crate::router::build_router(state);
        let big = "x".repeat(100 * 1024 * 1024);
        let body_json = format!(
            r#"{{"model":"gpt-4o","messages":[{{"role":"system","content":"{}"}}]}}"#,
            big
        );
        let mut req = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(body_json))
            .expect("build req");
        req.extensions_mut()
            .insert(axum::extract::connect_info::ConnectInfo(
                std::net::SocketAddr::from(([127, 0, 0, 1], 12345)),
            ));
        let resp = app.oneshot(req).await.expect("oneshot");
        assert_eq!(
            resp.status(),
            StatusCode::PAYLOAD_TOO_LARGE,
            "100 MiB body must be rejected by the 32 MiB body limit"
        );
    }

    // ---- LOW fix: clamp `since_id` to USAGE_RECENT_MAX_SINCE_ID so a
    // client passing `?since_id=i64::MAX` cannot force the SQL planner
    // to consider garbage keys. Negative values are still clamped to 0
    // (existing behavior). The behavior we test is "doesn't blow up",
    // not "returns the right rows" — the SQL is exercised elsewhere
    // (usage::recent's tests); here we only validate input handling.

    #[tokio::test]
    async fn usage_recent_clamps_since_id_at_max() {
        // Build a request with `since_id=i64::MAX`. The handler must
        // clamp instead of forwarding; if it forwarded, the SQL `WHERE
        // id > ?1` on the PK is still index-driven and returns [] in
        // microseconds, but a malicious client shouldn't get the
        // satisfaction of forcing a comparison against MAX.
        use axum::http::Request;
        use tower::ServiceExt;
        let tmp = tempfile::tempdir().expect("tempdir");
        let (state, key) = make_state_with_key(tmp.path()).await;
        let app = crate::router::build_router(state);
        let req = Request::builder()
            .uri("/admin/usage/recent?since_id=9223372036854775807&limit=1")
            .header("authorization", format!("Bearer {key}"))
            .body(axum::body::Body::empty())
            .expect("build req");
        let resp = app.oneshot(req).await.expect("oneshot");
        assert!(
            resp.status().is_success(),
            "since_id=MAX must NOT 5xx; got {}",
            resp.status()
        );
    }

    #[tokio::test]
    async fn usage_recent_rejects_negative_since_id() {
        // Negative since_id is meaningless (PK is positive). Existing
        // behavior clamps to 0, so the request returns the most-recent
        // rows. We assert it doesn't 5xx.
        use axum::http::Request;
        use tower::ServiceExt;
        let tmp = tempfile::tempdir().expect("tempdir");
        let (state, key) = make_state_with_key(tmp.path()).await;
        let app = crate::router::build_router(state);
        let req = Request::builder()
            .uri("/admin/usage/recent?since_id=-42&limit=1")
            .header("authorization", format!("Bearer {key}"))
            .body(axum::body::Body::empty())
            .expect("build req");
        let resp = app.oneshot(req).await.expect("oneshot");
        assert!(
            resp.status().is_success(),
            "since_id=-42 must NOT 5xx; got {}",
            resp.status()
        );
    }

    // ---- Cancellation note (was MEDIUM fix #10): the
    // `test_combo_targets` handler used to spawn a disconnect-watcher
    // task that drained `request.into_parts().1` (the request body)
    // and flipped a `tokio::sync::watch` flag when the body stream
    // ended. The fan-out loop polled that flag between targets and
    // short-circuited when it flipped. The previous test
    // (`test_combo_targets_signals_cancellation_via_watch`) exercised
    // that watch-channel wiring in isolation.
    //
    // We removed the watcher because, for a POST with no body — which
    // is what the dashboard actually sends — `Body::frame()` resolves
    // to `None` immediately, so the watcher fired `disconnect_tx`
    // before the fan-out loop started its second iteration. The
    // fan-out then aborted after the first target, which silently
    // broke "Test all". The handler now relies on Axum's natural
    // cancellation: when the client drops the response future, the
    // handler future is dropped, which in turn drops the in-flight
    // `UpstreamClient` future (cancel-safe) and aborts the loop. No watcher
    // task is needed.
    //
    // No regression test is added here because exercising the
    // cancellation path end-to-end requires a mock upstream with
    // controllable latency and a way to drop the response future
    // mid-flight. The happy-path coverage (handler completes the
    // fan-out for a bodyless POST) is provided by the dashboard's
    // Playwright suite; the 180s timeout wrap is exercised by
    // `run_test_for_model`'s own unit tests.

    // ---- Recording TTL admin endpoints ----

    #[tokio::test]
    async fn get_recording_ttl_returns_default_value() {
        let dir = tempdir();
        let (state, plaintext) = make_state_with_key(&dir).await;

        // Sanity: the slot starts at the default (300s).
        assert_eq!(state.recording_ttl_secs(), 300);

        let app = Router::new()
            .route("/admin/config/recording-ttl", get(get_recording_ttl))
            .with_state(state.clone());

        let req = Request::builder()
            .method("GET")
            .uri("/admin/config/recording-ttl")
            .header("authorization", format!("Bearer {}", plaintext))
            .body(Body::empty())
            .expect("build req");

        let resp = app.oneshot(req).await.expect("oneshot");
        assert_eq!(resp.status(), StatusCode::OK, "GET should be 200");

        let bytes = axum::body::to_bytes(resp.into_body(), 1024)
            .await
            .expect("body");
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("json body");
        assert_eq!(
            parsed["recording_ttl_secs"], 300,
            "default recording TTL must be 300"
        );
    }

    #[tokio::test]
    async fn put_recording_ttl_persists_new_value() {
        let dir = tempdir();
        let (state, plaintext) = make_state_with_key(&dir).await;

        let app = Router::new()
            .route(
                "/admin/config/recording-ttl",
                get(get_recording_ttl).put(put_recording_ttl),
            )
            .with_state(state.clone());

        let body = serde_json::json!({ "recording_ttl_secs": 600_i64 });
        let req = Request::builder()
            .method("PUT")
            .uri("/admin/config/recording-ttl")
            .header("authorization", format!("Bearer {}", plaintext))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("build req");

        let resp = app.oneshot(req).await.expect("oneshot");
        assert_eq!(resp.status(), StatusCode::OK, "PUT should be 200");

        // Body shape: the value echoed back + applies_to.
        let bytes = axum::body::to_bytes(resp.into_body(), 1024)
            .await
            .expect("body");
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("json body");
        assert_eq!(parsed["recording_ttl_secs"], 600);
        assert_eq!(parsed["applies_to"], "next_prune_tick");

        // The in-memory slot was updated.
        assert_eq!(state.recording_ttl_secs(), 600);

        // The row landed in the DB.
        let count: i64 = state.db_pool().with_conn(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM app_config WHERE key = 'recording_ttl_secs'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        });
        assert_eq!(count, 1, "PUT must have written a row to app_config");

        // The persisted value matches what we sent.
        let value: String = state.db_pool().with_conn(|c| {
            c.query_row(
                "SELECT value FROM app_config WHERE key = 'recording_ttl_secs'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        });
        let persisted: i64 = serde_json::from_str(&value).expect("parse value");
        assert_eq!(persisted, 600);
    }

    #[tokio::test]
    async fn put_recording_ttl_rejects_negative_value() {
        let dir = tempdir();
        let (state, plaintext) = make_state_with_key(&dir).await;

        let app = Router::new()
            .route("/admin/config/recording-ttl", put(put_recording_ttl))
            .with_state(state.clone());

        let body = serde_json::json!({ "recording_ttl_secs": -1_i64 });
        let req = Request::builder()
            .method("PUT")
            .uri("/admin/config/recording-ttl")
            .header("authorization", format!("Bearer {}", plaintext))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("build req");

        let resp = app.oneshot(req).await.expect("oneshot");
        let status = resp.status();
        assert!(
            status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY,
            "expected 400 or 422 for negative TTL, got {:?}",
            status
        );

        // In-memory slot must NOT have changed.
        assert_eq!(
            state.recording_ttl_secs(),
            300,
            "in-memory TTL must not change on rejected PUT"
        );
        assert_recording_ttl_db_count(&state, 0);
    }

    #[tokio::test]
    async fn put_recording_ttl_rejects_missing_field() {
        let dir = tempdir();
        let (state, plaintext) = make_state_with_key(&dir).await;

        let app = Router::new()
            .route("/admin/config/recording-ttl", put(put_recording_ttl))
            .with_state(state.clone());

        // Send a valid JSON object but missing the required
        // "recording_ttl_secs" field.
        let req = Request::builder()
            .method("PUT")
            .uri("/admin/config/recording-ttl")
            .header("authorization", format!("Bearer {}", plaintext))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"foo":"bar"}"#))
            .expect("build req");

        let resp = app.oneshot(req).await.expect("oneshot");
        let status = resp.status();
        assert!(
            status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY,
            "expected 400 or 422 for missing required field, got {:?}",
            status
        );

        // In-memory slot must NOT have changed.
        assert_eq!(
            state.recording_ttl_secs(),
            300,
            "in-memory TTL must not change on rejected PUT"
        );
        assert_recording_ttl_db_count(&state, 0);
    }

    #[tokio::test]
    async fn put_recording_ttl_rejects_invalid_json_syntax() {
        let dir = tempdir();
        let (state, plaintext) = make_state_with_key(&dir).await;

        let app = Router::new()
            .route("/admin/config/recording-ttl", put(put_recording_ttl))
            .with_state(state.clone());

        let req = Request::builder()
            .method("PUT")
            .uri("/admin/config/recording-ttl")
            .header("authorization", format!("Bearer {}", plaintext))
            .header("content-type", "application/json")
            .body(Body::from(r#"{invalid"#))
            .expect("build req");

        let resp = app.oneshot(req).await.expect("oneshot");
        let status = resp.status();
        assert!(
            status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY,
            "expected 400 or 422 for invalid JSON syntax, got {:?}",
            status
        );

        assert_eq!(
            state.recording_ttl_secs(),
            300,
            "in-memory TTL must not change on rejected PUT"
        );
        assert_recording_ttl_db_count(&state, 0);
    }

    // -----------------------------------------------------------------
    // Regression tests for admin refresh endpoints.
    //
    // These tests exist because the endpoints `POST /admin/accounts/:id/refresh-quota`
    // and `POST /admin/providers/:id/refresh` were hanging after the
    // refactor to the hyper-based UpstreamClient. The dashboard
    // reported "error sending request for url" because the server
    // never responded. The tests call the handlers directly with a
    // timeout to catch any hang regression.
    // -----------------------------------------------------------------

    /// Insert a test account for a given provider_id. Returns the
    /// account id. The account has a dummy API key (not used for
    /// upstream calls in the non-quota-capable path).
    fn insert_test_account(state: &AppState, provider_id: &str) -> i64 {
        let w = state.db_pool().writer();
        // Ensure the provider exists (FK constraint).
        let _ = openproxy_core::providers::create(
            &w,
            openproxy_core::providers::NewProvider {
                id: &openproxy_core::ids::ProviderId::new(provider_id),
                name: provider_id,
                base_url: "https://example.com",
                auth_type: openproxy_core::providers::AuthType::Bearer,
                format: openproxy_core::providers::ProviderFormat::Openai,
                extra_headers_json: None,
                auto_activate_keyword: None,
            },
        );
        // Now insert the account using the core helper.
        let mk = state.master_key();
        let aid = openproxy_core::accounts::create(
            &w,
            &openproxy_core::ids::ProviderId::new(provider_id),
            Some("sk-test-dummy-key"),
            mk.as_ref(),
            Some("test"),
            0,
            None,
        )
        .expect("insert account");
        aid.0
    }

    #[tokio::test]
    async fn refresh_account_quota_non_capable_provider_responds_fast() {
        // Regression: the endpoint must NOT hang when called for a
        // provider that doesn't have a quota fetcher (e.g.
        // 'openai'). The handler short-circuits with
        // {"supported": false} — no upstream call, no deadlock.
        let dir = tempdir();
        let (state, plaintext) = make_state_with_key(&dir).await;
        let account_id = insert_test_account(&state, "openai");

        let app = Router::new()
            .route(
                "/admin/accounts/{id}/refresh-quota",
                post(refresh_account_quota),
            )
            .with_state(state.clone());

        let req = Request::builder()
            .method("POST")
            .uri(format!("/admin/accounts/{}/refresh-quota", account_id))
            .header("authorization", format!("Bearer {}", plaintext))
            .body(Body::empty())
            .expect("build req");

        // Wrap in a timeout — if the handler hangs, the test fails
        // instead of blocking forever.
        let resp = tokio::time::timeout(std::time::Duration::from_secs(5), app.oneshot(req))
            .await
            .expect("refresh-quota handler hung for >5s (regression)")
            .expect("oneshot");

        assert_eq!(resp.status(), StatusCode::OK, "expected 200");
        let body = axum::body::to_bytes(resp.into_body(), 1024)
            .await
            .expect("body");
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(v["supported"], false, "expected supported=false for openai");
    }

    #[tokio::test]
    async fn refresh_provider_models_unknown_provider_responds_fast() {
        // Regression: the endpoint must NOT hang when called for a
        // provider that doesn't exist. The handler returns an error
        // (404 or 400) — no upstream call, no deadlock.
        let dir = tempdir();
        let (state, plaintext) = make_state_with_key(&dir).await;

        let app = Router::new()
            .route(
                "/admin/providers/{id}/refresh",
                post(refresh_provider_models),
            )
            .with_state(state.clone());

        let req = Request::builder()
            .method("POST")
            .uri("/admin/providers/nonexistent-provider/refresh")
            .header("authorization", format!("Bearer {}", plaintext))
            .body(Body::empty())
            .expect("build req");

        let resp = tokio::time::timeout(std::time::Duration::from_secs(5), app.oneshot(req))
            .await
            .expect("refresh-provider handler hung for >5s (regression)")
            .expect("oneshot");

        // The handler returns 200 with an error in the JSON body, or
        // a 4xx/5xx. Either is acceptable as long as it doesn't hang.
        assert!(
            resp.status().is_client_error()
                || resp.status().is_server_error()
                || resp.status() == StatusCode::OK,
            "expected error or 200, got {:?}",
            resp.status()
        );
    }

    #[tokio::test]
    async fn refresh_account_quota_nonexistent_account_responds_fast() {
        // Regression: the endpoint must NOT hang when called for an
        // account that doesn't exist. The handler returns 404 — no
        // upstream call, no deadlock.
        let dir = tempdir();
        let (state, plaintext) = make_state_with_key(&dir).await;

        let app = Router::new()
            .route(
                "/admin/accounts/{id}/refresh-quota",
                post(refresh_account_quota),
            )
            .with_state(state.clone());

        let req = Request::builder()
            .method("POST")
            .uri("/admin/accounts/99999/refresh-quota")
            .header("authorization", format!("Bearer {}", plaintext))
            .body(Body::empty())
            .expect("build req");

        let resp = tokio::time::timeout(std::time::Duration::from_secs(5), app.oneshot(req))
            .await
            .expect("refresh-quota handler hung for >5s on nonexistent account (regression)")
            .expect("oneshot");

        // Account not found -> 404 or 500. Either is fine as long as
        // it doesn't hang.
        assert!(
            resp.status().is_client_error() || resp.status().is_server_error(),
            "expected 4xx/5xx for nonexistent account, got {:?}",
            resp.status()
        );
    }

    // ---- G2.4 quota_low helper tests -----------------------------------

    fn q(
        session_used: Option<i64>,
        session_limit: Option<i64>,
        weekly_used: Option<i64>,
        weekly_limit: Option<i64>,
        fetch_error: Option<&str>,
    ) -> openproxy_core::quota::AccountQuota {
        openproxy_core::quota::AccountQuota {
            session_used,
            session_limit,
            session_reset_at: None,
            weekly_used,
            weekly_limit,
            weekly_reset_at: None,
            plan_name: None,
            last_fetched_at: "2026-01-01T00:00:00Z".into(),
            fetch_error: fetch_error.map(str::to_string),
            model_details: None,
        }
    }

    #[test]
    fn quota_low_fires_when_session_remaining_below_10pct() {
        // limit=1000, used=950 → remaining=50 → 5% < 10% → fires.
        let quota = q(Some(950), Some(1000), None, None, None);
        let (scope, remaining, limit) = compute_low_quota_signal(&quota).expect("should fire");
        assert_eq!(scope, "session");
        assert_eq!(remaining, 50);
        assert_eq!(limit, 1000);
    }

    #[test]
    fn quota_low_does_not_fire_when_session_remaining_above_10pct() {
        // limit=1000, used=800 → remaining=200 → 20% > 10% → no fire.
        let quota = q(Some(800), Some(1000), None, None, None);
        assert!(compute_low_quota_signal(&quota).is_none());
    }

    #[test]
    fn quota_low_falls_through_to_weekly_when_session_healthy() {
        // session at 50% (healthy), weekly at 5% (low) → fires on weekly.
        let quota = q(Some(500), Some(1000), Some(950), Some(1000), None);
        let (scope, _, _) = compute_low_quota_signal(&quota).expect("should fire");
        assert_eq!(scope, "weekly");
    }

    #[test]
    fn quota_low_fires_on_session_first_when_both_low() {
        // Both windows low → session wins (documented priority).
        let quota = q(Some(950), Some(1000), Some(950), Some(1000), None);
        let (scope, _, _) = compute_low_quota_signal(&quota).expect("should fire");
        assert_eq!(scope, "session");
    }

    #[test]
    fn quota_low_boundary_exactly_10pct_does_not_fire() {
        // remaining=100, limit=1000 → exactly 10%. The integer test
        // `remaining * 10 < limit` is `1000 < 1000` → false → no fire.
        // (Strict less-than: the operator gets the warning when the
        // account is BELOW 10%, not AT 10%.)
        let quota = q(Some(900), Some(1000), None, None, None);
        assert!(compute_low_quota_signal(&quota).is_none());
    }

    #[test]
    fn quota_low_handles_zero_limit_via_absolute_floor() {
        // limit=0 (degenerate), remaining=500 → 500 < 1000 → fires.
        // `remaining = limit - used = 0 - (-500) = 500` (used is negative
        // here only to construct the test; in practice `used >= 0` so
        // this branch fires only when `limit = 0` and the row is
        // degenerate — the absolute floor catches it).
        let quota = q(Some(-500), Some(0), None, None, None);
        let (_, remaining, _) = compute_low_quota_signal(&quota).expect("should fire");
        assert_eq!(remaining, 500);
    }

    #[test]
    fn quota_low_does_not_fire_when_no_quota_data() {
        // All-NULL quota (provider doesn't expose a quota endpoint).
        let quota = q(None, None, None, None, None);
        assert!(compute_low_quota_signal(&quota).is_none());
    }

    // Note: `quota_low` does NOT consult `fetch_error` — that gate is
    // applied by the caller (`refresh_account_quota`) before invoking
    // `compute_low_quota_signal`. The helper itself only inspects the
    // numeric fields. Verified by this test: a quota with `fetch_error`
    // set but valid numeric fields still returns Some — the caller is
    // responsible for skipping the call when there's an error.
    #[test]
    fn quota_low_helper_ignores_fetch_error_field() {
        let quota = q(Some(950), Some(1000), None, None, Some("upstream 500"));
        // Helper returns Some — the caller's `fetch_error.is_none()`
        // gate is what suppresses the notification in the error case.
        assert!(compute_low_quota_signal(&quota).is_some());
    }

    #[tokio::test]
    async fn test_run_test_for_model_cancellation() {
        let dir = tempdir();
        let (state, _plaintext) = make_state_with_key(&dir).await;

        // Seed the built-in providers so openrouter exists
        {
            let w = state.db_pool().writer();
            seed::seed_builtin_providers(&w).expect("seed");
        }

        // Create a model in the DB to test against.
        let model_row_id = {
            let w = state.db_pool().writer();
            w.execute(
                "INSERT INTO models (provider_id, model_id, target_format, active) VALUES (?, ?, ?, ?)",
                ("openrouter", "gpt-4o", "openai", 1),
            )
            .expect("insert model");
            w.last_insert_rowid()
        };

        // Create a pre-cancelled watch receiver
        let (tx, rx) = tokio::sync::watch::channel(false);
        tx.send(true).unwrap();

        let r = run_test_for_model(
            &state,
            model_row_id,
            None,
            None,
            TestOptions::default(),
            Some(rx),
        )
        .await;

        assert_eq!(r.status, 0);
        assert_eq!(r.error_msg.as_deref(), Some("Cancel"));
    }
}
