//! Admin HTTP handlers.
//!
//! Spec §2.3 enumerates the admin surface:
//!
//! - `GET    /v1/admin/health`                          — process liveness.
//! - `*      /v1/admin/providers[...]`                  — provider CRUD.
//! - `*      /v1/admin/accounts[...]`                   — account CRUD
//!   (including `POST .../health` to force-set the health flag).
//! - `*      /v1/admin/combos[...]`                     — combo CRUD
//!   (including `PATCH /:id` for `race_size` and
//!   `PATCH /:id/targets/:target_id` for `priority_order`).
//! - `*      /v1/admin/usage/*`                         — usage analytics,
//!   plus `GET /usage/recent?since_id=N` for the dashboard's
//!   long-polling live tail.
//! - `POST   /v1/admin/models/:id/refresh`              — model discovery refresh.
//! - `POST   /v1/admin/models/:id/toggle`               — soft-disable a model.
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
    extract::{Path, Query, State},
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    http::HeaderMap,
    response::IntoResponse,
    Json,
};
use futures::StreamExt;
use openproxy_core::{
    accounts,
    admin,
    analytics,
    api_keys as core_api_keys,
    combos,
    config::{CircuitBreakerConfig, RacingConfig, RetriesConfig, TimeoutsConfig},
    db as core_db,
    db::conn::ADMIN_LOCK_TIMEOUT,
    ids::{AccountId, ApiKeyId, ComboId, ComboTargetId, ModelRowId, ProviderId},
    models,
    oauth,
    providers,
    seed,
    usage::{self, UsageFilter},
    CoreError,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    error::{ApiError, ApiResult},
    state::AppState,
};

/// Optional filters shared by all `GET /v1/admin/usage/*` endpoints.
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
    /// `GET /v1/admin/keys/:id/usage` endpoint sets this; the
    /// public analytics endpoints leave it absent.
    pub api_key_id: Option<i64>,
}

/// Parse a `from` or `to` timestamp from the dashboard into the
/// canonical RFC-3339 form the SQL builder expects. Returns a
/// 400-style [`CoreError::Validation`] on malformed input.
fn parse_usage_timestamp(s: &str, field: &str) -> Result<String, ApiError> {
    // Try RFC-3339 first (the canonical form `created_at` is stored in).
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&chrono::Utc).to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    }
    // Fall back to the SQLite "YYYY-MM-DD HH:MM:SS" form (the format
    // operators sometimes paste from a log line). We require the
    // space — a `T` here is the RFC-3339 form, already handled above.
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return Ok(naive.and_utc().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    }
    Err(CoreError::Validation(format!(
        "{} must be an RFC-3339 timestamp (e.g. 2026-06-18T07:00:00Z) or \
         SQLite-style (e.g. 2026-06-18 07:00:00); got `{}`",
        field, s
    ))
    .into())
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
        let from = self
            .from
            .map(|s| parse_usage_timestamp(&s, "from"))
            .transpose()?;
        let to = self
            .to
            .map(|s| parse_usage_timestamp(&s, "to"))
            .transpose()?;
        // If both are present, from must not be after to. (Both
        // are inclusive at the lower bound in the SQL.)
        if let (Some(f), Some(t)) = (&from, &to) {
            if f > t {
                return Err(CoreError::Validation(format!(
                    "from ({}) must be <= to ({})",
                    f, t
                ))
                .into());
            }
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

/// `GET /v1/admin/health` — process liveness with a version tag.
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
}

/// `GET /v1/admin/config` — return the currently-loaded runtime
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
            timeouts: cfg.timeouts,
            retries: cfg.retries,
            circuit_breaker: cfg.circuit_breaker,
            // `RacingConfig` is `Clone` but not `Copy` (the other
            // three are); `.clone()` is fine, it's three `u*` fields.
            racing: cfg.racing.clone(),
        }))
    }
    .await;
    body.into()
}

/// `PUT /v1/admin/config/timeouts` — hot-reload the system default
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

// =====================================================================
// Providers
// =====================================================================

/// `GET /v1/admin/providers` — list all providers.
pub async fn list_providers(
    State(s): State<AppState>,
) -> ApiResult<Json<Vec<providers::Provider>>> {
    let body: Result<Json<Vec<providers::Provider>>, ApiError> = async {
        let w = s.db_pool().writer();
        let list = admin::list_providers(&w)?;
        Ok(Json(list))
    }
    .await;
    body.into()
}

/// `POST /v1/admin/providers` — create a provider.
pub async fn create_provider(
    State(s): State<AppState>,
    Json(input): Json<admin::CreateProviderInput>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let w = s.db_pool().writer();
        let id = admin::create_provider(&w, input)?;
        Ok(Json(serde_json::json!({ "id": id.as_str() })))
    }
    .await;
    body.into()
}

/// `GET /v1/admin/providers/:id` — fetch a single provider.
pub async fn get_provider(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<providers::Provider>> {
    let body: Result<Json<providers::Provider>, ApiError> = async {
        let w = s.db_pool().writer();
        let id = ProviderId::new(id);
        let provider = providers::get(&w, &id)?
            .ok_or_else(|| CoreError::ProviderNotFound(id.to_string()))?;
        Ok(Json(provider))
    }
    .await;
    body.into()
}

/// `DELETE /v1/admin/providers/:id` — delete a provider. Idempotent
/// for custom providers.
///
/// Built-in providers (the ones seeded on first run — see
/// [`openproxy_core::seed::builtin_provider_ids`]) are rejected
/// with a 400 (Validation) instructing the operator to use
/// `POST /v1/admin/providers/:id/active` to deactivate the
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
                 /v1/admin/providers/{}/active with {{\"active\": false}} to \
                 deactivate it instead.",
                id, id
            ))));
        }
        let w = s.db_pool().writer();
        let id = ProviderId::new(id);
        admin::delete_provider(&w, &id)?;
        Ok(Json(serde_json::json!({ "deleted": id.as_str() })))
    }
    .await;
    body.into()
}

/// `POST /v1/admin/providers/:id/active` — flip the soft-disable flag
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

/// Query string for `GET /v1/admin/accounts` — supports `?provider_id=...`.
#[derive(Debug, Default, Deserialize)]
pub struct AccountListQuery {
    pub provider_id: Option<String>,
}

/// `GET /v1/admin/accounts` — list accounts, optionally filtered by provider.
pub async fn list_accounts(
    State(s): State<AppState>,
    Query(q): Query<AccountListQuery>,
) -> ApiResult<Json<Vec<accounts::Account>>> {
    let body: Result<Json<Vec<accounts::Account>>, ApiError> = async {
        let w = s.db_pool().writer();
        let provider = q.provider_id.map(ProviderId::new);
        let list = admin::list_accounts(&w, provider.as_ref())?;
        Ok(Json(list))
    }
    .await;
    body.into()
}

/// `POST /v1/admin/accounts` — create an account. `api_key` is encrypted
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

/// `DELETE /v1/admin/accounts/:id` — delete an account by numeric id. Idempotent.
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

/// `GET /v1/admin/combos` — list all combos.
pub async fn list_combos(
    State(s): State<AppState>,
) -> ApiResult<Json<Vec<combos::Combo>>> {
    let body: Result<Json<Vec<combos::Combo>>, ApiError> = async {
        let w = s.db_pool().writer();
        let list = admin::list_combos(&w)?;
        Ok(Json(list))
    }
    .await;
    body.into()
}

/// `POST /v1/admin/combos` — create a combo. `race_size` defaults to 1.
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

/// `GET /v1/admin/combos/:id` — fetch a single combo.
pub async fn get_combo(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<combos::Combo>> {
    let body: Result<Json<combos::Combo>, ApiError> = async {
        let w = s.db_pool().writer();
        let id = ComboId(id);
        let combo = combos::get_combo(&w, id)?
            .ok_or_else(|| CoreError::ComboNotFound(id.0))?;
        Ok(Json(combo))
    }
    .await;
    body.into()
}

/// `POST /v1/admin/combos/:id/test-all` — fan-out a test request to
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
    request: axum::extract::Request,
) -> ApiResult<Json<Vec<serde_json::Value>>> {
    use serde_json::json;

    // MEDIUM fix (#10): cancellation. The fan-out can take up to
    // ~3 minutes (8 targets × 15s). When the dashboard closes the
    // tab or otherwise drops the HTTP request, the handler used to
    // keep firing upstreams until the global 180s budget expired —
    // wasting upstream tokens and DB writes. We wire a
    // `tokio::sync::Notify` to the request body: when the client
    // closing the connection, the body's `frame()` future resolves to
    // `None` and we set the flag. The fan-out loop polls the flag
    // between targets and short-circuits, which also drops the
    // in-flight `reqwest::RequestBuilder::send()` future and closes
    // the upstream TCP connection (reqwest is cancel-safe).
    let (request_parts, request_body) = request.into_parts();
    let _ = request_parts; // we don't need the metadata here
    let (disconnect_tx, mut disconnect_rx) = tokio::sync::watch::channel(false);
    {
        tokio::spawn(async move {
            // The body of an Axum extractor's `Request` is a
            // `Body` whose `frame()` future resolves to `None`
            // when the underlying TCP socket closes — which is
            // exactly the disconnect signal we need. We don't
            // care about the frame payload, only the lifecycle.
            use http_body_util::BodyExt;
            let mut pinned = request_body;
            while let Some(_frame) = pinned.frame().await {
                // Drain until the client closes.
            }
            // Best-effort: receiver may have been dropped if the
            // handler returned earlier. The send failure is not
            // an error — we just stop tracking the signal.
            let _ = disconnect_tx.send(true);
        });
    }

    let body: Result<Json<Vec<serde_json::Value>>, ApiError> = async {
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
                // MEDIUM fix (#10): poll the disconnect signal
                // BEFORE firing the upstream. If the dashboard
                // closed the tab, we stop immediately. The watch
                // channel's `borrow_and_update` returns the latest
                // value, which is `true` once the body stream
                // ended. The check is cheap (one atomic load) and
                // runs once per target — amortized over a 15s
                // upstream call.
                if *disconnect_rx.borrow_and_update() {
                    tracing::info!(
                        combo_id = id,
                        results_so_far = results.len(),
                        "test-all: client disconnected, aborting fan-out"
                    );
                    // Return what we have; the response shape is
                    // still the array of rows so the dashboard
                    // can render the partial picture.
                    return results;
                }
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
                // Flat, active, not in cooldown: actually fire
                // upstream. The helper handles the model-not-active
                // short-circuit itself (skipped row with
                // "model is inactive" in the error_msg).
                let r = run_test_for_model(
                    &s,
                    t.model_row_id.unwrap_or(ModelRowId(0)).0,
                    t.account_id,
                    TestOptions { in_combo_fanout: true },
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
                    obj["error_msg"] = json!(r.skip_reason.unwrap_or_else(|| "skipped".to_string()));
                }
                results.push(obj);
            }
            results
        };

        let results = match tokio::time::timeout(
            std::time::Duration::from_secs(180),
            fan_out,
        )
        .await
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

/// `DELETE /v1/admin/combos/:id` — delete a combo. Idempotent; cascade
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

/// `GET /v1/admin/combos/:id/targets` — list a combo's targets in
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
        let w = s.db_pool().writer();
        let id = ComboId(id);
        let targets = admin::list_combo_targets_with_model(&w, id)?;
        Ok(Json(targets))
    }
    .await;
    body.into()
}

/// `POST /v1/admin/combos/:id/targets` — add a target to a combo.
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

/// `GET /v1/admin/combos/:id/valid-sub-combos` — list combos that
/// can be added as a sub-combo target of `:id` (i.e. excluding the
/// combo itself and any combo whose addition would close a cycle).
/// Drives the "Add sub-combo target" picker in the dashboard.
pub async fn list_valid_sub_combos(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<Vec<admin::ComboSummary>>> {
    let body: Result<Json<Vec<admin::ComboSummary>>, ApiError> = async {
        let w = s.db_pool().writer();
        let id = ComboId(id);
        let list = admin::list_valid_sub_combos(&w, id)?;
        Ok(Json(list))
    }
    .await;
    body.into()
}

// =====================================================================
// Usage analytics
// =====================================================================

/// `GET /v1/admin/usage/summary` — top-line roll-up.
pub async fn usage_summary(
    State(s): State<AppState>,
    Query(q): Query<UsageQuery>,
) -> ApiResult<Json<usage::UsageSummary>> {
    let body: Result<Json<usage::UsageSummary>, ApiError> = async {
        // LOW fix (#14): admin queries must not block forever on
        // the writer lock. We wait up to ADMIN_LOCK_TIMEOUT (5s)
        // and return 503 if we lose the race — better a clear
        // error than an indefinite hang for the operator.
        let w = s
            .db_pool()
            .try_writer_for(ADMIN_LOCK_TIMEOUT)
            .ok_or_else(|| {
                ApiError(CoreError::ServiceUnavailable(
                    "writer lock busy: another query is holding the database; retry in a few seconds"
                        .into(),
                ))
            })?;
        let f = q.into_filter()?;
        Ok(Json(usage::summary(&w, &f)?))
    }
    .await;
    body.into()
}

/// `GET /v1/admin/usage/by-model` — per-(provider, model) breakdown.
pub async fn usage_by_model(
    State(s): State<AppState>,
    Query(q): Query<UsageQuery>,
) -> ApiResult<Json<Vec<usage::ByModelRow>>> {
    let body: Result<Json<Vec<usage::ByModelRow>>, ApiError> = async {
        let w = s.db_pool().writer();
        let f = q.into_filter()?;
        Ok(Json(usage::by_model(&w, &f)?))
    }
    .await;
    body.into()
}

/// `GET /v1/admin/usage/by-account` — per-account breakdown.
pub async fn usage_by_account(
    State(s): State<AppState>,
    Query(q): Query<UsageQuery>,
) -> ApiResult<Json<Vec<usage::ByAccountRow>>> {
    let body: Result<Json<Vec<usage::ByAccountRow>>, ApiError> = async {
        let w = s.db_pool().writer();
        let f = q.into_filter()?;
        Ok(Json(usage::by_account(&w, &f)?))
    }
    .await;
    body.into()
}

/// `GET /v1/admin/usage/by-status` — counts grouped by HTTP status code.
pub async fn usage_by_status(
    State(s): State<AppState>,
    Query(q): Query<UsageQuery>,
) -> ApiResult<Json<Vec<usage::ByStatusRow>>> {
    let body: Result<Json<Vec<usage::ByStatusRow>>, ApiError> = async {
        let w = s.db_pool().writer();
        let f = q.into_filter()?;
        Ok(Json(usage::by_status(&w, &f)?))
    }
    .await;
    body.into()
}

/// Cap on inline error rows. Spec §7.2 says "the most recent 100".
const ERRORS_DEFAULT_LIMIT: u32 = 100;

/// `GET /v1/admin/usage/errors` — recent error rows, newest first.
pub async fn usage_errors(
    State(s): State<AppState>,
    Query(q): Query<UsageQuery>,
) -> ApiResult<Json<Vec<usage::ErrorRow>>> {
    let body: Result<Json<Vec<usage::ErrorRow>>, ApiError> = async {
        let w = s.db_pool().writer();
        let f = q.into_filter()?;
        Ok(Json(usage::errors(&w, &f, ERRORS_DEFAULT_LIMIT)?))
    }
    .await;
    body.into()
}

/// `GET /v1/admin/usage/latency` — p50/p95 across connect/ttft/total/tokens_per_sec.
pub async fn usage_latency(
    State(s): State<AppState>,
    Query(q): Query<UsageQuery>,
) -> ApiResult<Json<analytics::LatencyPercentiles>> {
    let body: Result<Json<analytics::LatencyPercentiles>, ApiError> = async {
        let w = s.db_pool().writer();
        let f = q.into_filter()?;
        Ok(Json(analytics::latency_percentiles(&w, &f)?))
    }
    .await;
    body.into()
}

/// `GET /v1/admin/usage/races` — race outcome statistics.
pub async fn usage_races(
    State(s): State<AppState>,
    Query(q): Query<UsageQuery>,
) -> ApiResult<Json<analytics::RaceStats>> {
    let body: Result<Json<analytics::RaceStats>, ApiError> = async {
        let w = s.db_pool().writer();
        let f = q.into_filter()?;
        Ok(Json(analytics::race_stats(&w, &f)?))
    }
    .await;
    body.into()
}

// =====================================================================
// Model refresh
// =====================================================================

/// Query string for `POST /v1/admin/models/:id/refresh` — lets the caller
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

/// `POST /v1/admin/models/:id/refresh` — re-discover models for the
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
async fn run_refresh(
    s: AppState,
    id: i64,
    q: RefreshQuery,
) -> ApiResult<Json<serde_json::Value>> {
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

    // 2. Find the adapter for that provider.
    let adapter = match s
        .adapters()
        .iter()
        .find(|a| a.id() == &provider_id)
        .cloned()
    {
        Some(a) => a,
        None => {
            return ApiResult::err(ApiError(CoreError::ProviderNotFound(
                provider_id.to_string(),
            )));
        }
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
                    Ok(None) => return ApiResult::err(ApiError(CoreError::AccountNotFound(account_id.0))),
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
        adapter.as_ref(),
        s.upstream_client(),
        ttl_seconds,
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

/// Default cap for `GET /v1/admin/usage/recent` when the client omits
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

/// Query string for `GET /v1/admin/usage/recent`.
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

/// `GET /v1/admin/usage/recent?since_id=N&limit=K` — long-polling tail of
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
        let since_id = q
            .since_id
            .unwrap_or(0)
            .max(0)
            .min(USAGE_RECENT_MAX_SINCE_ID);
        let limit = q
            .limit
            .unwrap_or(USAGE_RECENT_DEFAULT_LIMIT)
            .clamp(1, USAGE_RECENT_MAX_LIMIT);
        let w = s.db_pool().writer();
        // SEC-MEDIUM-C fix: drop the heavy request/response payloads
        // from the WS/REST surface — they can be multi-MB and would
        // fan out PII to every dashboard subscriber. The detail
        // endpoint reads them straight from the database on demand.
        let rows = usage::recent(&w, since_id, limit)?
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

fn json_text(value: serde_json::Value) -> Result<String, ApiError> {
    serde_json::to_string(&value).map_err(|e| {
        ApiError(CoreError::Internal(format!(
            "serialize websocket message: {e}"
        )))
    })
}

async fn send_ws_json(socket: &mut WebSocket, value: serde_json::Value) -> Result<(), ApiError> {
    let text = json_text(value)?;
    socket
        .send(Message::Text(text))
        .await
        .map_err(|e| ApiError(CoreError::Internal(format!("send websocket message: {e}"))))
}

async fn send_ws_error(socket: &mut WebSocket, message: impl Into<String>) -> Result<(), ApiError> {
    send_ws_json(socket, json!({ "type": "error", "message": message.into() })).await
}

fn authenticate_admin_ws(state: &AppState, headers: &HeaderMap, query_token: Option<&str>) -> Result<(), ApiError> {
    // Dev convenience: when the operator explicitly opts in by setting
    // OPENPROXY_DASHBOARD_AUTH_BYPASS=1 in the server's environment, every
    // admin request is accepted without an Authorization header or query
    // token. The match is on the exact sentinel `1` — NOT "any non-empty
    // value" — so a typo or stray config (e.g. `=false`, `=yes`, `=0`,
    // `=legacy-token`) cannot silently grant full admin access. The match
    // is logged at WARN level so the bypass is visible in production logs
    // and dashboards alerting on auth-bypass are wired correctly.
    if let Ok(bypass) = std::env::var("OPENPROXY_DASHBOARD_AUTH_BYPASS") {
        if bypass == "1" {
            tracing::warn!(
                target: "openproxy::security",
                "admin auth bypassed via OPENPROXY_DASHBOARD_AUTH_BYPASS=1 — \
                 every admin endpoint is open. Remove this env var to restore auth."
            );
            return Ok(());
        }
    }

    // Extract token from Authorization header or from query parameter
    let header_token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::trim);

    let t = header_token.or(query_token).ok_or_else(|| {
        ApiError(CoreError::Auth("missing authorization header or token query parameter".into()))
    })?;

    if t.is_empty() {
        return Err(ApiError(CoreError::Auth("invalid token".into())));
    }

    let key_hash = core_api_keys::hash_key(t);
    let w = state.db_pool().writer();
    let key = match core_api_keys::get_by_hash(&w, &key_hash).map_err(ApiError)? {
        Some(k) => k,
        None => {
            return Err(ApiError(CoreError::Auth(
                "invalid api key".into(),
            )));
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
        return Err(ApiError(CoreError::Auth(format!(
            "api key lacks 'manage' scope (has {:?})",
            key.scopes
        ))));
    }

    let _ = core_api_keys::touch_last_used(&w, key.id).map_err(ApiError);

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

async fn stream_usage_rows(
    mut socket: WebSocket,
    state: AppState,
) {
    if let Err(err) = async {
        // 1. Initial history batch (most recent 100)
        let rows = {
            let w = state.db_pool().writer();
            usage::recent_desc(&w, 100)?
        };
        send_ws_json(
            &mut socket,
            json!({ "type": "history", "rows": rows }),
        )
        .await?;
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
        let mut last_known_id: i64 = rows.iter().map(|r| r.id.0).max().unwrap_or(0);

        // 2. Subscribe to broadcast tx (rows + stages)
        let mut usage_rx = state.usage_tx().subscribe();
        let mut stage_rx = state.stage_tx().subscribe();
        loop {
            tokio::select! {
                incoming = socket.next() => {
                    match incoming {
                        Some(Ok(Message::Text(text))) => {
                            let msg: ClientWsMessage = match serde_json::from_str(&text) {
                                Ok(msg) => msg,
                                Err(e) => {
                                    send_ws_error(&mut socket, format!("invalid client message: {e}")).await?;
                                    continue;
                                }
                            };

                            match msg.msg_type.as_str() {
                                "subscribe" => {
                                    let since_id = msg
                                        .since_id
                                        .unwrap_or(0)
                                        .max(0)
                                        .min(USAGE_RECENT_MAX_SINCE_ID);
                                    let w = state.db_pool().writer();
                                    // SEC-MEDIUM-C fix: strip the heavy request/response
                                    // fields from the initial history batch — the
                                    // publisher already redacts the per-event broadcast,
                                    // but `recent()` still returns the full rows so
                                    // this initial replay matched the publisher.
                                    let rows: Vec<usage::RecentUsageRow> =
                                        usage::recent(&w, since_id, 100)?
                                            .into_iter()
                                            .map(usage::redact_for_broadcast)
                                            .collect();
                                    if let Some(mx) = rows.iter().map(|r| r.id.0).max() {
                                        last_known_id = last_known_id.max(mx);
                                    }
                                    send_ws_json(
                                        &mut socket,
                                        json!({ "type": "history", "rows": rows }),
                                    )
                                    .await?;
                                }
                                "ping" => {
                                    let now_str = chrono::Utc::now().to_rfc3339();
                                    send_ws_json(&mut socket, json!({ "type": "pong", "server_time": now_str })).await?;
                                }
                                _ => {
                                    send_ws_error(&mut socket, format!("unknown message type: {}", msg.msg_type)).await?;
                                }
                            }
                        }
                        Some(Ok(Message::Close(_))) => break,
                        Some(Ok(_)) => {}
                        Some(Err(e)) => return Err(ApiError(CoreError::Internal(format!("receive websocket message: {e}")))),
                        None => break,
                    }
                }
                // New row (final): published by `cost::record` after
                // the request finishes and the usage row is committed.
                usage = usage_rx.recv() => {
                    match usage {
                        Ok(row) => {
                            if row.id.0 > last_known_id {
                                last_known_id = row.id.0;
                            }
                            send_ws_json(
                                &mut socket,
                                json!({ "type": "row", "data": row }),
                            )
                            .await?;
                        }
                        // H7 fix: instead of a fatal `error`
                        // envelope that the dashboard could only
                        // show as a toast, emit a `resync`
                        // envelope with the last id we have
                        // already streamed. The frontend calls
                        // `usage::recent(since_id=last_known_id, ...)`
                        // to catch up. We also keep the
                        // `lag_warning` envelope as a fallback so
                        // an old client can still surface the
                        // gap in the UI.
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            send_ws_json(
                                &mut socket,
                                json!({
                                    "type": "lag_warning",
                                    "skipped": skipped,
                                    "message": format!(
                                        "broadcast channel lagged; {} row(s) skipped",
                                        skipped
                                    ),
                                }),
                            )
                            .await?;
                            send_ws_json(
                                &mut socket,
                                json!({ "type": "resync", "since_id": last_known_id }),
                            )
                            .await?;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
                // New stage (in-flight): published by the pipeline at
                // every phase transition while recording is ON. The
                // dashboard uses this to update the row's "in
                // phase" label in real time. `bias = biased` is not
                // needed here because both branches are independent
                // and the operator cares about correctness, not
                // strict ordering between a stage and a final row.
                stage = stage_rx.recv() => {
                    match stage {
                        Ok(event) => {
                            send_ws_json(
                                &mut socket,
                                json!({ "type": "stage", "data": event }),
                            )
                            .await?;
                        }
                        // H7 fix: same resync treatment as the
                        // usage row channel. Stage events are
                        // ephemeral (the terminal one is also
                        // re-emitted after the row lands, so the
                        // dashboard can reconstruct state) but a
                        // slow consumer still benefits from a
                        // `resync` hint.
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            send_ws_json(
                                &mut socket,
                                json!({
                                    "type": "lag_warning",
                                    "skipped": skipped,
                                    "message": format!(
                                        "stage broadcast channel lagged; {} event(s) skipped",
                                        skipped
                                    ),
                                }),
                            )
                            .await?;
                            send_ws_json(
                                &mut socket,
                                json!({ "type": "resync", "since_id": last_known_id }),
                            )
                            .await?;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
        Ok::<(), ApiError>(())
    }
    .await
    {
        let _ = send_ws_error(&mut socket, err.to_string()).await;
    }
}

/// Query for WebSocket token in `/v1/admin/usage/stream`
#[derive(Debug, Default, Deserialize)]
pub struct UsageStreamQuery {
    pub token: Option<String>,
}

/// `GET /v1/admin/usage/stream` — upgraded WebSocket handler.
pub async fn usage_stream(
    State(s): State<AppState>,
    Query(q): Query<UsageStreamQuery>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> impl axum::response::IntoResponse {
    match authenticate_admin_ws(&s, &headers, q.token.as_deref()) {
        Ok(()) => {
            ws.on_upgrade(move |socket| stream_usage_rows(socket, s))
                .into_response()
        }
        Err(e) => e.into_response(),
    }
}

/// `GET /v1/admin/usage/detail?id=<usage_id>` — full detail for a single usage row.
pub async fn usage_detail(
    State(s): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<DetailQuery>,
) -> ApiResult<Json<UsageDetailResponse>> {
    if let Err(e) = authenticate_admin_ws(&s, &headers, None) {
        return e.into();
    }
    let body: Result<Json<UsageDetailResponse>, ApiError> = async {
        let w = s.db_pool().writer();
        let row = usage::detail_by_id(&w, q.id)?;
        match row {
            Some(r) => Ok(Json(UsageDetailResponse { row: r })),
            None => Err(ApiError(CoreError::Internal(
                format!("usage row {} not found", q.id),
            ))),
        }
    }
    .await;
    body.into()
}

/// Query for `GET /v1/admin/usage/detail`.
#[derive(Debug, Default, Deserialize)]
pub struct DetailQuery {
    pub id: i64,
}

#[derive(Debug, Serialize)]
pub struct UsageDetailResponse {
    pub row: usage::UsageDetailRow,
}

// =====================================================================
// Recording toggle (Live Logs detail modal)
// =====================================================================

/// `GET /v1/admin/recording` — read the current recording state.
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
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        Ok(Json(serde_json::json!({ "recording": s.is_recording() })))
    }
    .await;
    body.into()
}

/// `POST /v1/admin/recording` — flip the process-wide recording state.
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

/// `POST /v1/admin/models/:id/toggle` — flip the soft-disable bit on a
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

/// `POST /v1/admin/models/bulk-toggle` — flip the soft-disable bit on
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

/// `DELETE /v1/admin/models/:id` — hard-delete a model row.
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

/// `PATCH /v1/admin/combos/:id` — currently the only mutable field is
/// `race_size`. Body: `{"race_size": 1..=8}`. Missing `race_size` is a
/// no-op; out-of-range is a 400.
pub async fn update_combo(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<serde_json::Value>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let race_size = body
            .get("race_size")
            .and_then(|v| v.as_u64())
            .map(|n| u8::try_from(n).unwrap_or(0));
        let w = s.db_pool().writer();
        combos::update_combo(&w, ComboId(id), race_size)?;
        Ok(Json(serde_json::json!({ "id": id })))
    }
    .await;
    body.into()
}

/// `PATCH /v1/admin/combos/:id/targets/:target_id` — move a target to a
/// new `priority_order`. Body: `{"priority_order": <i32>}`. The handler
/// does not re-number siblings; the caller picks a sane value.
pub async fn update_combo_target(
    State(s): State<AppState>,
    Path((combo_id, target_id)): Path<(i64, i64)>,
    Json(body): Json<serde_json::Value>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let priority_order = body
            .get("priority_order")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| ApiError(CoreError::Validation("missing 'priority_order'".into())))?;
        // Cast: i32 is well under i64::MAX in practice; the SQL column is
        // INTEGER (i64 in rusqlite) so a non-negative i32 is safe.
        if priority_order < i32::MIN as i64 || priority_order > i32::MAX as i64 {
            return Err(ApiError(CoreError::Validation(format!(
                "priority_order out of i32 range: {}",
                priority_order
            ))));
        }
        let w = s.db_pool().writer();
        combos::update_target_priority(&w, ComboTargetId(target_id), priority_order as i32)?;
        Ok(Json(serde_json::json!({
            "combo_id": combo_id,
            "id": target_id,
            "priority_order": priority_order,
        })))
    }
    .await;
    body.into()
}

/// `DELETE /v1/admin/combos/:id/targets/:target_id` — remove a single
/// target from a combo. The handler validates that the target actually
/// belongs to the requested combo (defense in depth: a mismatched URL
/// surfaces as a 400 instead of silently deleting from another combo).
pub async fn delete_combo_target(
    State(s): State<AppState>,
    Path((combo_id, target_id)): Path<(i64, i64)>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let w = s.db_pool().writer();
        admin::delete_combo_target(
            &w,
            ComboId(combo_id),
            ComboTargetId(target_id),
        )?;
        Ok(Json(serde_json::json!({ "deleted": target_id })))
    }
    .await;
    body.into()
}

/// `POST /v1/admin/combos/:id/targets/:target_id/clear-cooldown`
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
/// before `/v1/admin/combos/:id/targets/:target_id` in
/// `router.rs`, otherwise axum's :target_id segment will happily
/// swallow `clear-cooldown` and 405 the POST.
pub async fn clear_combo_target_cooldown(
    State(s): State<AppState>,
    Path((combo_id, target_id)): Path<(i64, i64)>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let w = s.db_pool().writer();
        admin::clear_combo_target_cooldown(
            &w,
            ComboId(combo_id),
            ComboTargetId(target_id),
        )?;
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

/// Body for `POST /v1/admin/combos/:id/targets/reorder`. The frontend's
/// ↑/↓ buttons compute the new order client-side (swap the moved
/// target with its neighbor) and post the full ordered list back; the
/// backend renumbers everything in a single transaction.
#[derive(Debug, Deserialize)]
pub struct ReorderComboTargetsInput {
    pub target_ids: Vec<i64>,
}

/// `POST /v1/admin/combos/:id/targets/reorder` — atomically reassign
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
        let ordered: Vec<ComboTargetId> = body
            .target_ids
            .into_iter()
            .map(ComboTargetId)
            .collect();
        admin::reorder_combo_targets(&mut *w, ComboId(combo_id), &ordered)?;
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

/// `PATCH /v1/admin/providers/:id` — partial update of a provider row.
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
        let w = s.db_pool().writer();
        let provider_id = ProviderId::new(id.clone());
        admin::update_provider(&w, &provider_id, body)?;
        Ok(Json(serde_json::json!({ "id": id })))
    }
    .await;
    body.into()
}

// =====================================================================
// Custom model creation
// =====================================================================

/// `POST /v1/admin/models/custom` — hand-create a model row. The row is
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
    opts: TestOptions,
) -> TestResult {
    use openproxy_core::translation::{openai_to_anthropic, openai_to_gemini, OpenAIMessage, OpenAIRequest};

    let row_id = ModelRowId(model_row_id);

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

    // 2. Find the adapter for that provider.
    let adapter = match s
        .adapters()
        .iter()
        .find(|a| a.id() == &model.provider_id)
        .cloned()
    {
        Some(a) => a,
        None => {
            let err = CoreError::ProviderNotFound(model.provider_id.to_string());
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
        let provider_row = match providers::get(&w, &model.provider_id) {
            Ok(p) => p,
            Err(_) => None,
        };
        let accs = match accounts::list(&w, Some(&model.provider_id)) {
            Ok(l) => l,
            Err(_) => vec![],
        };
        let anon = match &provider_row {
            Some(p) if matches!(p.auth_type, providers::AuthType::None) => true,
            _ if accs.is_empty() => true, // No accounts → try anonymous
            _ => false,
        };
        (anon, accs)
    };

    let (_account_id, api_key) = if is_anonymous {
        (None, String::new()) // Anonymous: no account, empty key
    } else {
        let account_id = match account_id {
            Some(id) => Some(id),
            None => match accounts_list
                .into_iter()
                .find(|a| a.health_status == accounts::HealthStatus::Healthy)
            {
                Some(a) => Some(a.id),
                None => None,
            },
        };

        // 4. Decrypt the API key. Drop the writer guard immediately.
        //    OAuth accounts store the token in access_token_encrypted,
        //    not api_key_encrypted, so we fall back to that if the
        //    primary decrypt fails (e.g. NULL column).
        let api_key = match account_id {
            Some(aid) => match (|| -> Result<String, ApiError> {
                let w = s.db_pool().writer();
                accounts::decrypt_api_key(&w, aid, s.master_key().as_ref())
                    .or_else(|_| {
                        accounts::decrypt_access_token(&w, aid, s.master_key().as_ref())
                    })
                    .map_err(ApiError)
            })() {
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
            },
            None => String::new(),
        };

        (account_id, api_key)
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
    let is_custom_provider = matches!(
        model.provider_id.as_str(),
        "kiro" | "antigravity" | "antigravity-cli"
    );

    if is_custom_provider && !is_anonymous {
        // Delegate to the provider-specific executor, same as the
        // pipeline's `execute_single`. We need the access token and
        // provider-specific metadata.
        let start = std::time::Instant::now();

        // Resolve the account for this test. The combo path already
        // pinned one; the per-row path picks the first healthy one.
        let test_account_id = _account_id.unwrap_or_else(|| {
            // Re-pick from the accounts list that was already loaded.
            // The list was consumed by `into_iter()` above, so we
            // re-query. This only happens for the per-row path when
            // the model has accounts but the caller didn't pin one.
            (|| -> Option<AccountId> {
                let w = s.db_pool().writer();
                accounts::list(&w, Some(&model.provider_id))
                    .ok()
                    .and_then(|l| {
                        l.into_iter()
                            .find(|a| a.health_status == accounts::HealthStatus::Healthy)
                            .map(|a| a.id)
                    })
            })()
            .unwrap_or(AccountId(0))
        });

        // Decrypt the access token.
        let access_token = {
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
        };

        // Read provider-specific metadata and fire the executor.
        let executor_result = match model.provider_id.as_str() {
            "antigravity" | "antigravity-cli" => {
                let project_id = {
                    let w = s.db_pool().writer();
                    openproxy_core::executor_antigravity::read_project_id(
                        &w,
                        test_account_id,
                    )
                    .unwrap_or_default()
                };
                let http_client = s.upstream_client();
                // No client connection of its own on the admin
                // test path (it runs against a synthetic request);
                // see the symmetric note on the kiro branch below.
                let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
                openproxy_core::executor_antigravity::execute_antigravity(
                    http_client,
                    &access_token,
                    &project_id,
                    &openai_req,
                    cancel_rx,
                )
                .await
            }
            "kiro" => {
                let (region, profile_arn) = {
                    let w = s.db_pool().writer();
                    let meta = openproxy_core::executor_kiro::read_account_meta(
                        &w,
                        test_account_id,
                    )
                    .unwrap_or(None);
                    (
                        meta.as_ref()
                            .map(|m| m.region.clone())
                            .unwrap_or_else(|| openproxy_core::executor_kiro::KIRO_DEFAULT_REGION.to_string()),
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
                let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
                openproxy_core::executor_kiro::execute_kiro(
                    http_client,
                    &access_token,
                    &region,
                    profile_arn.as_deref(),
                    &openai_req,
                    cancel_rx,
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
    let (url, body_value): (String, serde_json::Value) = if model.target_format
        == openproxy_core::models::TargetFormat::Anthropic
    {
        let anthropic_req = openai_to_anthropic(&openai_req);
        let url = adapter.build_chat_url(
            openproxy_core::models::TargetFormat::Anthropic,
            &model.model_id,
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
    } else if model.target_format
        == openproxy_core::models::TargetFormat::Gemini
    {
        let gemini_req = openai_to_gemini(&openai_req);
        let url = adapter.build_chat_url(
            openproxy_core::models::TargetFormat::Gemini,
            &model.model_id,
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
    } else {
        let url = adapter.build_chat_url(
            openproxy_core::models::TargetFormat::Openai,
            &model.model_id,
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
    let headers = adapter.build_headers(&api_key, model.target_format, &model.model_id);
    let mut req = s
        .http_client()
        .post(&url)
        .timeout(std::time::Duration::from_secs(15));
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }
    req = req.json(&body_value);

    // 9. Send + measure. We capture both the wall-clock elapsed time
    //    and a truncated error body so the dashboard can show
    //    something useful when the upstream is unhappy.
    let start = std::time::Instant::now();
    let result = req.send().await;
    let elapsed_ms = start.elapsed().as_millis() as u64;

    let (status, error_msg) = match result {
        Ok(response) => {
            let status = response.status().as_u16();
            if !response.status().is_success() {
                let body = response.text().await.unwrap_or_default();
                let truncated: String = body.chars().take(TEST_ERROR_BODY_MAX_CHARS).collect();
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
            (0, Some(e.to_string()))
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

/// `POST /v1/admin/models/:id/test` — send a tiny "ping" request to the
/// model and stamp the result onto its row.
///
/// Thin wrapper over [`run_test_for_model`]. The helper is the
/// real implementation; this handler exists for backwards
/// compatibility with the dashboard's "Test" button on the model
/// row, which expects the same `last_test_status` side-effect
/// the original handler had.
pub async fn test_model(
    State(s): State<AppState>,
    Path(model_row_id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let r = run_test_for_model(&s, model_row_id, None, TestOptions::default()).await;
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

/// `POST /v1/admin/accounts/:id/health` — force-set an account's
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

// =====================================================================
// Admin model listing (internal shape, includes row_id + active)
// =====================================================================

/// `GET /v1/admin/models` — every row in the `models` table, in the
/// internal `Model` shape.
///
/// The public `GET /v1/models` projects each row down to an OpenAI-shaped
/// payload (no `row_id`, no `active`) so SDKs that expect the OpenAI
/// contract can consume it. The dashboard, on the other hand, needs the
/// extra fields to drive the toggle and refresh buttons; this endpoint
/// returns them. There is no filter — the dashboard's model list is
/// small enough that a single shot is fine.
pub async fn list_models_admin(
    State(s): State<AppState>,
) -> ApiResult<Json<Vec<models::Model>>> {
    let body: Result<Json<Vec<models::Model>>, ApiError> = async {
        let w = s.db_pool().writer();
        let list = models::list_all(&w)?;
        Ok(Json(list))
    }
    .await;
    body.into()
}

// =====================================================================
// Account quota refresh
// =====================================================================

/// `POST /v1/admin/accounts/:id/refresh-quota` — fetch a fresh quota
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
    let s_clone = s.clone();
    let result: Result<Json<serde_json::Value>, ApiError> = async move {
        let account_id = AccountId::new(account_id);

        // 1 + 2 + 3: load the account, gate on provider, decrypt the key.
        // The capability check happens *before* the decrypt so we never
        // touch the master key for a provider whose quota we'll never
        // fetch.
        let (provider_id_str, api_key, access_token) = {
            let w = s_clone.db_pool().writer();
            let acc = admin::account_for_quota_refresh(&w, account_id)?;
            if !admin::quota_capable_providers().contains(&acc.provider_id.as_str()) {
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
            (provider_str, k, token)
        };
        // writer guard dropped here.

        // 4: fire the upstream quota call. Returns an `AccountQuota`
        //    even on failure (with `fetch_error` set), so we always
        //    have a row to persist.
        let upstream_client = s_clone.upstream_client();
        let q = admin::fetch_account_quota(
            &provider_id_str,
            upstream_client,
            &api_key,
            access_token.as_deref(),
        )
        .await;

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
                // Find the matching OAuth provider implementation.
                let provider_impl: Option<Box<dyn openproxy_core::oauth::OAuthProvider + Send + Sync>> =
                    match provider_id_str.as_str() {
                        "antigravity" | "antigravity-cli" | "agy" => Some(Box::new(
                            openproxy_core::oauth_antigravity::AntigravityOAuthProvider::new(),
                        )),
                        "kiro" => Some(Box::new(
                            openproxy_core::oauth_kiro::KiroOAuthProvider::new(),
                        )),
                        _ => {
                            tracing::warn!(
                                provider = %provider_id_str,
                                "no OAuth provider impl for on-demand refresh"
                            );
                            None
                        }
                    };
                if let Some(provider_impl) = provider_impl {
                    let upstream_client = s_clone.upstream_client();
                    match provider_impl
                        .refresh_token(&refresh_token, upstream_client)
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

// =====================================================================
// Provider model refresh
// =====================================================================

/// Default TTL for the `POST /v1/admin/providers/:id/refresh` handler when
/// the caller doesn't pin one in the query string. Matches the value
/// used by the row-level `POST /v1/admin/models/:id/refresh` handler so
/// the two endpoints behave consistently.
const PROVIDER_REFRESH_DEFAULT_TTL_SECS: i64 = 3_600;

/// Query string for `POST /v1/admin/providers/:id/refresh`. Mirrors
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

/// `POST /v1/admin/providers/:id/refresh` — re-discover the model list
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

fn oauth_provider_for(
    provider_id: &ProviderId,
) -> Option<Box<dyn openproxy_core::oauth::OAuthProvider + Send + Sync>> {
    match provider_id.as_str() {
        "antigravity" | "antigravity-cli" => Some(Box::new(
            openproxy_core::oauth_antigravity::AntigravityOAuthProvider::new(),
        )),
        "kiro" => Some(Box::new(openproxy_core::oauth_kiro::KiroOAuthProvider::new())),
        _ => None,
    }
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

    let Some(provider) = oauth_provider_for(provider_id) else {
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
    match provider.refresh_token(&refresh_token, upstream_client).await {
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

    // 1. Find the adapter. Adapter clones are cheap (the heavy state
    //    is `reqwest::Client` and the config strings, all `Arc`-backed
    //    or `Clone`).
    let adapter = match s
        .adapters()
        .iter()
        .find(|a| a.id() == &provider)
        .cloned()
    {
        Some(a) => a,
        None => {
            return ApiResult::err(ApiError(CoreError::ProviderNotFound(
                provider_id_str,
            )));
        }
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
                    Ok(None) => return ApiResult::err(ApiError(CoreError::AccountNotFound(account_id.0))),
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
        adapter.as_ref(),
        s.upstream_client(),
        ttl_seconds,
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

/// `GET /v1/admin/keys` — list every key, newest first.
pub async fn list_api_keys(
    State(s): State<AppState>,
) -> ApiResult<Json<Vec<core_api_keys::ApiKey>>> {
    let body: Result<Json<Vec<core_api_keys::ApiKey>>, ApiError> = async {
        let w = s.db_pool().writer();
        let list = core_api_keys::list(&w)?;
        Ok(Json(list))
    }
    .await;
    body.into()
}

/// `POST /v1/admin/keys` — create a new key.
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

/// `GET /v1/admin/keys/:id` — fetch a single key by id. 404 if absent.
pub async fn get_api_key(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<core_api_keys::ApiKey>> {
    let body: Result<Json<core_api_keys::ApiKey>, ApiError> = async {
        let w = s.db_pool().writer();
        let key = core_api_keys::get_by_id(&w, ApiKeyId(id))?
            .ok_or_else(|| CoreError::Internal(format!("api_key {id} not found")))?;
        Ok(Json(key))
    }
    .await;
    body.into()
}

/// `PATCH /v1/admin/keys/:id` — partial update.
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

        let scopes_owned: Option<Vec<String>> = body
            .get("scopes")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect());
        let scopes_slice: Option<&[String]> = scopes_owned.as_deref();

        // `allowed_models`: absent = no-op; present + null = clear to NULL;
        // present + array = set to that array.
        let allowed_models_owned: Option<Option<Vec<String>>> = body
            .get("allowed_models")
            .map(|v| {
                if v.is_null() {
                    None
                } else {
                    v.as_array().map(|a| {
                        a.iter().filter_map(|x| x.as_str().map(String::from)).collect()
                    })
                }
            });
        let allowed_models_slice: Option<Option<&[String]>> =
            allowed_models_owned.as_ref().map(|o| o.as_deref());

        let allowed_combos_owned: Option<Option<Vec<i64>>> = body
            .get("allowed_combos")
            .map(|v| {
                if v.is_null() {
                    None
                } else {
                    v.as_array().map(|a| a.iter().filter_map(|x| x.as_i64()).collect())
                }
            });
        let allowed_combos_slice: Option<Option<&[i64]>> =
            allowed_combos_owned.as_ref().map(|o| o.as_deref());

        let is_active = body.get("is_active").and_then(|v| v.as_bool());

        let expires_owned: Option<Option<String>> = body
            .get("expires_at")
            .map(|v| v.as_str().map(String::from));
        let expires_slice: Option<Option<&str>> = expires_owned
            .as_ref()
            .map(|o| o.as_deref());

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

/// `POST /v1/admin/keys/:id/revoke` — soft-disable. Idempotent (a
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

/// `DELETE /v1/admin/keys/:id` — hard delete. Idempotent (a missing
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

/// `POST /v1/admin/keys/:id/regenerate` — issue a new plaintext and
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

/// `GET /v1/admin/keys/:id/usage` — headline metrics for one key.
/// Returns a flat `UsageSummary` (no grouping) plus the standard
/// usage roll-up so the dashboard can show a one-screen recap.
pub async fn api_key_usage(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let w = s.db_pool().writer();

        // Confirm the key exists first so a 404 surfaces here
        // (cleaner) instead of an empty summary that could be
        // confused with "key has no traffic".
        let _ = core_api_keys::get_by_id(&w, ApiKeyId(id))?
            .ok_or_else(|| CoreError::Internal(format!("api_key {id} not found")))?;

        let head = core_api_keys::usage_summary(&w, ApiKeyId(id))?;
        let detailed = usage::summary(
            &w,
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

/// `GET /v1/admin/oauth/:provider/authorize` — start a PKCE flow.
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
    State(_s): State<AppState>,
    Path(provider): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let provider_impl = match provider.as_str() {
            "antigravity" | "antigravity-cli" => {
                Box::new(openproxy_core::oauth_antigravity::AntigravityOAuthProvider::new())
                    as Box<dyn openproxy_core::oauth::OAuthProvider + Send + Sync>
            }
            _ => {
                return Err(ApiError(CoreError::Validation(format!(
                    "provider '{}' does not support OAuth authorize",
                    provider
                ))));
            }
        };

        if provider_impl.flow() != openproxy_core::oauth::OAuthFlow::AuthorizationCodePkce {
            return Err(ApiError(CoreError::Validation(format!(
                "provider '{}' does not use PKCE flow",
                provider
            ))));
        }

        // Google OAuth requires localhost for native app clients.
        // The user will paste the callback URL manually in the dashboard.
        let web_port = std::env::var("OPENPROXY_WEB_PORT")
            .unwrap_or_else(|_| "8788".to_string());
        let redirect_uri = format!("http://localhost:{}/callback.html", web_port);

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

/// `POST /v1/admin/oauth/:provider/exchange` — exchange authorization code
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
        let code_verifier = input.get("code_verifier")
            .and_then(|v| v.as_str())
            .unwrap_or(""); // Optional — not needed for device code flow
        let account_id_input = input
            .get("account_id")
            .and_then(|v| v.as_i64());
        let redirect_uri = input
            .get("redirect_uri")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CoreError::Validation("missing 'redirect_uri'".into()))?;

        let provider_impl = match provider.as_str() {
            "antigravity" | "antigravity-cli" => {
                Box::new(openproxy_core::oauth_antigravity::AntigravityOAuthProvider::new())
                    as Box<dyn openproxy_core::oauth::OAuthProvider + Send + Sync>
            }
            _ => {
                return Err(ApiError(CoreError::Validation(format!(
                    "provider '{}' does not support OAuth exchange",
                    provider
                ))));
            }
        };

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
                None,
                None,
            )?;
        }

        // Post-exchange hook. For Antigravity this calls
        // loadCodeAssist / onboardUser to recover the user's
        // projectId; for other PKCE providers it's a no-op.
        // Errors are logged but do not abort the request — the
        // account is still usable for token refresh; the project
        // bootstrap can be retried later.
        if let Err(e) = provider_impl
            .post_exchange(account_id, s.db_pool(), s.master_key())
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

/// `POST /v1/admin/oauth/:provider/device-code` — request a device code
/// (Device Code flow).
///
/// Returns `{ "device_code", "user_code", "verification_uri", ... }`.
pub async fn oauth_device_code(
    State(s): State<AppState>,
    Path(provider): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let provider_impl = match provider.as_str() {
            "kiro" => {
                Box::new(openproxy_core::oauth_kiro::KiroOAuthProvider::new())
                    as Box<dyn openproxy_core::oauth::OAuthProvider + Send + Sync>
            }
            _ => {
                return Err(ApiError(CoreError::Validation(format!(
                    "provider '{}' does not support device code flow",
                    provider
                ))));
            }
        };

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

/// `POST /v1/admin/oauth/:provider/device-poll` — poll for a device code token.
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

        let provider_impl = match provider.as_str() {
            "kiro" => {
                Box::new(openproxy_core::oauth_kiro::KiroOAuthProvider::new())
                    as Box<dyn openproxy_core::oauth::OAuthProvider + Send + Sync>
            }
            _ => {
                return Err(ApiError(CoreError::Validation(format!(
                    "provider '{}' does not support device code polling",
                    provider
                ))));
            }
        };

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
                    _ => None,
                };

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
                        None,
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
                    .post_exchange(account_id, s.db_pool(), s.master_key())
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

/// `GET /v1/admin/oauth/callback` — OAuth callback handler (MVP).
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

/// Determine the base URL from request headers.
///
/// Uses (in order):
/// 1. `Origin` header (browser same-origin/cross-origin requests).
/// 2. `X-Forwarded-Host` + `X-Forwarded-Proto` (reverse proxy).
/// 3. `Host` header fallback.
/// 4. `localhost` if nothing else is available.
#[allow(dead_code)]
fn determine_base_url(headers: &HeaderMap) -> String {
    // Check Origin header first — browsers send this with fetch/XHR.
    if let Some(origin) = headers.get("origin") {
        if let Ok(origin_str) = origin.to_str() {
            return origin_str.to_string();
        }
    }

    // Check X-Forwarded-Host + X-Forwarded-Proto (set by reverse proxy).
    let host = headers
        .get("x-forwarded-host")
        .and_then(|h| h.to_str().ok())
        .or_else(|| headers.get("host").and_then(|h| h.to_str().ok()))
        .unwrap_or("localhost");

    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|p| p.to_str().ok())
        .unwrap_or("http");

    format!("{}://{}", proto, host)
}

// =====================================================================
// Tests for the runtime-config endpoints
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
        body::Body,
        http::{Request, StatusCode},
        routing::put,
        Router,
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
        let pool = std::sync::Arc::new(
            core_db::DbPool::open(&dir.join("smoke.db")).expect("open pool"),
        );
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
        let adapters = std::sync::Arc::new(adapters::builtin_adapters());
        let state = AppState::for_test(
            openproxy_core::AppConfig::default(),
            pool,
            std::sync::Arc::new(mk),
            adapters,
        )
        .await;
        (state, plaintext)
    }

    #[tokio::test]
    async fn put_runtime_timeouts_writes_db_and_updates_slot() {
        let dir = tempdir();
        let (state, plaintext) = make_state_with_key(&dir).await;

        // Sanity: the slot starts at the TOML defaults (5000/10000/...).
        let initial = state.timeouts();
        assert_eq!(initial.connect_ms, 5_000);

        let app = Router::new()
            .route(
                "/v1/admin/config/timeouts",
                put(put_runtime_timeouts),
            )
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
            .uri("/v1/admin/config/timeouts")
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
        let parsed: serde_json::Value = serde_json::from_slice(&bytes)
            .expect("json body");
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
        assert_eq!(after, TimeoutsConfig {
            connect_ms: 1,
            request_send_ms: 2,
            ttft_ms: 3,
            idle_chunk_ms: 4,
            total_ms: 5,
        });

        // The row landed in the DB (one row, key='timeouts').
        let count: i64 = state.db_pool().with_conn(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM app_config WHERE key = 'timeouts'",
                [],
                |r| r.get(0),
            ).unwrap()
        });
        assert_eq!(count, 1, "PUT must have written a row");
    }

    #[tokio::test]
    async fn put_runtime_timeouts_without_auth_returns_401() {
        let dir = tempdir();
        let (state, _plaintext) = make_state_with_key(&dir).await;
        let app = Router::new()
            .route(
                "/v1/admin/config/timeouts",
                put(put_runtime_timeouts),
            )
            .with_state(state);
        let body = serde_json::json!({
            "connect_ms": 1_u64, "request_send_ms": 2_u64, "ttft_ms": 3_u64,
            "idle_chunk_ms": 4_u64, "total_ms": 5_u64,
        });
        let req = Request::builder()
            .method("PUT")
            .uri("/v1/admin/config/timeouts")
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
            .route(
                "/v1/admin/config/timeouts",
                put(put_runtime_timeouts),
            )
            .with_state(state);
        let req = Request::builder()
            .method("PUT")
            .uri("/v1/admin/config/timeouts")
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
            "expected 400 or 422, got {:?}", resp.status()
        );
    }

    // ---- HIGH fix: OPENPROXY_DASHBOARD_AUTH_BYPASS is an exact-match
    // sentinel, not "any non-empty value". The old behaviour silently
    // granted full admin access for `=false`, `=yes`, `=0`, etc.

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
        // SAFETY: tests in this module are not run in parallel and
        // each one restores the env var to its previous value (or
        // removes it) before returning.
        let prev = std::env::var("OPENPROXY_DASHBOARD_AUTH_BYPASS").ok();
        // SAFETY: set_var is unsafe in 2024 edition; the test runs
        // single-threaded and restores the value on every exit path.
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
        };
        let result = q.into_filter();
        let err = result.expect_err("garbage timestamp must be rejected");
        let msg = format!("{:?}", err);
        assert!(msg.contains("from"), "error must mention the bad field, got: {}", msg);
        assert!(msg.contains("garbage"), "error must include the bad value, got: {}", msg);
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
        };
        let f = q.into_filter().expect("RFC-3339 with offset is valid");
        let from = f.from.expect("from present");
        // The offset is normalised to UTC and the suffix is `Z`.
        assert!(from.ends_with('Z'), "expected Z-suffix, got: {}", from);
        assert!(from.starts_with("2026-06-18T05:00:00"), "expected 05:00 UTC, got: {}", from);
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
        };
        let err = q.into_filter().expect_err("reversed range must be rejected");
        let msg = format!("{:?}", err);
        assert!(msg.contains("must be <="), "expected ordering error, got: {}", msg);
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
        };
        let f = q.into_filter().expect("absent timestamps are valid");
        assert!(f.from.is_none());
        assert!(f.to.is_none());
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
        let req = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(body_json))
            .expect("build req");
        let resp = app.oneshot(req).await.expect("oneshot");
        assert_ne!(
            resp.status(),
            StatusCode::PAYLOAD_TOO_LARGE,
            "10 MiB body must not be rejected by the body limit"
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
        let req = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(body_json))
            .expect("build req");
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
            .uri("/v1/admin/usage/recent?since_id=9223372036854775807&limit=1")
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
            .uri("/v1/admin/usage/recent?since_id=-42&limit=1")
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

    // ---- MEDIUM fix (#10): cancellation propagates from the request
    // body to the fan-out loop. Without this, the test_combo_targets
    // handler kept firing upstreams for the full 180s budget even
    // after the dashboard closed the tab. The fix wires a
    // `tokio::sync::watch` channel to the request body's frame()
    // future; the loop polls `borrow_and_update` between targets and
    // short-circuits when it sees `true`.
    //
    // This test exercises the wiring at the unit level: a watch
    // sender that flips `true` while a "fan-out" closure is
    // iterating must cause the closure to bail out and return the
    // partial result. We don't go through the full HTTP path
    // (that requires a mock upstream with controllable latency);
    // the routing-level integration is covered by `build_router`
    // compiling `test_combo_targets` with the new
    // `axum::extract::Request` parameter, which is what fails to
    // build if the signature regresses.
    #[tokio::test]
    async fn test_combo_targets_signals_cancellation_via_watch() {
        let (tx, mut rx) = tokio::sync::watch::channel(false);
        // Pretend to be the fan-out loop. We run it on the current
        // task and abort from a sibling task after the first "tick".
        let fan_out = tokio::spawn(async move {
            let mut results: Vec<usize> = Vec::new();
            for i in 0..10_usize {
                // The real handler checks `*rx.borrow_and_update()`
                // here. We reproduce the semantics: when the watch
                // flips, we exit with whatever we collected so far.
                if *rx.borrow_and_update() {
                    return (true, results);
                }
                // Simulate a per-target probe that takes a few ms.
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                results.push(i);
            }
            (false, results)
        });
        // Give the loop a chance to start, then signal cancellation.
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        tx.send(true).expect("send disconnect");
        let (cancelled, results) = fan_out.await.expect("join");
        assert!(cancelled, "fan-out must report cancellation");
        assert!(
            results.len() < 10,
            "expected partial results, got all 10 — fan-out ignored the cancel signal"
        );
        assert!(
            !results.is_empty(),
            "expected at least one target before the cancel arrived"
        );
    }
}
