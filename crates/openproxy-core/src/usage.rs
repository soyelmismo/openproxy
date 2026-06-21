//! Usage queries: list, summary, by-model, by-account, by-status, errors.
//!
//! See docs/mvp-spec.md §7 (Analytics Queries) and §8 (SQLite Schema).
//!
//! This module is the *read* side of the usage table; inserts live in
//! [`crate::cost::record`]. All queries share a common filter shape and are
//! race-aware: `winners` counts rows where `race_lost = 0`, `losers` counts
//! `race_lost = 1`, and `unique_requests` is `COUNT(DISTINCT request_id)` so a
//! race of N losers + 1 winner counts as one logical request.
//!
//! ## Broadcast channel
//!
//! The module also owns the canonical in-process broadcast channel for newly
//! inserted usage rows. After [`crate::cost::record`] inserts a row, it calls
//! [`publish_usage_row`] to push the new row to all connected WebSocket
//! clients. The sender is stored in a `once_cell::sync::OnceCell` so it is
//! initialized exactly once and accessible from any crate.

use crate::error::{CoreError, Result};
use crate::ids::{AccountId, ApiKeyId, ComboId, ComboTargetId, ModelRowId, ProviderId, UsageId};
use once_cell::sync::OnceCell;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, ToSql};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use tokio::sync::broadcast;

// ---------------------------------------------------------------------------
// Broadcast channel (global, initialized once)
// ---------------------------------------------------------------------------

/// Channel capacity for the usage broadcast. Rows older than this are
/// dropped for slow receivers.
const BROADCAST_CAPACITY: usize = 256;

static USAGE_SENDER: OnceCell<broadcast::Sender<RecentUsageRow>> = OnceCell::new();

/// Secondary broadcast channel for *in-flight* stage transitions of
/// requests still being processed. Subscribers (the admin live-log
/// WebSocket) re-emit these to the dashboard so the operator can see
/// each request progress through phases like
/// `connecting → waiting_ttft → streaming → completed`.
///
/// This is intentionally a *separate* channel from `USAGE_SENDER`:
/// `USAGE_SENDER` carries full `RecentUsageRow`s stamped at the very
/// end of a request (post-`cost::record`), and every row has a real
/// `UsageId`. `STAGE_SENDER` carries transient `StageEvent`s keyed
/// only by `request_id` and have no DB id — the dashboard uses
/// `request_id` to update the row that the matching `recent` row
/// lives under.
///
/// Channel capacity: stages fire in bursts. A typical request emits
/// `started → connecting → waiting_ttft → streaming → completed`
/// (~5 events). 1024 is plenty headroom for 100+ concurrent requests.
const STAGE_BROADCAST_CAPACITY: usize = 1024;
static STAGE_SENDER: OnceCell<broadcast::Sender<StageEvent>> = OnceCell::new();

/// Initialize the global usage broadcast sender. Must be called exactly
/// once before any call to [`usage_broadcast`] or [`publish_usage_row`].
/// Returns a clone of the sender so callers (e.g. `AppState`) can store
/// it for later subscription.
pub fn init_usage_broadcast() -> broadcast::Sender<RecentUsageRow> {
    let (tx, _rx) = broadcast::channel(BROADCAST_CAPACITY);
    // We ignore the error: if already initialized, the existing sender
    // is returned and we just discard the new one. This makes the function
    // idempotent for tests that call it more than once.
    let _ = USAGE_SENDER.set(tx);
    usage_broadcast()
}

/// Return a clone of the global usage broadcast sender.
/// Panics if [`init_usage_broadcast`] has not been called yet.
pub fn usage_broadcast() -> broadcast::Sender<RecentUsageRow> {
    USAGE_SENDER
        .get()
        .expect("init_usage_broadcast() must be called before usage_broadcast()")
        .clone()
}

/// Publish a newly inserted usage row to all broadcast subscribers.
/// Silently ignores errors (e.g. no subscribers) — broadcast send
/// failures must never fail the caller.
///
/// SEC-MEDIUM-C fix: the raw row includes `request_body_json`,
/// `response_body_json`, `request_headers`, and `response_headers`,
/// whose combined size can be multi-MB. The dashboard's WS broadcast
/// was fan-outing the full row to every subscriber (PII + bandwidth
/// amplifier). Strip the heavy fields before sending — the detail view
/// at `GET /admin/usage/detail?id=...` reads them straight from
/// the database so on-demand access is preserved.
pub fn publish_usage_row(row: RecentUsageRow) {
    if let Some(tx) = USAGE_SENDER.get() {
        // `send` returns Err when there are no receivers, which is
        // expected and harmless.
        let _ = tx.send(redact_for_broadcast(row));
    }
}

/// Strip the heavyweight fields from a row before it leaves the
/// process. The dashboard subscribes to recent rows via WS and
/// `GET /admin/usage/recent`; both routes return this redacted shape.
/// The full fields remain available on demand via the detail endpoint.
pub fn redact_for_broadcast(mut row: RecentUsageRow) -> RecentUsageRow {
    row.request_body_json = None;
    row.response_body_json = None;
    row.request_headers = None;
    row.response_headers = None;
    row
}

// ---------------------------------------------------------------------------
// In-flight stage events (for the live-log dashboard's millisecond
// debug view). These are NOT persisted to the database; they are
// transient broadcasts that update the dashboard view of an in-flight
// request as it transitions through phases. The wire shape is a JSON
// object with `{ type: "stage", request_id, stage, elapsed_ms, ... }`.
// ---------------------------------------------------------------------------

/// One phase transition of an in-flight request. The dashboard maps
/// these to the matching row by `request_id`.
///
/// `elapsed_ms` is the wall-clock milliseconds since the pipeline
/// accepted the request (the `started` instant). The dashboard uses
/// this for the "X ms in this phase" sublabel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageEvent {
    /// Which request this stage belongs to. The dashboard uses this
    /// to find the existing row in the live-log view.
    pub request_id: String,
    /// Trace id (per-attempt). Allows distinguishing race-lost
    /// attempts of the same `request_id` if the dashboard grows
    /// that view later.
    pub trace_id: String,
    /// Free-form provider id (e.g. `openrouter`, `kiro`).
    pub provider_id: String,
    /// Upstream model id. May be empty for the very first
    /// `started` event when the model hasn't been resolved yet.
    pub upstream_model_id: String,
    /// Coarse stage label. The dashboard's `STAGE_LABELS` map renders
    /// a human-friendly description and picks a colour class.
    /// One of: `started`, `connecting`, `waiting_ttft`,
    /// `streaming`, `completed`, `failed`.
    pub stage: String,
    /// Wall-clock ms since the request was accepted by the pipeline.
    /// Lets the dashboard show "X ms in this phase" without
    /// recomputing on the client.
    pub elapsed_ms: u64,
    /// `connect_ms` (ms from request build to first upstream byte)
    /// when the stage event captures that, else `None`. Only set on
    /// `waiting_ttft` and beyond.
    pub connect_ms: Option<u64>,
    /// `ttft_ms` (ms from first upstream byte to first SSE data line)
    /// when the stage event captures that, else `None`. Only set on
    /// `streaming` and beyond.
    pub ttft_ms: Option<u64>,
    /// HTTP status code. `0` while in flight; the final code on
    /// `completed`/`failed`.
    pub status_code: u16,
    /// `Some(reason)` only on `failed`; `None` for all other stages.
    pub error: Option<String>,
    /// Upstream stop reason (e.g. "end_turn", "max_tokens",
    /// "stop_sequence" for Anthropic; "stop", "length" for OpenAI).
    /// Only set on terminal events (`completed`/`failed`).
    pub stop_reason: Option<String>,
    /// Compression savings percentage (0.0–100.0). None when off.
    pub compression_savings_pct: Option<f64>,
    /// Compression techniques applied (CSV). None when off.
    pub compression_techniques: Option<String>,
    /// Wall-clock millis at the time the event was emitted
    /// (RFC-3339). Used by the dashboard to keep the stage label
    /// timeline accurate even if the WS delivery is slightly late.
    pub timestamp: String,
}

/// Initialize the global stage broadcast sender. Idempotent (safe
/// to call multiple times in tests).
pub fn init_stage_broadcast() -> broadcast::Sender<StageEvent> {
    let (tx, _rx) = broadcast::channel(STAGE_BROADCAST_CAPACITY);
    let _ = STAGE_SENDER.set(tx);
    stage_broadcast()
}

/// Return a clone of the global stage broadcast sender.
/// Panics if [`init_stage_broadcast`] has not been called yet.
pub fn stage_broadcast() -> broadcast::Sender<StageEvent> {
    STAGE_SENDER
        .get()
        .expect("init_stage_broadcast() must be called before stage_broadcast()")
        .clone()
}

/// Publish a stage event to all broadcast subscribers. Silently
/// drops on send errors (no subscribers, lagged slow consumer).
/// Formats the timestamp lazily — only when there are subscribers,
/// saving the chrono allocation in the common case where the live
/// dashboard is not connected.
pub fn publish_stage_event(mut event: StageEvent) {
    let Some(tx) = STAGE_SENDER.get() else { return };
    // Skip the timestamp formatting when no dashboard is connected.
    // The doc comment always claimed this was optimized away; this
    // implements the optimization.
    if tx.receiver_count() == 0 {
        return;
    }
    event.timestamp = chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string();
    let _ = tx.send(event);
}

/// Initialize BOTH broadcast senders in the canonical order.
/// Idempotent. Returns a clone of the usage sender for callers
/// that want to subscribe immediately (e.g. `AppState`).
pub fn init_all_broadcasts() -> broadcast::Sender<RecentUsageRow> {
    init_stage_broadcast();
    init_usage_broadcast()
}

/// All optional filters shared by the read-side analytics queries.
///
/// Date bounds are ISO-8601 strings (e.g. `2026-01-15T00:00:00Z`) and apply
/// directly to `usage.created_at`. `from` is inclusive; `to` is exclusive.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageFilter {
    /// Inclusive lower bound on `created_at` (ISO-8601).
    pub from: Option<String>,
    /// Exclusive upper bound on `created_at` (ISO-8601).
    pub to: Option<String>,
    pub provider_id: Option<ProviderId>,
    /// Matches `usage.upstream_model_id`.
    pub model_id: Option<String>,
    pub account_id: Option<AccountId>,
    pub combo_id: Option<ComboId>,
    /// Restrict to rows produced under a specific API key. The
    /// per-key usage view (the dashboard's "Usage" tab on the
    /// keys page) sets this; the public-facing analytics endpoints
    /// leave it `None` so the global roll-up is unaffected.
    pub api_key_id: Option<ApiKeyId>,
}

impl UsageFilter {
    /// Returns `true` if no filter is set.
    fn is_empty(&self) -> bool {
        self.from.is_none()
            && self.to.is_none()
            && self.provider_id.is_none()
            && self.model_id.is_none()
            && self.account_id.is_none()
            && self.combo_id.is_none()
            && self.api_key_id.is_none()
    }
}

/// Aggregate roll-up of a filtered set of usage rows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageSummary {
    /// `COUNT(DISTINCT request_id)` — one per logical user request, regardless
    /// of how many race losers it produced.
    pub unique_requests: u64,
    /// `COUNT(*)` — every row, including race losers and retry attempts.
    pub total_rows: u64,
    /// `COUNT(DISTINCT trace_id)` — every per-attempt trace.
    pub total_attempts: u64,
    /// Rows where `race_lost = 0`. Note: non-race rows are also winners.
    pub winners: u64,
    /// Rows where `race_lost = 1`.
    pub losers: u64,
    /// Rows with `status_code >= 400`.
    pub errors: u64,
    pub total_prompt_tokens: i64,
    pub total_completion_tokens: i64,
    pub total_cost_usd: f64,
    /// `AVG(ttft_ms)` over rows where `ttft_ms IS NOT NULL`. `None` when no
    /// such row exists in the filter.
    pub avg_ttft_ms: Option<f64>,
    /// `AVG(total_ms)` over all rows in the filter.
    pub avg_total_ms: f64,
    /// Rows where `cost_usd = 0.0 AND prompt_tokens > 0` — i.e. the row
    /// consumed tokens (so pricing should have applied) but the cost column
    /// is zero, meaning pricing was missing at record time. Surfaces
    /// under-reporting in the dashboard.
    pub rows_with_null_pricing: u64,
}

/// One row of the `by_model` aggregation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ByModelRow {
    pub provider_id: ProviderId,
    pub upstream_model_id: String,
    pub unique_requests: u64,
    pub total_rows: u64,
    pub winners: u64,
    pub total_prompt_tokens: i64,
    pub total_completion_tokens: i64,
    pub total_cost_usd: f64,
}

/// One row of the `by_provider` aggregation.
///
/// Mirrors [`ByModelRow`] but groups by `provider_id` only — the frontend
/// "monthly usage by provider" report uses this for the top-level roll-up,
/// and [`monthly_by_provider`] for the time-bucketed breakdown.
#[derive(Debug, Clone, Serialize)]
pub struct ByProviderRow {
    pub provider_id: String,
    pub unique_requests: u64,
    pub total_rows: u64,
    pub winners: u64,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub total_cost_usd: f64,
}

/// One row of the `monthly_by_provider` aggregation.
///
/// `month` is `strftime('%Y-%m', created_at)` — e.g. `"2026-06"`. Ordered
/// by `month ASC, total_cost_usd DESC` so the frontend can pivot into a
/// providers × months matrix that walks time forward.
#[derive(Debug, Clone, Serialize)]
pub struct MonthlyByProviderRow {
    pub provider_id: String,
    /// `"YYYY-MM"` — the calendar month in UTC.
    pub month: String,
    pub unique_requests: u64,
    pub total_rows: u64,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub total_cost_usd: f64,
}

/// One row of the `by_day` aggregation — daily usage totals for
/// charting. `date` is `"YYYY-MM-DD"` in UTC.
#[derive(Debug, Clone, Serialize)]
pub struct ByDayRow {
    /// `"YYYY-MM-DD"` — the calendar day in UTC.
    pub date: String,
    pub unique_requests: u64,
    pub total_rows: u64,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub total_cost_usd: f64,
    pub errors: u64,
}

/// One row of the `by_account` aggregation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ByAccountRow {
    pub account_id: AccountId,
    pub provider_id: ProviderId,
    pub unique_requests: u64,
    pub total_rows: u64,
    pub errors: u64,
    pub total_cost_usd: f64,
}

/// One row of the `by_status` aggregation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ByStatusRow {
    pub status_code: u16,
    pub count: u64,
}

/// One row of the `errors` query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorRow {
    pub request_id: String,
    pub trace_id: String,
    pub provider_id: ProviderId,
    pub upstream_model_id: String,
    pub status_code: u16,
    /// Pre-redacted error message; secrets already stripped by the writer
    /// ([`crate::cost::redact_error_msg`]).
    pub error_msg_redacted: Option<String>,
    pub created_at: String,
}

// ---------------------------------------------------------------------------
// WHERE-clause builder
// ---------------------------------------------------------------------------

/// A built `(where_sql, params)` pair ready to splice into a query.
///
/// `where_sql` is either the literal string `"WHERE ..."` (with the leading
/// `WHERE` keyword) or the empty string if no filter is set. It is designed
/// to be inserted directly into a SQL template via `format!`.
struct BuiltWhere {
    sql: String,
    params: Vec<Box<dyn ToSql>>,
}

impl BuiltWhere {
    fn from_filter(f: &UsageFilter) -> Self {
        if f.is_empty() {
            return Self { sql: String::new(), params: Vec::new() };
        }

        let mut clauses: Vec<&'static str> = Vec::new();
        let mut params: Vec<Box<dyn ToSql>> = Vec::new();

        if let Some(from) = &f.from {
            clauses.push("created_at >= ?");
            params.push(Box::new(from.clone()));
        }
        if let Some(to) = &f.to {
            clauses.push("created_at < ?");
            params.push(Box::new(to.clone()));
        }
        if let Some(pid) = &f.provider_id {
            clauses.push("provider_id = ?");
            params.push(Box::new(pid.0.clone()));
        }
        if let Some(mid) = &f.model_id {
            clauses.push("upstream_model_id = ?");
            params.push(Box::new(mid.clone()));
        }
        if let Some(aid) = f.account_id {
            clauses.push("account_id = ?");
            params.push(Box::new(aid.0));
        }
        if let Some(cid) = f.combo_id {
            clauses.push("combo_id = ?");
            params.push(Box::new(cid.0));
        }
        if let Some(kid) = f.api_key_id {
            clauses.push("api_key_id = ?");
            params.push(Box::new(kid.0));
        }

        let joined = clauses.join(" AND ");
        // joined is non-empty because is_empty() returned false above.
        let mut sql = String::with_capacity(joined.len() + 7);
        sql.push_str("WHERE ");
        sql.push_str(&joined);
        Self { sql, params }
    }
}

/// Wrapper that lets us call [`params_from_iter`] over a `Vec<Box<dyn ToSql>>`.
///
/// `params_from_iter` needs an `IntoIterator`; the boxed `ToSql` trait objects
/// own their data and implement the trait via `dyn ToSql`, but `params_from_iter`
/// is generic over `IntoIterator` so this pass-through is the cleanest way to
/// keep the call site readable.
fn to_params(v: &[Box<dyn ToSql>]) -> Vec<&dyn ToSql> {
    v.iter().map(|b| b.as_ref() as &dyn ToSql).collect()
}

// ---------------------------------------------------------------------------
// Public queries
// ---------------------------------------------------------------------------

/// Aggregate summary over all rows matching `f`.
pub fn summary(conn: &Connection, f: &UsageFilter) -> Result<UsageSummary> {
    let w = BuiltWhere::from_filter(f);
    let sql = format!(
        "SELECT \
             COUNT(DISTINCT request_id)                                    AS unique_requests, \
             COUNT(*)                                                      AS total_rows, \
             COUNT(DISTINCT trace_id)                                      AS total_attempts, \
             SUM(CASE WHEN race_lost = 0 THEN 1 ELSE 0 END)                AS winners, \
             SUM(CASE WHEN race_lost = 1 THEN 1 ELSE 0 END)                AS losers, \
             SUM(CASE WHEN status_code >= 400 THEN 1 ELSE 0 END)            AS errors, \
             COALESCE(SUM(prompt_tokens), 0)                                AS total_prompt_tokens, \
             COALESCE(SUM(completion_tokens), 0)                            AS total_completion_tokens, \
             COALESCE(SUM(cost_usd), 0.0)                                   AS total_cost_usd, \
             AVG(ttft_ms) FILTER (WHERE ttft_ms IS NOT NULL)               AS avg_ttft_ms, \
             COALESCE(AVG(total_ms), 0.0)                                   AS avg_total_ms, \
             SUM(CASE WHEN cost_usd = 0.0 AND prompt_tokens > 0 THEN 1 ELSE 0 END) AS rows_with_null_pricing \
         FROM usage {}",
        w.sql,
    );

    let mut stmt = conn.prepare(&sql).map_err(|e| CoreError::Database {
        message: format!("prepare usage summary: {}", e),
        source: Some(Box::new(e)),
    })?;

    let params_slice = to_params(&w.params);
    let summary = stmt
        .query_row(params_from_iter(params_slice), |row| {
            let unique_requests: i64 = row.get(0)?;
            let total_rows: i64 = row.get(1)?;
            let total_attempts: i64 = row.get(2)?;
            // SUM(...) over an empty result set yields NULL in SQLite, not 0,
            // so we accept Option<i64> and substitute 0. COALESCE only helps
            // for nullable columns (prompt_tokens, cost_usd), not for the
            // SUM(CASE WHEN...) zero-row edge case.
            let winners: i64 = row.get::<_, Option<i64>>(3)?.unwrap_or(0);
            let losers: i64 = row.get::<_, Option<i64>>(4)?.unwrap_or(0);
            let errors: i64 = row.get::<_, Option<i64>>(5)?.unwrap_or(0);
            let total_prompt_tokens: i64 = row.get(6)?;
            let total_completion_tokens: i64 = row.get(7)?;
            let total_cost_usd: f64 = row.get(8)?;
            let avg_ttft_ms: Option<f64> = row.get(9)?;
            let avg_total_ms: f64 = row.get(10)?;
            let rows_with_null_pricing: i64 = row.get::<_, Option<i64>>(11)?.unwrap_or(0);

            Ok(UsageSummary {
                unique_requests: as_u64(unique_requests, "unique_requests")?,
                total_rows: as_u64(total_rows, "total_rows")?,
                total_attempts: as_u64(total_attempts, "total_attempts")?,
                winners: as_u64(winners, "winners")?,
                losers: as_u64(losers, "losers")?,
                errors: as_u64(errors, "errors")?,
                total_prompt_tokens,
                total_completion_tokens,
                total_cost_usd,
                avg_ttft_ms,
                avg_total_ms,
                rows_with_null_pricing: as_u64(rows_with_null_pricing, "rows_with_null_pricing")?,
            })
        })
        .map_err(|e| CoreError::Database {
            message: format!("query usage summary: {}", e),
            source: Some(Box::new(e)),
        })?;

    Ok(summary)
}

/// Per-(provider, model) breakdown. Ordered by total cost descending.
pub fn by_model(conn: &Connection, f: &UsageFilter) -> Result<Vec<ByModelRow>> {
    let w = BuiltWhere::from_filter(f);
    let sql = format!(
        "SELECT \
             provider_id, \
             upstream_model_id, \
             COUNT(DISTINCT request_id)                       AS unique_requests, \
             COUNT(*)                                         AS total_rows, \
             SUM(CASE WHEN race_lost = 0 THEN 1 ELSE 0 END)   AS winners, \
             COALESCE(SUM(prompt_tokens), 0)                  AS total_prompt_tokens, \
             COALESCE(SUM(completion_tokens), 0)              AS total_completion_tokens, \
             COALESCE(SUM(cost_usd), 0.0)                     AS total_cost_usd \
         FROM usage {} \
         GROUP BY provider_id, upstream_model_id \
         ORDER BY total_cost_usd DESC, provider_id ASC, upstream_model_id ASC",
        w.sql,
    );

    let mut stmt = conn.prepare(&sql).map_err(|e| CoreError::Database {
        message: format!("prepare usage by_model: {}", e),
        source: Some(Box::new(e)),
    })?;

    let params_slice = to_params(&w.params);
    let rows = stmt
        .query_map(params_from_iter(params_slice), |row| {
            let provider_id: String = row.get(0)?;
            let upstream_model_id: String = row.get(1)?;
            let unique_requests: i64 = row.get(2)?;
            let total_rows: i64 = row.get(3)?;
            let winners: i64 = row.get(4)?;
            let total_prompt_tokens: i64 = row.get(5)?;
            let total_completion_tokens: i64 = row.get(6)?;
            let total_cost_usd: f64 = row.get(7)?;

            Ok(ByModelRow {
                provider_id: ProviderId::new(provider_id),
                upstream_model_id,
                unique_requests: as_u64(unique_requests, "unique_requests")?,
                total_rows: as_u64(total_rows, "total_rows")?,
                winners: as_u64(winners, "winners")?,
                total_prompt_tokens,
                total_completion_tokens,
                total_cost_usd,
            })
        })
        .map_err(|e| CoreError::Database {
            message: format!("query usage by_model: {}", e),
            source: Some(Box::new(e)),
        })?;

    collect_rows(rows, "by_model")
}

/// Per-`provider_id` breakdown. Ordered by total cost descending so the
/// biggest spend providers float to the top.
///
/// Mirrors [`by_model`] but groups by `provider_id` only (no
/// `upstream_model_id`). The frontend's "monthly usage by provider" report
/// uses this for the top-level roll-up and [`monthly_by_provider`] for the
/// time-bucketed breakdown.
pub fn by_provider(conn: &Connection, f: &UsageFilter) -> Result<Vec<ByProviderRow>> {
    let w = BuiltWhere::from_filter(f);
    let sql = format!(
        "SELECT \
             provider_id, \
             COUNT(DISTINCT request_id)                       AS unique_requests, \
             COUNT(*)                                         AS total_rows, \
             SUM(CASE WHEN race_lost = 0 THEN 1 ELSE 0 END)   AS winners, \
             COALESCE(SUM(prompt_tokens), 0)                  AS total_prompt_tokens, \
             COALESCE(SUM(completion_tokens), 0)              AS total_completion_tokens, \
             COALESCE(SUM(cost_usd), 0.0)                     AS total_cost_usd \
         FROM usage {} \
         GROUP BY provider_id \
         ORDER BY total_cost_usd DESC, provider_id ASC",
        w.sql,
    );

    let mut stmt = conn.prepare(&sql).map_err(|e| CoreError::Database {
        message: format!("prepare usage by_provider: {}", e),
        source: Some(Box::new(e)),
    })?;

    let params_slice = to_params(&w.params);
    let rows = stmt
        .query_map(params_from_iter(params_slice), |row| {
            let provider_id: String = row.get(0)?;
            let unique_requests: i64 = row.get(1)?;
            let total_rows: i64 = row.get(2)?;
            let winners: i64 = row.get(3)?;
            let total_prompt_tokens: i64 = row.get(4)?;
            let total_completion_tokens: i64 = row.get(5)?;
            let total_cost_usd: f64 = row.get(6)?;

            Ok(ByProviderRow {
                provider_id,
                unique_requests: as_u64(unique_requests, "unique_requests")?,
                total_rows: as_u64(total_rows, "total_rows")?,
                winners: as_u64(winners, "winners")?,
                total_prompt_tokens: as_u64(total_prompt_tokens, "total_prompt_tokens")?,
                total_completion_tokens: as_u64(total_completion_tokens, "total_completion_tokens")?,
                total_cost_usd,
            })
        })
        .map_err(|e| CoreError::Database {
            message: format!("query usage by_provider: {}", e),
            source: Some(Box::new(e)),
        })?;

    collect_rows(rows, "by_provider")
}

/// Per-`(provider_id, month)` breakdown, where `month = strftime('%Y-%m',
/// created_at)`. Ordered by `month ASC, total_cost_usd DESC` so the
/// frontend can pivot into a providers × months matrix that walks time
/// forward and within each month shows the biggest spend provider first.
///
/// `winners` is intentionally omitted — the monthly view is for cost /
/// token tracking, not race outcomes. Use [`by_provider`] if you need
/// winner counts.
pub fn monthly_by_provider(conn: &Connection, f: &UsageFilter) -> Result<Vec<MonthlyByProviderRow>> {
    let w = BuiltWhere::from_filter(f);
    let sql = format!(
        "SELECT \
             provider_id, \
             strftime('%Y-%m', created_at)                     AS month, \
             COUNT(DISTINCT request_id)                       AS unique_requests, \
             COUNT(*)                                         AS total_rows, \
             COALESCE(SUM(prompt_tokens), 0)                  AS total_prompt_tokens, \
             COALESCE(SUM(completion_tokens), 0)              AS total_completion_tokens, \
             COALESCE(SUM(cost_usd), 0.0)                     AS total_cost_usd \
         FROM usage {} \
         GROUP BY provider_id, month \
         ORDER BY month ASC, total_cost_usd DESC, provider_id ASC",
        w.sql,
    );

    let mut stmt = conn.prepare(&sql).map_err(|e| CoreError::Database {
        message: format!("prepare usage monthly_by_provider: {}", e),
        source: Some(Box::new(e)),
    })?;

    let params_slice = to_params(&w.params);
    let rows = stmt
        .query_map(params_from_iter(params_slice), |row| {
            let provider_id: String = row.get(0)?;
            let month: String = row.get(1)?;
            let unique_requests: i64 = row.get(2)?;
            let total_rows: i64 = row.get(3)?;
            let total_prompt_tokens: i64 = row.get(4)?;
            let total_completion_tokens: i64 = row.get(5)?;
            let total_cost_usd: f64 = row.get(6)?;

            Ok(MonthlyByProviderRow {
                provider_id,
                month,
                unique_requests: as_u64(unique_requests, "unique_requests")?,
                total_rows: as_u64(total_rows, "total_rows")?,
                total_prompt_tokens: as_u64(total_prompt_tokens, "total_prompt_tokens")?,
                total_completion_tokens: as_u64(total_completion_tokens, "total_completion_tokens")?,
                total_cost_usd,
            })
        })
        .map_err(|e| CoreError::Database {
            message: format!("query usage monthly_by_provider: {}", e),
            source: Some(Box::new(e)),
        })?;

    collect_rows(rows, "monthly_by_provider")
}

/// Per-day usage totals for charting. Groups by
/// `strftime('%Y-%m-%d', created_at)` and returns rows ordered by
/// date ASC. Each row includes request counts, token totals, cost,
/// and error count (rows with `status_code >= 400`).
pub fn by_day(conn: &Connection, f: &UsageFilter) -> Result<Vec<ByDayRow>> {
    let w = BuiltWhere::from_filter(f);
    let sql = format!(
        "SELECT \
             strftime('%Y-%m-%d', created_at)                     AS date, \
             COUNT(DISTINCT request_id)                           AS unique_requests, \
             COUNT(*)                                             AS total_rows, \
             COALESCE(SUM(prompt_tokens), 0)                      AS total_prompt_tokens, \
             COALESCE(SUM(completion_tokens), 0)                  AS total_completion_tokens, \
             COALESCE(SUM(cost_usd), 0.0)                         AS total_cost_usd, \
             SUM(CASE WHEN status_code >= 400 THEN 1 ELSE 0 END)  AS errors \
         FROM usage {} \
         GROUP BY date \
         ORDER BY date ASC",
        w.sql,
    );

    let mut stmt = conn.prepare(&sql).map_err(|e| CoreError::Database {
        message: format!("prepare usage by_day: {}", e),
        source: Some(Box::new(e)),
    })?;

    let params_slice = to_params(&w.params);
    let rows = stmt
        .query_map(params_from_iter(params_slice), |row| {
            let date: String = row.get(0)?;
            let unique_requests: i64 = row.get(1)?;
            let total_rows: i64 = row.get(2)?;
            let total_prompt_tokens: i64 = row.get(3)?;
            let total_completion_tokens: i64 = row.get(4)?;
            let total_cost_usd: f64 = row.get(5)?;
            let errors: i64 = row.get(6)?;

            Ok(ByDayRow {
                date,
                unique_requests: as_u64(unique_requests, "unique_requests")?,
                total_rows: as_u64(total_rows, "total_rows")?,
                total_prompt_tokens: as_u64(total_prompt_tokens, "total_prompt_tokens")?,
                total_completion_tokens: as_u64(total_completion_tokens, "total_completion_tokens")?,
                total_cost_usd,
                errors: as_u64(errors, "errors")?,
            })
        })
        .map_err(|e| CoreError::Database {
            message: format!("query usage by_day: {}", e),
            source: Some(Box::new(e)),
        })?;

    collect_rows(rows, "by_day")
}

/// Per-(account, provider) breakdown. Ordered by total cost descending.
///
/// Note: an account's `provider_id` is technically redundant in a real schema
/// with a FK on `accounts.provider_id`, but the `usage` table itself doesn't
/// enforce that relationship, so we group by both for correctness and to
/// avoid a join.
pub fn by_account(conn: &Connection, f: &UsageFilter) -> Result<Vec<ByAccountRow>> {
    let w = BuiltWhere::from_filter(f);
    let sql = format!(
        "SELECT \
             account_id, \
             provider_id, \
             COUNT(DISTINCT request_id)                          AS unique_requests, \
             COUNT(*)                                            AS total_rows, \
             SUM(CASE WHEN status_code >= 400 THEN 1 ELSE 0 END) AS errors, \
             COALESCE(SUM(cost_usd), 0.0)                        AS total_cost_usd \
         FROM usage {} \
         GROUP BY account_id, provider_id \
         ORDER BY total_cost_usd DESC, account_id ASC",
        w.sql,
    );

    let mut stmt = conn.prepare(&sql).map_err(|e| CoreError::Database {
        message: format!("prepare usage by_account: {}", e),
        source: Some(Box::new(e)),
    })?;

    let params_slice = to_params(&w.params);
    let rows = stmt
        .query_map(params_from_iter(params_slice), |row| {
            let account_id: Option<i64> = row.get(0)?;
            let provider_id: String = row.get(1)?;
            let unique_requests: i64 = row.get(2)?;
            let total_rows: i64 = row.get(3)?;
            let errors: i64 = row.get(4)?;
            let total_cost_usd: f64 = row.get(5)?;

            // `account_id` is nullable in the schema. The group-by collapses
            // NULLs into a single bucket in SQLite, so an account-less row
            // is possible. Surface it as a stable synthetic id of -1 so
            // downstream JSON has a numeric value, but keep the provider
            // id alongside so callers can disambiguate.
            let aid = account_id.unwrap_or(-1);
            if aid == 0 {
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Integer,
                    Box::new(SimpleErr("account_id must be non-zero".into())),
                ));
            }

            Ok(ByAccountRow {
                account_id: AccountId::new(aid),
                provider_id: ProviderId::new(provider_id),
                unique_requests: as_u64(unique_requests, "unique_requests")?,
                total_rows: as_u64(total_rows, "total_rows")?,
                errors: as_u64(errors, "errors")?,
                total_cost_usd,
            })
        })
        .map_err(|e| CoreError::Database {
            message: format!("query usage by_account: {}", e),
            source: Some(Box::new(e)),
        })?;

    collect_rows(rows, "by_account")
}

/// Per-`status_code` count. Ordered by count descending so the busiest code
/// comes first; ties broken by `status_code` ascending.
pub fn by_status(conn: &Connection, f: &UsageFilter) -> Result<Vec<ByStatusRow>> {
    let w = BuiltWhere::from_filter(f);
    let sql = format!(
        "SELECT status_code, COUNT(*) AS cnt \
         FROM usage {} \
         GROUP BY status_code \
         ORDER BY cnt DESC, status_code ASC",
        w.sql,
    );

    let mut stmt = conn.prepare(&sql).map_err(|e| CoreError::Database {
        message: format!("prepare usage by_status: {}", e),
        source: Some(Box::new(e)),
    })?;

    let params_slice = to_params(&w.params);
    let rows = stmt
        .query_map(params_from_iter(params_slice), |row| {
            let status_code: i64 = row.get(0)?;
            let count: i64 = row.get(1)?;
            if !(0..=u16::MAX as i64).contains(&status_code) {
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Integer,
                    Box::new(SimpleErr(format!("status_code out of u16 range: {}", status_code))),
                ));
            }
            Ok(ByStatusRow {
                status_code: status_code as u16,
                count: as_u64(count, "count")?,
            })
        })
        .map_err(|e| CoreError::Database {
            message: format!("query usage by_status: {}", e),
            source: Some(Box::new(e)),
        })?;

    collect_rows(rows, "by_status")
}

/// Recent error rows (`status_code >= 400`), newest first, capped at `limit`.
///
/// `limit` is a hard cap on the number of returned rows; callers typically
/// pass 100 (matching the spec example) and the value is forwarded verbatim
/// to SQL as an integer parameter.
pub fn errors(conn: &Connection, f: &UsageFilter, limit: u32) -> Result<Vec<ErrorRow>> {
    let w = BuiltWhere::from_filter(f);

    // Build the WHERE clause manually because we need to AND in the
    // status_code predicate that isn't part of `UsageFilter`.
    let mut clauses: Vec<String> = Vec::new();
    if !w.sql.is_empty() {
        // Strip the leading "WHERE " to get bare clauses we can AND with more.
        let bare = w.sql.trim_start_matches("WHERE ").to_string();
        clauses.push(format!("({})", bare));
    }
    clauses.push("status_code >= 400".to_string());
    let where_clause = format!("WHERE {}", clauses.join(" AND "));

    let mut sql = String::new();
    write!(
        &mut sql,
        "SELECT request_id, trace_id, provider_id, upstream_model_id, \
                status_code, error_msg_redacted, created_at \
         FROM usage {} \
         ORDER BY created_at DESC, id DESC \
         LIMIT ?",
        where_clause,
    )
    .expect("writing to String never fails");

    // `limit` is a u32 — well under i64::MAX — so this cast is safe.
    let limit_param: i64 = limit as i64;
    let mut all_params: Vec<Box<dyn ToSql>> = w.params;
    all_params.push(Box::new(limit_param));
    let params_slice = to_params(&all_params);

    let mut stmt = conn.prepare(&sql).map_err(|e| CoreError::Database {
        message: format!("prepare usage errors: {}", e),
        source: Some(Box::new(e)),
    })?;

    let rows = stmt
        .query_map(params_from_iter(params_slice), |row| {
            let request_id: String = row.get(0)?;
            let trace_id: String = row.get(1)?;
            let provider_id: String = row.get(2)?;
            let upstream_model_id: String = row.get(3)?;
            let status_code: i64 = row.get(4)?;
            let error_msg_redacted: Option<String> = row.get(5)?;
            let created_at: String = row.get(6)?;
            if !(0..=u16::MAX as i64).contains(&status_code) {
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    4,
                    rusqlite::types::Type::Integer,
                    Box::new(SimpleErr(format!("status_code out of u16 range: {}", status_code))),
                ));
            }
            Ok(ErrorRow {
                request_id,
                trace_id,
                provider_id: ProviderId::new(provider_id),
                upstream_model_id,
                status_code: status_code as u16,
                error_msg_redacted,
                created_at,
            })
        })
        .map_err(|e| CoreError::Database {
            message: format!("query usage errors: {}", e),
            source: Some(Box::new(e)),
        })?;

    collect_rows(rows, "errors")
}

// ---------------------------------------------------------------------------
// Recent rows (long-polling support)
// ---------------------------------------------------------------------------

/// A single `usage` row, projected for the dashboard's "live tail" view.
///
/// Mirrors the columns the spec calls out for the long-polling feed (and a
/// couple more for convenience). Returned by [`recent`] in
/// `(id ASC)` order, with `id > since_id` so the dashboard can paginate
/// forward by remembering the last seen id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentUsageRow {
    pub id: UsageId,
    pub request_id: String,
    pub trace_id: String,
    pub provider_id: ProviderId,
    pub upstream_model_id: String,
    pub status_code: u16,
    pub total_ms: u64,
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    pub cost_usd: Option<f64>,
    pub connect_ms: Option<u64>,
    pub ttft_ms: Option<u64>,
    pub request_body_json: Option<Value>,
    pub response_body_json: Option<Value>,
    pub request_headers: Option<BTreeMap<String, String>>,
    pub response_headers: Option<BTreeMap<String, String>>,
    pub error_message: Option<String>,
    pub race_total: Option<u8>,
    pub race_attempts: Option<u8>,
    pub is_streaming: bool,
    pub stream_complete: bool,
    pub race_lost: bool,
    /// Upstream stop reason (e.g. "end_turn", "max_tokens",
    /// "stop_sequence" for Anthropic; "stop", "length" for OpenAI).
    pub stop_reason: Option<String>,
    /// Compression savings percentage.
    pub compression_savings_pct: Option<f64>,
    /// Compression techniques (CSV).
    pub compression_techniques: Option<String>,
    pub created_at: String,
}

/// Full `usage` row projection for live-log detail views.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageDetailRow {
    pub id: UsageId,
    pub request_id: String,
    pub trace_id: String,
    pub attempt: i64,
    pub provider_id: ProviderId,
    pub account_id: Option<AccountId>,
    pub combo_id: Option<ComboId>,
    pub combo_target_id: Option<ComboTargetId>,
    pub model_row_id: Option<ModelRowId>,
    pub upstream_model_id: String,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub connect_ms: Option<i64>,
    pub ttft_ms: Option<i64>,
    pub total_ms: i64,
    pub tokens_per_sec: Option<f64>,
    pub status_code: u16,
    pub error_msg: Option<String>,
    pub error_msg_redacted: Option<String>,
    pub request_body_json: Option<Value>,
    pub response_body_json: Option<Value>,
    pub request_headers: Option<BTreeMap<String, String>>,
    pub response_headers: Option<BTreeMap<String, String>>,
    pub error_message: Option<String>,
    pub race_total: i64,
    pub race_attempts: i64,
    pub race_lost: bool,
    pub is_streaming: bool,
    pub stream_complete: bool,
    pub api_key_id: Option<ApiKeyId>,
    pub created_at: String,
}

/// Return up to `limit` usage rows whose `id` is strictly greater than
/// `since_id`, oldest first (so the dashboard can append in order).
///
/// The implementation is the read side of the long-polling feed the
/// dashboard uses in place of an SSE channel: the client passes back the
/// last id it has seen, and we return everything that arrived since.
///
/// `limit` is a hard cap and is forwarded verbatim to SQL.
pub fn recent(conn: &Connection, since_id: i64, limit: u32) -> Result<Vec<RecentUsageRow>> {
    // H1 fix: each retry within a single request writes a
    // separate row to the `usage` table (the per-attempt row is
    // what the operator sees when they want to inspect a
    // particular 5xx/4xx/timeout), but the dashboard groups
    // rows by `request_id` and the cost/token SUMs MUST come
    // from the GROUP BY. We do the aggregation in SQL with
    // scalar fields pulled from the first-attempt row
    // (`MIN(id)`), the SUMs for the per-attempt-additive
    // billing fields, and a deterministic status_code rule:
    //   1. if any attempt is 2xx, take the MIN of the 2xx rows
    //      (i.e. the earliest success);
    //   2. otherwise take the MAX of the failing rows (the
    //      most-recent failure — what the user actually saw).
    // The `id` returned to the dashboard is the first attempt's
    // id so long-polling `since_id` continues to work.
    let limit_param: i64 = limit as i64;
    let mut stmt = conn
        .prepare(
            "WITH grouped AS ( \
                 SELECT request_id, \
                        MIN(id)         AS agg_id, \
                        MIN(created_at) AS agg_created_at, \
                        COALESCE( \
                            (SELECT u2.status_code FROM usage u2 \
                             WHERE u2.request_id = usage.request_id \
                               AND u2.status_code BETWEEN 200 AND 299 \
                             ORDER BY u2.id ASC LIMIT 1), \
                            (SELECT MAX(u3.status_code) FROM usage u3 \
                             WHERE u3.request_id = usage.request_id) \
                        ) AS agg_status_code, \
                        MAX(total_ms)   AS agg_total_ms, \
                        SUM(COALESCE(prompt_tokens, 0))     AS agg_prompt_tokens, \
                        SUM(COALESCE(completion_tokens, 0)) AS agg_completion_tokens, \
                        SUM(COALESCE(cost_usd, 0.0))        AS agg_cost_usd, \
                        MAX(connect_ms) AS agg_connect_ms, \
                        MAX(ttft_ms)    AS agg_ttft_ms, \
                        MAX(race_total) AS agg_race_total, \
                        MAX(race_attempts) AS agg_race_attempts, \
                        MAX(is_streaming)  AS agg_is_streaming, \
                        MAX(stream_complete) AS agg_stream_complete, \
                        MAX(race_lost) AS agg_race_lost, \
                        MAX(compression_savings_pct) AS agg_compression_savings_pct, \
                        MAX(compression_techniques) AS agg_compression_techniques \
                 FROM usage \
                 WHERE id > ?1 \
                 GROUP BY request_id \
             ) \
             SELECT g.agg_id, u.request_id, u.trace_id, u.provider_id, u.upstream_model_id, \
                    g.agg_status_code, g.agg_total_ms, g.agg_prompt_tokens, g.agg_completion_tokens, \
                    g.agg_cost_usd, g.agg_connect_ms, g.agg_ttft_ms, u.request_body_json, u.response_body_json, \
                    u.request_headers, u.response_headers, u.error_msg_redacted, u.error_msg, \
                    g.agg_race_total, g.agg_race_attempts, g.agg_is_streaming, g.agg_stream_complete, \
                    g.agg_race_lost, g.agg_created_at, u.stop_reason, \
                    g.agg_compression_savings_pct, g.agg_compression_techniques \
             FROM grouped g \
             JOIN usage u ON u.id = g.agg_id \
             ORDER BY g.agg_id ASC \
             LIMIT ?2",
        )
        .map_err(|e| CoreError::Database {
            message: format!("prepare usage recent: {}", e),
            source: Some(Box::new(e)),
        })?;

    let rows = stmt
        .query_map(params![since_id, limit_param], |row| {
            let mut col_idx = 0;
            let id: i64 = row.get(col_idx)?; col_idx += 1;
            let request_id: String = row.get(col_idx)?; col_idx += 1;
            let trace_id: String = row.get(col_idx)?; col_idx += 1;
            let provider_id: String = row.get(col_idx)?; col_idx += 1;
            let upstream_model_id: String = row.get(col_idx)?; col_idx += 1;
            let status_code: i64 = row.get(col_idx)?; col_idx += 1;
            let total_ms: i64 = row.get(col_idx)?; col_idx += 1;
            let prompt_tokens: Option<i64> = row.get(col_idx)?; col_idx += 1;
            let completion_tokens: Option<i64> = row.get(col_idx)?; col_idx += 1;
            let cost_usd: Option<f64> = row.get(col_idx)?; col_idx += 1;
            let connect_ms: Option<i64> = row.get(col_idx)?; col_idx += 1;
            let ttft_ms: Option<i64> = row.get(col_idx)?; col_idx += 1;
            let request_body_json: Option<serde_json::Value> = row.get::<_, Option<String>>(col_idx)?.and_then(|s| serde_json::from_str(&s).ok()); col_idx += 1;
            let response_body_json: Option<serde_json::Value> = row.get::<_, Option<String>>(col_idx)?.and_then(|s| serde_json::from_str(&s).ok()); col_idx += 1;
            let request_headers: Option<String> = row.get(col_idx)?; col_idx += 1;
            let response_headers: Option<String> = row.get(col_idx)?; col_idx += 1;
            let error_msg_redacted: Option<String> = row.get(col_idx)?; col_idx += 1;
            let error_msg: Option<String> = row.get(col_idx)?; col_idx += 1;
            let race_total: i64 = row.get(col_idx)?; col_idx += 1;
            let race_attempts: i64 = row.get(col_idx)?; col_idx += 1;
            let is_streaming: i64 = row.get(col_idx)?; col_idx += 1;
            let stream_complete: i64 = row.get(col_idx)?; col_idx += 1;
            let race_lost: i64 = row.get(col_idx)?; col_idx += 1;
            let created_at: String = row.get(col_idx)?; col_idx += 1;
            let stop_reason: Option<String> = row.get(col_idx)?; col_idx += 1;
            let compression_savings_pct: Option<f64> = row.get(col_idx)?; col_idx += 1;
            let compression_techniques: Option<String> = row.get(col_idx)?;

            if !(0..=u16::MAX as i64).contains(&status_code) {
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    5,
                    rusqlite::types::Type::Integer,
                    Box::new(SimpleErr(format!("status_code out of u16 range: {}", status_code))),
                ));
            }
            if total_ms < 0 {
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    6,
                    rusqlite::types::Type::Integer,
                    Box::new(SimpleErr(format!("total_ms unexpectedly negative: {}", total_ms))),
                ));
            }
            let request_headers =
                request_headers.and_then(|s| serde_json::from_str(&s).ok());
            let response_headers =
                response_headers.and_then(|s| serde_json::from_str(&s).ok());
            let error_message = error_msg_redacted.or(error_msg);
            let prompt_tokens = prompt_tokens.and_then(|v| u32::try_from(v).ok());
            let completion_tokens = completion_tokens.and_then(|v| u32::try_from(v).ok());
            let race_total_u8 = u8::try_from(race_total).ok();
            let race_attempts_u8 = u8::try_from(race_attempts).ok();
            let is_streaming_bool = is_streaming != 0;
            let stream_complete_bool = stream_complete != 0;
            Ok(RecentUsageRow {
                id: UsageId(id),
                request_id,
                trace_id,
                provider_id: ProviderId::new(provider_id),
                upstream_model_id,
                status_code: status_code as u16,
                total_ms: total_ms as u64,
                prompt_tokens,
                completion_tokens,
                cost_usd,
                connect_ms: connect_ms.map(|v| v as u64),
                ttft_ms: ttft_ms.map(|v| v as u64),
                request_body_json,
                response_body_json,
                request_headers,
                response_headers,
                error_message,
                race_total: race_total_u8,
                race_attempts: race_attempts_u8,
                is_streaming: is_streaming_bool,
                stream_complete: stream_complete_bool,
                race_lost: race_lost != 0,
                stop_reason,
                compression_savings_pct,
                compression_techniques,
                created_at,
            })
        })
        .map_err(|e| CoreError::Database {
            message: format!("query usage recent: {}", e),
            source: Some(Box::new(e)),
        })?;

    collect_rows(rows, "recent")
}

/// Return the most recent `limit` usage rows, newest first.
///
/// H1 fix: this mirrors the per-request aggregation in
/// `recent()` so the dashboard sees the same grouped
/// cost/token SUMs whether it's polling (`recent`) or
/// fetching the head (`recent_desc`). Without this, the
/// same 3-attempt request that `recent()` shows as 0.03
/// cost would show as 0.01 in the admin table at the top.
pub fn recent_desc(conn: &Connection, limit: u32) -> Result<Vec<RecentUsageRow>> {
    let limit_param: i64 = limit as i64;
    let mut stmt = conn
        .prepare(
            "WITH grouped AS ( \
                 SELECT request_id, \
                        MIN(id)         AS agg_id, \
                        MIN(created_at) AS agg_created_at, \
                        COALESCE( \
                            (SELECT u2.status_code FROM usage u2 \
                             WHERE u2.request_id = usage.request_id \
                               AND u2.status_code BETWEEN 200 AND 299 \
                             ORDER BY u2.id ASC LIMIT 1), \
                            (SELECT MAX(u3.status_code) FROM usage u3 \
                             WHERE u3.request_id = usage.request_id) \
                        ) AS agg_status_code, \
                        MAX(total_ms)   AS agg_total_ms, \
                        SUM(COALESCE(prompt_tokens, 0))     AS agg_prompt_tokens, \
                        SUM(COALESCE(completion_tokens, 0)) AS agg_completion_tokens, \
                        SUM(COALESCE(cost_usd, 0.0))        AS agg_cost_usd, \
                        MAX(connect_ms) AS agg_connect_ms, \
                        MAX(ttft_ms)    AS agg_ttft_ms, \
                        MAX(race_total) AS agg_race_total, \
                        MAX(race_attempts) AS agg_race_attempts, \
                        MAX(is_streaming)  AS agg_is_streaming, \
                        MAX(stream_complete) AS agg_stream_complete, \
                        MAX(race_lost) AS agg_race_lost, \
                        MAX(compression_savings_pct) AS agg_compression_savings_pct, \
                        MAX(compression_techniques) AS agg_compression_techniques \
                 FROM usage \
                 GROUP BY request_id \
             ) \
             SELECT g.agg_id, u.request_id, u.trace_id, u.provider_id, u.upstream_model_id, \
                    g.agg_status_code, g.agg_total_ms, g.agg_prompt_tokens, g.agg_completion_tokens, \
                    g.agg_cost_usd, g.agg_connect_ms, g.agg_ttft_ms, u.request_body_json, u.response_body_json, \
                    u.request_headers, u.response_headers, u.error_msg_redacted, u.error_msg, \
                    g.agg_race_total, g.agg_race_attempts, g.agg_is_streaming, g.agg_stream_complete, \
                    g.agg_race_lost, g.agg_created_at, u.stop_reason, \
                    g.agg_compression_savings_pct, g.agg_compression_techniques \
             FROM grouped g \
             JOIN usage u ON u.id = g.agg_id \
             ORDER BY g.agg_id DESC \
             LIMIT ?1",
        )
        .map_err(|e| CoreError::Database {
            message: format!("prepare usage recent_desc: {}", e),
            source: Some(Box::new(e)),
        })?;

    let rows = stmt
        .query_map(params![limit_param], |row| {
            let mut col_idx = 0;
            let id: i64 = row.get(col_idx)?; col_idx += 1;
            let request_id: String = row.get(col_idx)?; col_idx += 1;
            let trace_id: String = row.get(col_idx)?; col_idx += 1;
            let provider_id: String = row.get(col_idx)?; col_idx += 1;
            let upstream_model_id: String = row.get(col_idx)?; col_idx += 1;
            let status_code: i64 = row.get(col_idx)?; col_idx += 1;
            let total_ms: i64 = row.get(col_idx)?; col_idx += 1;
            let prompt_tokens: Option<i64> = row.get(col_idx)?; col_idx += 1;
            let completion_tokens: Option<i64> = row.get(col_idx)?; col_idx += 1;
            let cost_usd: Option<f64> = row.get(col_idx)?; col_idx += 1;
            let connect_ms: Option<i64> = row.get(col_idx)?; col_idx += 1;
            let ttft_ms: Option<i64> = row.get(col_idx)?; col_idx += 1;
            let request_body_json: Option<serde_json::Value> = row.get::<_, Option<String>>(col_idx)?.and_then(|s| serde_json::from_str(&s).ok()); col_idx += 1;
            let response_body_json: Option<serde_json::Value> = row.get::<_, Option<String>>(col_idx)?.and_then(|s| serde_json::from_str(&s).ok()); col_idx += 1;
            let request_headers: Option<String> = row.get(col_idx)?; col_idx += 1;
            let response_headers: Option<String> = row.get(col_idx)?; col_idx += 1;
            let error_msg_redacted: Option<String> = row.get(col_idx)?; col_idx += 1;
            let error_msg: Option<String> = row.get(col_idx)?; col_idx += 1;
            let race_total: i64 = row.get(col_idx)?; col_idx += 1;
            let race_attempts: i64 = row.get(col_idx)?; col_idx += 1;
            let is_streaming: i64 = row.get(col_idx)?; col_idx += 1;
            let stream_complete: i64 = row.get(col_idx)?; col_idx += 1;
            let race_lost: i64 = row.get(col_idx)?; col_idx += 1;
            let created_at: String = row.get(col_idx)?; col_idx += 1;
            let stop_reason: Option<String> = row.get(col_idx)?; col_idx += 1;
            let compression_savings_pct: Option<f64> = row.get(col_idx)?; col_idx += 1;
            let compression_techniques: Option<String> = row.get(col_idx)?;

            if !(0..=u16::MAX as i64).contains(&status_code) {
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    5,
                    rusqlite::types::Type::Integer,
                    Box::new(SimpleErr(format!("status_code out of u16 range: {}", status_code))),
                ));
            }
            if total_ms < 0 {
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    6,
                    rusqlite::types::Type::Integer,
                    Box::new(SimpleErr(format!("total_ms unexpectedly negative: {}", total_ms))),
                ));
            }
            let request_headers =
                request_headers.and_then(|s| serde_json::from_str(&s).ok());
            let response_headers =
                response_headers.and_then(|s| serde_json::from_str(&s).ok());
            let error_message = error_msg_redacted.or(error_msg);
            let prompt_tokens = prompt_tokens.and_then(|v| u32::try_from(v).ok());
            let completion_tokens = completion_tokens.and_then(|v| u32::try_from(v).ok());
            let race_total_u8 = u8::try_from(race_total).ok();
            let race_attempts_u8 = u8::try_from(race_attempts).ok();
            let is_streaming_bool = is_streaming != 0;
            let stream_complete_bool = stream_complete != 0;

            Ok(RecentUsageRow {
                id: UsageId(id),
                request_id,
                trace_id,
                provider_id: ProviderId::new(provider_id),
                upstream_model_id,
                status_code: status_code as u16,
                total_ms: total_ms as u64,
                prompt_tokens,
                completion_tokens,
                cost_usd,
                connect_ms: connect_ms.map(|v| v as u64),
                ttft_ms: ttft_ms.map(|v| v as u64),
                request_body_json,
                response_body_json,
                request_headers,
                response_headers,
                error_message,
                race_total: race_total_u8,
                race_attempts: race_attempts_u8,
                is_streaming: is_streaming_bool,
                stream_complete: stream_complete_bool,
                race_lost: race_lost != 0,
                stop_reason,
                compression_savings_pct,
                compression_techniques,
                created_at,
            })
        })
        .map_err(|e| CoreError::Database {
            message: format!("query usage recent_desc: {}", e),
            source: Some(Box::new(e)),
        })?;

    collect_rows(rows, "recent_desc")
}

/// Return one full `usage` row by id.
pub fn detail_by_id(conn: &Connection, id: i64) -> Result<Option<UsageDetailRow>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, request_id, trace_id, attempt, provider_id, account_id, \
                    combo_id, combo_target_id, model_row_id, upstream_model_id, \
                    prompt_tokens, completion_tokens, connect_ms, ttft_ms, \
                    total_ms, tokens_per_sec, status_code, error_msg, \
                    error_msg_redacted, race_total, race_attempts, race_lost, \
                    api_key_id, created_at, is_streaming, stream_complete, \
                    request_body_json, response_body_json, request_headers, \
                    response_headers, error_message \
             FROM usage \
             WHERE id = ?1",
        )
        .map_err(|e| CoreError::Database {
            message: format!("prepare usage detail_by_id: {}", e),
            source: Some(Box::new(e)),
        })?;

    let row = stmt
        .query_row(params![id], |row| {
            let id: i64 = row.get(0)?;
            let request_id: String = row.get(1)?;
            let trace_id: String = row.get(2)?;
            let attempt: i64 = row.get(3)?;
            let provider_id: String = row.get(4)?;
            let account_id: Option<i64> = row.get(5)?;
            let combo_id: Option<i64> = row.get(6)?;
            let combo_target_id: Option<i64> = row.get(7)?;
            let model_row_id: Option<i64> = row.get(8)?;
            let upstream_model_id: String = row.get(9)?;
            let prompt_tokens: Option<i64> = row.get(10)?;
            let completion_tokens: Option<i64> = row.get(11)?;
            let connect_ms: Option<i64> = row.get(12)?;
            let ttft_ms: Option<i64> = row.get(13)?;
            let total_ms: i64 = row.get(14)?;
            let tokens_per_sec: Option<f64> = row.get(15)?;
            let status_code: i64 = row.get(16)?;
            let error_msg: Option<String> = row.get(17)?;
            let error_msg_redacted: Option<String> = row.get(18)?;
            let race_total: i64 = row.get(19)?;
            let race_attempts: i64 = row.get(20)?;
            let race_lost: i64 = row.get(21)?;
            let api_key_id: Option<i64> = row.get(22)?;
            let created_at: String = row.get(23)?;
            let is_streaming: i64 = row.get(24)?;
            let stream_complete: i64 = row.get(25)?;
            let mut col_idx = 26;
            let request_body_json: Option<serde_json::Value> = row.get::<_, Option<String>>(col_idx)?.and_then(|s| serde_json::from_str(&s).ok()); col_idx += 1;
            let response_body_json: Option<serde_json::Value> = row.get::<_, Option<String>>(col_idx)?.and_then(|s| serde_json::from_str(&s).ok()); col_idx += 1;
            let request_headers: Option<String> = row.get(col_idx)?; col_idx += 1;
            let response_headers: Option<String> = row.get(col_idx)?; col_idx += 1;
            let error_message: Option<String> = row.get(col_idx)?;

            if !(0..=u16::MAX as i64).contains(&status_code) {
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    16,
                    rusqlite::types::Type::Integer,
                    Box::new(SimpleErr(format!("status_code out of u16 range: {}", status_code))),
                ));
            }
            if total_ms < 0 {
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    14,
                    rusqlite::types::Type::Integer,
                    Box::new(SimpleErr(format!("total_ms unexpectedly negative: {}", total_ms))),
                ));
            }
            let request_headers =
                request_headers.and_then(|s| serde_json::from_str(&s).ok());
            let response_headers =
                response_headers.and_then(|s| serde_json::from_str(&s).ok());

            Ok(UsageDetailRow {
                id: UsageId(id),
                request_id,
                trace_id,
                attempt,
                provider_id: ProviderId::new(provider_id),
                account_id: account_id.map(AccountId),
                combo_id: combo_id.map(ComboId),
                combo_target_id: combo_target_id.map(ComboTargetId),
                model_row_id: model_row_id.map(ModelRowId),
                upstream_model_id,
                prompt_tokens,
                completion_tokens,
                connect_ms,
                ttft_ms,
                total_ms,
                tokens_per_sec,
                status_code: status_code as u16,
                error_msg,
                error_msg_redacted,
                request_body_json,
                response_body_json,
                request_headers,
                response_headers,
                race_total,
                race_attempts,
                race_lost: race_lost != 0,
                is_streaming: is_streaming != 0,
                stream_complete: stream_complete != 0,
                created_at,
                api_key_id: api_key_id.map(ApiKeyId),
                error_message,
            })
        })
        .optional()
        .map_err(|e| CoreError::Database {
            message: format!("query usage detail_by_id: {}", e),
            source: Some(Box::new(e)),
        })?;

    Ok(row)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Drain a `MappedRows` iterator into a `Vec`, converting each row's rusqlite
/// error into a `CoreError::Database` with a query-specific message.
fn collect_rows<T>(
    iter: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>>,
    query_name: &'static str,
) -> Result<Vec<T>> {
    let mut out = Vec::new();
    for r in iter {
        out.push(r.map_err(|e| CoreError::Database {
            message: format!("read {} row: {}", query_name, e),
            source: Some(Box::new(e)),
        })?);
    }
    Ok(out)
}

/// Coerce a non-negative `i64` (from `COUNT(*)` etc.) to `u64`, panicking on
/// the unreachable negative case with a clear field name.
fn as_u64(v: i64, field: &'static str) -> rusqlite::Result<u64> {
    if v < 0 {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Integer,
            Box::new(SimpleErr(format!("{} unexpectedly negative: {}", field, v))),
        ));
    }
    Ok(v as u64)
}

/// Tiny `std::error::Error` shim for ad-hoc conversion errors.
#[derive(Debug)]
struct SimpleErr(String);
impl std::fmt::Display for SimpleErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for SimpleErr {}

// ---------------------------------------------------------------------------
// Recording TTL cleanup
// ---------------------------------------------------------------------------

/// Clear recorded request/response bodies and headers once they are older than
/// the configured TTL. Metadata rows are preserved for analytics; only the
/// heavyweight live-log detail fields are expired.
pub fn prune_expired_recording_bodies(conn: &Connection, ttl_secs: i64) -> Result<usize> {
    let ttl_secs = ttl_secs.max(0);
    let n = conn
        .execute(
            "UPDATE usage \
             SET request_body_json = NULL, \
                 response_body_json = NULL, \
                 request_headers = NULL, \
                 response_headers = NULL \
             WHERE datetime(created_at) <= datetime(?1, ?2)",
            params![chrono::Utc::now().to_rfc3339(), format!("-{} seconds", ttl_secs)],
        )
        .map_err(|e| CoreError::Database {
            message: format!("prune_expired_recording_bodies: {}", e),
            source: Some(Box::new(e)),
        })?;
    Ok(n)
}

/// Delete usage rows (live logs) older than the configured TTL. Unlike
/// `prune_expired_recording_bodies` which only nullifies heavy columns,
/// this function removes the entire row — metadata and all — so the
/// live-logs table does not grow indefinitely. Called on a 60s
/// background loop AND once at startup so a service restart combined
/// with the configured TTL gives the operator a clean slate.
pub fn prune_expired_usage_rows(conn: &Connection, ttl_secs: i64) -> Result<usize> {
    let ttl_secs = ttl_secs.max(0);
    let n = conn
        .execute(
            "DELETE FROM usage \
             WHERE datetime(created_at) <= datetime(?1, ?2)",
            params![chrono::Utc::now().to_rfc3339(), format!("-{} seconds", ttl_secs)],
        )
        .map_err(|e| CoreError::Database {
            message: format!("prune_expired_usage_rows: {}", e),
            source: Some(Box::new(e)),
        })?;
    Ok(n)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;
    use crate::ids::{RequestId, TraceId};
    use rusqlite::Connection;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = base.join(format!("openproxy-usage-test-{}-{}", std::process::id(), nanos));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    /// Build a fresh in-memory-style DB on disk, run migrations, and return
    /// the connection plus a cleanup closure (we use a file because rusqlite
    /// doesn't expose `:memory:` across `&Connection` borrows without
    /// `OpenFlags::SQLITE_OPEN_URI`).
    fn fresh_conn() -> (Connection, PathBuf) {
        let dir = tempdir();
        let path = dir.join("usage-test.db");
        let mut conn = Connection::open(&path).expect("open");
        migrations::run(&mut conn).expect("migrate");
        (conn, path)
    }

    /// Insert one usage row with all defaults driven by the test fixture.
    /// Counts start at 0/200ms ttft/1200ms total to make aggregate assertions
    /// easy to write by inspection.
    fn insert(
        conn: &Connection,
        request_id: &str,
        trace_id: &str,
        provider: &str,
        model: &str,
        account: Option<i64>,
        status: u16,
        prompt: i64,
        completion: i64,
        cost: f64,
        ttft: Option<i64>,
        total: i64,
        race_lost: bool,
        err: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO usage (\
                request_id, trace_id, attempt, provider_id, account_id, \
                upstream_model_id, prompt_tokens, completion_tokens, cost_usd, \
                connect_ms, ttft_ms, total_ms, status_code, error_msg, \
                error_msg_redacted, race_total, race_lost, created_at\
             ) VALUES (\
                ?1, ?2, 1, ?3, ?4, ?5, ?6, ?7, ?8, 50, ?9, ?10, ?11, ?12, ?12, 1, ?13, datetime('now')\
             )",
            params![
                request_id,
                trace_id,
                provider,
                account,
                model,
                prompt,
                completion,
                cost,
                ttft,
                total,
                status as i64,
                err,
                race_lost as i64,
            ],
        )
        .expect("insert");
    }

    // -----------------------------------------------------------------------
    // 1. summary_basic — 3 rows sharing one request_id, 1 winner + 2 losers.
    // -----------------------------------------------------------------------
    #[test]
    fn summary_basic() {
        let (conn, _p) = fresh_conn();
        let req = RequestId::new().to_string();
        let t1 = TraceId::new().to_string();
        let t2 = TraceId::new().to_string();
        let t3 = TraceId::new().to_string();
        insert(&conn, &req, &t1, "openrouter", "openai/gpt-4o", Some(1), 200, 100, 50, 0.01, Some(200), 1200, false, None);
        insert(&conn, &req, &t2, "openrouter", "openai/gpt-4o", Some(2), 200, 100, 50, 0.01, Some(200), 1200, true, None);
        insert(&conn, &req, &t3, "openrouter", "openai/gpt-4o", Some(3), 200, 100, 50, 0.01, Some(200), 1200, true, None);

        let s = summary(&conn, &UsageFilter::default()).expect("summary");
        assert_eq!(s.unique_requests, 1, "three rows share one request_id");
        assert_eq!(s.total_rows, 3);
        assert_eq!(s.total_attempts, 3, "three distinct trace_ids");
        assert_eq!(s.winners, 1);
        assert_eq!(s.losers, 2);
        assert_eq!(s.errors, 0);
        assert_eq!(s.total_prompt_tokens, 300);
        assert_eq!(s.total_completion_tokens, 150);
        assert!((s.total_cost_usd - 0.03).abs() < 1e-9);
        assert_eq!(s.avg_ttft_ms, Some(200.0));
        assert!((s.avg_total_ms - 1200.0).abs() < 1e-9);
    }

    // -----------------------------------------------------------------------
    // 2. summary_with_provider_filter
    // -----------------------------------------------------------------------
    #[test]
    fn summary_with_provider_filter() {
        let (conn, _p) = fresh_conn();
        // 2 rows for openrouter, 1 row for anthropic.
        let r1 = RequestId::new().to_string();
        let r2 = RequestId::new().to_string();
        let r3 = RequestId::new().to_string();
        insert(&conn, &r1, &TraceId::new().to_string(), "openrouter", "openai/gpt-4o", Some(1), 200, 10, 5, 0.001, Some(100), 600, false, None);
        insert(&conn, &r2, &TraceId::new().to_string(), "openrouter", "openai/gpt-4o", Some(2), 200, 10, 5, 0.001, Some(100), 600, false, None);
        insert(&conn, &r3, &TraceId::new().to_string(), "anthropic", "claude-3.5-sonnet", Some(3), 200, 10, 5, 0.001, Some(100), 600, false, None);

        let f = UsageFilter { provider_id: Some(ProviderId::new("openrouter")), ..Default::default() };
        let s = summary(&conn, &f).expect("filtered summary");
        assert_eq!(s.unique_requests, 2);
        assert_eq!(s.total_rows, 2);
        assert_eq!(s.total_cost_usd, 0.002);

        let f_none = UsageFilter::default();
        let s_all = summary(&conn, &f_none).expect("unfiltered summary");
        assert_eq!(s_all.unique_requests, 3);
        assert_eq!(s_all.total_rows, 3);
    }

    // -----------------------------------------------------------------------
    // 3. by_model_groups_by_provider_and_model
    // -----------------------------------------------------------------------
    #[test]
    fn by_model_groups_by_provider_and_model() {
        let (conn, _p) = fresh_conn();
        // gpt-4o x2, gpt-4o-mini x1, claude x1
        let r1 = RequestId::new().to_string();
        let r2 = RequestId::new().to_string();
        let r3 = RequestId::new().to_string();
        let r4 = RequestId::new().to_string();
        insert(&conn, &r1, &TraceId::new().to_string(), "openrouter", "openai/gpt-4o", Some(1), 200, 100, 50, 0.5, Some(200), 1200, false, None);
        insert(&conn, &r2, &TraceId::new().to_string(), "openrouter", "openai/gpt-4o", Some(1), 200, 100, 50, 0.5, Some(200), 1200, false, None);
        insert(&conn, &r3, &TraceId::new().to_string(), "openrouter", "openai/gpt-4o-mini", Some(1), 200, 100, 50, 0.05, Some(200), 600, false, None);
        insert(&conn, &r4, &TraceId::new().to_string(), "anthropic", "claude-3.5-sonnet", Some(2), 200, 100, 50, 1.0, Some(200), 1500, false, None);

        let rows = by_model(&conn, &UsageFilter::default()).expect("by_model");
        assert_eq!(rows.len(), 3, "three (provider, model) buckets");

        // Order: total_cost_usd DESC. Costs are 1.0, 1.0, 0.05.
        assert_eq!(rows[0].upstream_model_id, "claude-3.5-sonnet");
        assert!((rows[0].total_cost_usd - 1.0).abs() < 1e-9);
        assert_eq!(rows[0].unique_requests, 1);
        assert_eq!(rows[0].winners, 1);

        // openai/gpt-4o cost 1.0 (0.5 + 0.5) — second by cost.
        let gpt = rows.iter().find(|r| r.upstream_model_id == "openai/gpt-4o").expect("gpt row");
        assert_eq!(gpt.unique_requests, 2);
        assert_eq!(gpt.total_rows, 2);
        assert!((gpt.total_cost_usd - 1.0).abs() < 1e-9);
        assert_eq!(gpt.provider_id, ProviderId::new("openrouter"));

        let mini = rows.iter().find(|r| r.upstream_model_id == "openai/gpt-4o-mini").expect("mini row");
        assert_eq!(mini.total_rows, 1);
        assert!((mini.total_cost_usd - 0.05).abs() < 1e-9);
    }

    // -----------------------------------------------------------------------
    // 4. by_account_groups_by_account
    // -----------------------------------------------------------------------
    #[test]
    fn by_account_groups_by_account() {
        let (conn, _p) = fresh_conn();
        // account 1: 2 successes
        // account 2: 1 success, 1 error
        let r1 = RequestId::new().to_string();
        let r2 = RequestId::new().to_string();
        let r3 = RequestId::new().to_string();
        let r4 = RequestId::new().to_string();
        insert(&conn, &r1, &TraceId::new().to_string(), "openrouter", "openai/gpt-4o", Some(1), 200, 10, 5, 0.10, Some(100), 600, false, None);
        insert(&conn, &r2, &TraceId::new().to_string(), "openrouter", "openai/gpt-4o", Some(1), 200, 10, 5, 0.10, Some(100), 600, false, None);
        insert(&conn, &r3, &TraceId::new().to_string(), "openrouter", "openai/gpt-4o", Some(2), 200, 10, 5, 0.05, Some(100), 600, false, None);
        insert(&conn, &r4, &TraceId::new().to_string(), "openrouter", "openai/gpt-4o", Some(2), 500, 10, 5, 0.0, Some(100), 600, false, Some("upstream 500"));

        let rows = by_account(&conn, &UsageFilter::default()).expect("by_account");
        assert_eq!(rows.len(), 2);

        // Ordered by total_cost_usd DESC: acc 1 (0.20) before acc 2 (0.05).
        assert_eq!(rows[0].account_id, AccountId::new(1));
        assert_eq!(rows[0].total_rows, 2);
        assert_eq!(rows[0].errors, 0);
        assert_eq!(rows[0].unique_requests, 2);
        assert!((rows[0].total_cost_usd - 0.20).abs() < 1e-9);
        assert_eq!(rows[0].provider_id, ProviderId::new("openrouter"));

        assert_eq!(rows[1].account_id, AccountId::new(2));
        assert_eq!(rows[1].total_rows, 2);
        assert_eq!(rows[1].errors, 1);
        assert!((rows[1].total_cost_usd - 0.05).abs() < 1e-9);
    }

    // -----------------------------------------------------------------------
    // 5. by_status_groups_by_code
    // -----------------------------------------------------------------------
    #[test]
    fn by_status_groups_by_code() {
        let (conn, _p) = fresh_conn();
        // 3x 200, 2x 429, 1x 500
        for _ in 0..3 {
            insert(&conn, &RequestId::new().to_string(), &TraceId::new().to_string(),
                "openrouter", "openai/gpt-4o", Some(1), 200, 10, 5, 0.01, Some(100), 600, false, None);
        }
        for _ in 0..2 {
            insert(&conn, &RequestId::new().to_string(), &TraceId::new().to_string(),
                "openrouter", "openai/gpt-4o", Some(1), 429, 10, 5, 0.0, Some(100), 600, false, Some("rate limited"));
        }
        insert(&conn, &RequestId::new().to_string(), &TraceId::new().to_string(),
            "openrouter", "openai/gpt-4o", Some(1), 500, 10, 5, 0.0, Some(100), 600, false, Some("oops"));

        let rows = by_status(&conn, &UsageFilter::default()).expect("by_status");
        assert_eq!(rows.len(), 3);
        // Ordered by count DESC: 200 (3) first, then 429 (2), then 500 (1).
        assert_eq!(rows[0].status_code, 200);
        assert_eq!(rows[0].count, 3);
        assert_eq!(rows[1].status_code, 429);
        assert_eq!(rows[1].count, 2);
        assert_eq!(rows[2].status_code, 500);
        assert_eq!(rows[2].count, 1);
    }

    // -----------------------------------------------------------------------
    // 6. errors_returns_only_4xx_5xx
    // -----------------------------------------------------------------------
    #[test]
    fn errors_returns_only_4xx_5xx() {
        let (conn, _p) = fresh_conn();
        // 1 success
        insert(&conn, &RequestId::new().to_string(), &TraceId::new().to_string(),
            "openrouter", "openai/gpt-4o", Some(1), 200, 10, 5, 0.01, Some(100), 600, false, None);
        // 1 400, 1 502
        insert(&conn, &RequestId::new().to_string(), &TraceId::new().to_string(),
            "openrouter", "openai/gpt-4o", Some(1), 400, 10, 5, 0.0, Some(100), 600, false, Some("bad request"));
        insert(&conn, &RequestId::new().to_string(), &TraceId::new().to_string(),
            "openrouter", "openai/gpt-4o", Some(1), 502, 10, 5, 0.0, Some(100), 600, false, Some("upstream down"));

        let rows = errors(&conn, &UsageFilter::default(), 50).expect("errors");
        assert_eq!(rows.len(), 2);
        // Both rows are >= 400, so we expect both.
        let codes: Vec<u16> = rows.iter().map(|r| r.status_code).collect();
        assert!(codes.contains(&400));
        assert!(codes.contains(&502));
        for r in &rows {
            assert!(r.status_code >= 400);
            assert!(r.error_msg_redacted.is_some());
        }
    }

    // -----------------------------------------------------------------------
    // 7. errors_respects_limit
    // -----------------------------------------------------------------------
    #[test]
    fn errors_respects_limit() {
        let (conn, _p) = fresh_conn();
        for i in 0..5 {
            let req = format!("req-{}", i);
            let trace = format!("trace-{}", i);
            insert(&conn, &req, &trace, "openrouter", "openai/gpt-4o", Some(1), 500, 10, 5, 0.0, Some(100), 600, false, Some("err"));
        }
        let rows = errors(&conn, &UsageFilter::default(), 2).expect("errors");
        assert_eq!(rows.len(), 2, "limit caps the result set");
    }

    // -----------------------------------------------------------------------
    // Bonus: filter composition + date bounds.
    // -----------------------------------------------------------------------
    #[test]
    fn summary_with_combo_and_model_filter() {
        let (conn, _p) = fresh_conn();
        // Two rows, one with combo 7 and one without.
        let r1 = RequestId::new().to_string();
        let r2 = RequestId::new().to_string();
        conn.execute(
            "INSERT INTO usage (\
                request_id, trace_id, attempt, provider_id, account_id, combo_id, \
                upstream_model_id, prompt_tokens, completion_tokens, cost_usd, \
                connect_ms, ttft_ms, total_ms, status_code, error_msg, \
                error_msg_redacted, race_total, race_lost, created_at\
             ) VALUES (\
                ?1, ?2, 1, 'openrouter', 1, 7, 'openai/gpt-4o', 10, 5, 0.01, 50, 100, 600, 200, NULL, NULL, 1, 0, datetime('now')\
             )",
            params![r1, TraceId::new().to_string()],
        ).expect("insert 1");
        conn.execute(
            "INSERT INTO usage (\
                request_id, trace_id, attempt, provider_id, account_id, combo_id, \
                upstream_model_id, prompt_tokens, completion_tokens, cost_usd, \
                connect_ms, ttft_ms, total_ms, status_code, error_msg, \
                error_msg_redacted, race_total, race_lost, created_at\
             ) VALUES (\
                ?1, ?2, 1, 'openrouter', 1, 8, 'openai/gpt-4o-mini', 10, 5, 0.02, 50, 100, 600, 200, NULL, NULL, 1, 0, datetime('now')\
             )",
            params![r2, TraceId::new().to_string()],
        ).expect("insert 2");

        let f = UsageFilter {
            combo_id: Some(ComboId(7)),
            model_id: Some("openai/gpt-4o".to_string()),
            ..Default::default()
        };
        let s = summary(&conn, &f).expect("summary");
        assert_eq!(s.total_rows, 1);
        assert_eq!(s.total_cost_usd, 0.01);
    }

    #[test]
    fn empty_db_returns_zero_summary() {
        let (conn, _p) = fresh_conn();
        let s = summary(&conn, &UsageFilter::default()).expect("summary");
        assert_eq!(s.unique_requests, 0);
        assert_eq!(s.total_rows, 0);
        assert_eq!(s.winners, 0);
        assert_eq!(s.losers, 0);
        // AVG over zero rows is NULL in SQLite → None.
        assert_eq!(s.avg_ttft_ms, None);
        // COALESCE keeps AVG(total_ms) at 0.0 even with no rows.
        assert_eq!(s.avg_total_ms, 0.0);
    }

    #[test]
    fn avg_ttft_skips_null_rows() {
        let (conn, _p) = fresh_conn();
        // Row 1: ttft=100; Row 2: ttft NULL (race_lost pre-body).
        insert(&conn, &RequestId::new().to_string(), &TraceId::new().to_string(),
            "openrouter", "openai/gpt-4o", Some(1), 200, 10, 5, 0.01, Some(100), 600, false, None);
        insert(&conn, &RequestId::new().to_string(), &TraceId::new().to_string(),
            "openrouter", "openai/gpt-4o", Some(1), 200, 10, 5, 0.01, None, 600, true, Some("race lost"));
        let s = summary(&conn, &UsageFilter::default()).expect("summary");
        assert_eq!(s.avg_ttft_ms, Some(100.0), "NULL ttft is excluded from AVG");
    }

    // -----------------------------------------------------------------------
    // 8. recent returns rows newer than since_id, in id ASC order.
    // -----------------------------------------------------------------------
    #[test]
    fn recent_returns_rows_after_since_id() {
        let (conn, _p) = fresh_conn();
        // Three rows, ids will be 1, 2, 3.
        insert(&conn, &RequestId::new().to_string(), &TraceId::new().to_string(),
            "openrouter", "openai/gpt-4o", Some(1), 200, 10, 5, 0.01, Some(100), 600, false, None);
        insert(&conn, &RequestId::new().to_string(), &TraceId::new().to_string(),
            "openrouter", "openai/gpt-4o", Some(2), 200, 20, 10, 0.02, Some(150), 700, true, None);
        insert(&conn, &RequestId::new().to_string(), &TraceId::new().to_string(),
            "anthropic", "claude-3.5-sonnet", Some(3), 500, 0, 0, 0.0, None, 800, false, Some("oops"));

        // since_id = 0 returns all three, in id ASC order.
        let all = recent(&conn, 0, 50).expect("recent");
        assert_eq!(all.len(), 3);
        assert!(all.iter().map(|r| r.id.0).eq(1..=3));
        // race_lost propagates (1 = true).
        assert!(!all[0].race_lost);
        assert!(all[1].race_lost);
        assert!(!all[2].race_lost);
        // cost_usd + tokens survive the round-trip.
        assert_eq!(all[0].prompt_tokens, Some(10));
        assert_eq!(all[0].completion_tokens, Some(5));
        assert!((all[0].cost_usd.unwrap() - 0.01).abs() < 1e-9);
        // The error row's cost is 0.0; the column is NOT NULL, so we get Some(0.0).
        assert_eq!(all[2].cost_usd, Some(0.0));
        assert_eq!(all[2].status_code, 500);

        // since_id = 1 → only ids > 1, i.e. 2 and 3.
        let after = recent(&conn, 1, 50).expect("recent");
        assert_eq!(after.len(), 2);
        assert!(after.iter().map(|r| r.id.0).eq(2..=3));

        // since_id past the last id → empty.
        let none = recent(&conn, 999, 50).expect("recent");
        assert!(none.is_empty());

        // limit caps the result.
        let capped = recent(&conn, 0, 2).expect("recent");
        assert_eq!(capped.len(), 2);
        assert!(capped.iter().map(|r| r.id.0).eq(1..=2));
    }

    #[test]
    fn recent_desc_returns_newest_first() {
        let (conn, _p) = fresh_conn();
        insert(&conn, &RequestId::new().to_string(), &TraceId::new().to_string(),
            "openrouter", "openai/gpt-4o", Some(1), 200, 10, 5, 0.01, Some(100), 600, false, None);
        insert(&conn, &RequestId::new().to_string(), &TraceId::new().to_string(),
            "openrouter", "openai/gpt-4o", Some(2), 200, 20, 10, 0.02, Some(150), 700, true, None);
        insert(&conn, &RequestId::new().to_string(), &TraceId::new().to_string(),
            "anthropic", "claude-3.5-sonnet", Some(3), 500, 0, 0, 0.0, None, 800, false, Some("oops"));

        let rows = recent_desc(&conn, 2).expect("recent_desc");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id.0, 3);
        assert_eq!(rows[1].id.0, 2);
    }

    /// H1 regression: three rows for the same request_id (the
    /// retry attempts) must be aggregated into a single
    /// `RecentUsageRow` whose `cost_usd` and token counts are
    /// the SUMs across all three attempts. Status_code picks
    /// the earliest 2xx (which here is the last attempt's
    /// `200`); `id` is the first attempt's id so the
    /// long-polling `since_id` cursor is still monotonically
    /// increasing.
    #[test]
    fn recent_aggregates_retry_attempts_by_request_id() {
        let (conn, _p) = fresh_conn();
        let shared_req = RequestId::new().to_string();
        // Attempt 1: 502 upstream failure (we still pay for
        // the tokens up to the failure; cost_usd = 0 here).
        insert(
            &conn,
            &shared_req,
            "trace-a",
            "openrouter",
            "openai/gpt-4o",
            Some(1),
            502,
            10,
            0,
            0.0,
            Some(100),
            600,
            false,
            Some("upstream 502"),
        );
        // Attempt 2: 429 rate limit.
        insert(
            &conn,
            &shared_req,
            "trace-b",
            "openrouter",
            "openai/gpt-4o",
            Some(2),
            429,
            10,
            0,
            0.0,
            Some(100),
            600,
            false,
            Some("rate limited"),
        );
        // Attempt 3: 200 success, full tokens.
        insert(
            &conn,
            &shared_req,
            "trace-c",
            "openrouter",
            "openai/gpt-4o",
            Some(3),
            200,
            100,
            50,
            0.03,
            Some(150),
            700,
            true,
            None,
        );

        let rows = recent(&conn, 0, 50).expect("recent");
        assert_eq!(rows.len(), 1, "three attempts collapse to one row");
        let row = &rows[0];
        // id is the first attempt's id.
        assert_eq!(row.id.0, 1);
        assert_eq!(row.request_id, shared_req);
        // status_code is the earliest 2xx — attempt 3's 200.
        assert_eq!(row.status_code, 200);
        // SUM(prompt_tokens) = 10 + 10 + 100 = 120.
        assert_eq!(row.prompt_tokens, Some(120));
        // SUM(completion_tokens) = 0 + 0 + 50 = 50.
        assert_eq!(row.completion_tokens, Some(50));
        // SUM(cost_usd) = 0.03.
        let cost = row.cost_usd.expect("cost_usd present");
        assert!((cost - 0.03).abs() < 1e-9, "cost_usd was {}", cost);
        // trace_id is the first attempt's trace (the one we
        // JOINed to from the grouped.MIN(id) row).
        assert_eq!(row.trace_id, "trace-a");
    }

    /// H1 mirror: `recent_desc` (the head-of-table fetch the
    /// admin table uses) must apply the same per-request
    /// aggregation as `recent` (the long-poll feed). Without
    /// this, the admin table at the top of the dashboard
    /// would show the cost of the LAST attempt only, even
    /// though the same request's history in the log would
    /// show the summed cost.
    #[test]
    fn recent_desc_aggregates_retry_attempts_by_request_id() {
        let (conn, _p) = fresh_conn();
        let shared_req = RequestId::new().to_string();
        insert(
            &conn,
            &shared_req,
            "trace-a",
            "openrouter",
            "openai/gpt-4o",
            Some(1),
            502,
            10,
            0,
            0.0,
            Some(100),
            600,
            false,
            Some("upstream 502"),
        );
        insert(
            &conn,
            &shared_req,
            "trace-b",
            "openrouter",
            "openai/gpt-4o",
            Some(2),
            200,
            100,
            50,
            0.03,
            Some(150),
            700,
            true,
            None,
        );
        let rows = recent_desc(&conn, 50).expect("recent_desc");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].request_id, shared_req);
        assert_eq!(rows[0].prompt_tokens, Some(110));
        assert_eq!(rows[0].completion_tokens, Some(50));
    }

    /// Compression regression: `recent_desc` (used by the WS
    /// initial history batch) must include the compression stats
    /// columns `compression_savings_pct` and
    /// `compression_techniques`. Before the migration-00031
    /// backfill the row mapping hardcoded both to `None`, so the
    /// dashboard's compression column showed `—` for every
    /// historical row even when compression was applied. Mirrors
    /// the same coverage `recent` already had.
    #[test]
    fn recent_desc_includes_compression_columns() {
        let (conn, _p) = fresh_conn();
        let req = RequestId::new().to_string();
        let trace = TraceId::new().to_string();
        conn.execute(
            "INSERT INTO usage (\
                request_id, trace_id, attempt, provider_id, account_id, \
                upstream_model_id, prompt_tokens, completion_tokens, cost_usd, \
                connect_ms, ttft_ms, total_ms, status_code, error_msg, \
                error_msg_redacted, race_total, race_lost, created_at, \
                compression_savings_pct, compression_techniques\
             ) VALUES (\
                ?1, ?2, 1, 'openrouter', 1, 'openai/gpt-4o', 100, 50, 0.01, \
                50, 200, 1200, 200, NULL, NULL, 1, 0, datetime('now'), \
                42.0, 'lite::collapse_whitespace'\
             )",
            params![req, trace],
        )
        .expect("insert with compression stats");
        let row_id = conn.last_insert_rowid();

        let rows = recent_desc(&conn, 10).expect("recent_desc");
        let row = rows
            .iter()
            .find(|r| r.id.0 == row_id)
            .expect("inserted row should be in recent_desc results");
        assert_eq!(row.compression_savings_pct, Some(42.0));
        assert_eq!(
            row.compression_techniques.as_deref(),
            Some("lite::collapse_whitespace")
        );
    }

    #[test]
    fn detail_by_id_returns_full_usage_row() {
        let (conn, _p) = fresh_conn();
        let req = RequestId::new().to_string();
        let trace = TraceId::new().to_string();
        conn.execute(
            "INSERT INTO usage (\
                request_id, trace_id, attempt, provider_id, account_id, combo_id, \
                model_row_id, upstream_model_id, combo_target_id, prompt_tokens, \
                completion_tokens, connect_ms, ttft_ms, total_ms, tokens_per_sec, \
                status_code, error_msg, error_msg_redacted, race_total, race_lost, \
                api_key_id, created_at\
             ) VALUES (\
                ?1, ?2, 2, 'openrouter', 1, 7, 9, 'openai/gpt-4o', 11, 100, 50, \
                25, 200, 1200, 50.0, 500, 'raw secret', 'raw secret', 3, 1, NULL, \
                datetime('now')\
             )",
            params![req, trace],
        )
        .expect("insert detail");
        let id = conn.last_insert_rowid();

        let row = detail_by_id(&conn, id).expect("detail_by_id").expect("row exists");
        assert_eq!(row.id.0, id);
        assert_eq!(row.request_id, req);
        assert_eq!(row.trace_id, trace);
        assert_eq!(row.attempt, 2);
        assert_eq!(row.provider_id, ProviderId::new("openrouter"));
        assert_eq!(row.account_id, Some(AccountId::new(1)));
        assert_eq!(row.combo_id, Some(ComboId(7)));
        assert_eq!(row.model_row_id, Some(ModelRowId(9)));
        assert_eq!(row.upstream_model_id, "openai/gpt-4o");
        assert_eq!(row.combo_target_id, Some(ComboTargetId(11)));
        assert_eq!(row.prompt_tokens, Some(100));
        assert_eq!(row.completion_tokens, Some(50));
        assert_eq!(row.connect_ms, Some(25));
        assert_eq!(row.ttft_ms, Some(200));
        assert_eq!(row.total_ms, 1200);
        assert_eq!(row.tokens_per_sec, Some(50.0));
        assert_eq!(row.status_code, 500);
        assert_eq!(row.error_msg, Some("raw secret".to_string()));
        assert_eq!(row.error_msg_redacted, Some("raw secret".to_string()));
        assert_eq!(row.race_total, 3);
        assert!(row.race_lost);
        assert_eq!(row.api_key_id, None);
        assert!(!row.created_at.is_empty());

        assert!(detail_by_id(&conn, id + 1).expect("detail_by_id missing").is_none());
    }

    #[test]
    fn prune_expired_recording_bodies_clears_old_rows() {
        use rusqlite::params;
        let (conn, _p) = fresh_conn();
        // Insert a row with bodies and a very recent created_at.
        conn.execute(
            "INSERT INTO usage (request_id, trace_id, attempt, provider_id, \
             upstream_model_id, prompt_tokens, completion_tokens, cost_usd, \
             connect_ms, ttft_ms, total_ms, status_code, race_total, race_lost, \
             created_at, request_body_json, response_body_json, request_headers, response_headers) \
             VALUES (?, ?, 1, 'openrouter', 'openai/gpt-4o', 100, 50, 0.01, 50, 200, 1200, 200, 1, 0, \
                     datetime('now'), '{\"q\":\"hello\"}', '{\"a\":\"world\"}', '{\"ct\":\"text/plain\"}', '{\"ct\":\"text/plain\"}')",
            params!["req1", "trace1"],
        )
        .expect("insert recent row");
        // Insert a row with bodies that is 10 minutes old.
        conn.execute(
            "INSERT INTO usage (request_id, trace_id, attempt, provider_id, \
             upstream_model_id, prompt_tokens, completion_tokens, cost_usd, \
             connect_ms, ttft_ms, total_ms, status_code, race_total, race_lost, \
             created_at, request_body_json, response_body_json, request_headers, response_headers) \
             VALUES (?, ?, 1, 'openrouter', 'openai/gpt-4o', 100, 50, 0.01, 50, 200, 1200, 200, 1, 0, \
                     datetime('now', '-600 seconds'), '{\"q\":\"old\"}', '{\"a\":\"old\"}', '{\"ct\":\"old\"}', '{\"ct\":\"old\"}')",
            params!["req2", "trace2"],
        )
        .expect("insert old row");
        // TTL of 5 minutes: only the old row should be pruned.
        let pruned = prune_expired_recording_bodies(&conn, 300).expect("prune");
        assert_eq!(pruned, 1, "only the old row should be pruned");
        // Verify: recent row still has bodies.
        let recent_body: Option<String> = conn
            .query_row(
                "SELECT request_body_json FROM usage WHERE request_id = 'req1'",
                [],
                |r| r.get(0),
            )
            .expect("query recent");
        assert!(recent_body.is_some(), "recent row should still have body");
        // Verify: old row bodies are now NULL.
        let old_body: Option<String> = conn
            .query_row(
                "SELECT request_body_json FROM usage WHERE request_id = 'req2'",
                [],
                |r| r.get(0),
            )
            .expect("query old");
        assert!(old_body.is_none(), "old row body should be NULL");
        // Verify: metadata (e.g. status_code) is preserved for both.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage WHERE status_code = 200", [], |r| r.get(0))
            .expect("count");
        assert_eq!(count, 2, "metadata should be preserved for both rows");
    }

    #[test]
    fn prune_expired_recording_bodies_zero_ttl_clears_all() {
        let (conn, _p) = fresh_conn();
        conn.execute(
            "INSERT INTO usage (request_id, trace_id, attempt, provider_id, \
             upstream_model_id, prompt_tokens, completion_tokens, cost_usd, \
             connect_ms, ttft_ms, total_ms, status_code, race_total, race_lost, \
             created_at, request_body_json, response_body_json) \
             VALUES (?, ?, 1, 'openrouter', 'openai/gpt-4o', 100, 50, 0.01, 50, 200, 1200, 200, 1, 0, \
                     datetime('now'), '{\"q\":\"hi\"}', '{\"a\":\"ok\"}')",
            params!["req1", "trace1"],
        )
        .expect("insert");
        let pruned = prune_expired_recording_bodies(&conn, 0).expect("prune");
        assert_eq!(pruned, 1, "zero TTL should clear all bodies");
        let body: Option<String> = conn
            .query_row(
                "SELECT request_body_json FROM usage WHERE request_id = 'req1'",
                [],
                |r| r.get(0),
            )
            .expect("query");
        assert!(body.is_none(), "body should be NULL after zero-TTL prune");
    }

    // -----------------------------------------------------------------------
    // by_provider: groups by provider_id, ordered by total_cost_usd DESC.
    // -----------------------------------------------------------------------
    #[test]
    fn by_provider_groups_by_provider_id() {
        let (conn, _p) = fresh_conn();
        // openrouter: 2 rows, total cost 0.50, both winners.
        // anthropic: 1 row, total cost 1.00, winner.
        // openai: 1 row, total cost 0.10, loser (race_lost=1).
        let r1 = RequestId::new().to_string();
        let r2 = RequestId::new().to_string();
        let r3 = RequestId::new().to_string();
        let r4 = RequestId::new().to_string();
        insert(&conn, &r1, &TraceId::new().to_string(), "openrouter", "openai/gpt-4o", Some(1), 200, 100, 50, 0.25, Some(200), 1200, false, None);
        insert(&conn, &r2, &TraceId::new().to_string(), "openrouter", "openai/gpt-4o-mini", Some(1), 200, 100, 50, 0.25, Some(200), 600, false, None);
        insert(&conn, &r3, &TraceId::new().to_string(), "anthropic", "claude-3.5-sonnet", Some(2), 200, 100, 50, 1.00, Some(200), 1500, false, None);
        insert(&conn, &r4, &TraceId::new().to_string(), "openai", "gpt-4o", Some(3), 200, 100, 50, 0.10, Some(200), 800, true, None);

        let rows = by_provider(&conn, &UsageFilter::default()).expect("by_provider");
        assert_eq!(rows.len(), 3, "three distinct providers");

        // Order: anthropic (1.00), openrouter (0.50), openai (0.10).
        assert_eq!(rows[0].provider_id, "anthropic");
        assert!((rows[0].total_cost_usd - 1.00).abs() < 1e-9);
        assert_eq!(rows[0].total_rows, 1);
        assert_eq!(rows[0].unique_requests, 1);
        assert_eq!(rows[0].winners, 1);
        assert_eq!(rows[0].total_prompt_tokens, 100);
        assert_eq!(rows[0].total_completion_tokens, 50);

        assert_eq!(rows[1].provider_id, "openrouter");
        assert!((rows[1].total_cost_usd - 0.50).abs() < 1e-9);
        assert_eq!(rows[1].total_rows, 2);
        assert_eq!(rows[1].unique_requests, 2);
        assert_eq!(rows[1].winners, 2);
        assert_eq!(rows[1].total_prompt_tokens, 200);

        assert_eq!(rows[2].provider_id, "openai");
        assert!((rows[2].total_cost_usd - 0.10).abs() < 1e-9);
        assert_eq!(rows[2].winners, 0, "race_lost=1 means 0 winners");

        // Filter by a single provider — only that row comes back.
        let f = UsageFilter {
            provider_id: Some(ProviderId::new("openrouter")),
            ..Default::default()
        };
        let filtered = by_provider(&conn, &f).expect("by_provider filtered");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].provider_id, "openrouter");
        assert_eq!(filtered[0].total_rows, 2);
    }

    // -----------------------------------------------------------------------
    // monthly_by_provider: groups by (provider_id, month).
    // -----------------------------------------------------------------------
    #[test]
    fn monthly_by_provider_groups_by_month() {
        let (conn, _p) = fresh_conn();
        // Insert rows with explicit created_at values across two months
        // and three providers. The `insert` helper uses datetime('now'),
        // so we hand-roll the INSERTs to pin the timestamp.
        let insert_at = |provider: &str, cost: f64, prompt: i64, created_at: &str| {
            conn.execute(
                "INSERT INTO usage (\
                    request_id, trace_id, attempt, provider_id, account_id, \
                    upstream_model_id, prompt_tokens, completion_tokens, cost_usd, \
                    connect_ms, ttft_ms, total_ms, status_code, error_msg, \
                    error_msg_redacted, race_total, race_lost, created_at\
                 ) VALUES (\
                    ?1, ?2, 1, ?3, 1, 'm', ?4, 0, ?5, 50, 200, 1200, 200, NULL, NULL, 1, 0, ?6\
                 )",
                params![
                    RequestId::new().to_string(),
                    TraceId::new().to_string(),
                    provider,
                    prompt,
                    cost,
                    created_at,
                ],
            )
            .expect("insert");
        };

        // June 2026: openrouter $1.00 + $0.50; anthropic $0.25.
        // RFC-3339 created_at so the lexical string comparison with
        // the filter `from`/`to` (also RFC-3339) is well-defined.
        insert_at("openrouter", 1.00, 100, "2026-06-01T12:00:00Z");
        insert_at("openrouter", 0.50, 100, "2026-06-15T12:00:00Z");
        insert_at("anthropic",  0.25,  50, "2026-06-20T12:00:00Z");
        // July 2026: openrouter $0.10; openai $0.75.
        insert_at("openrouter", 0.10,  10, "2026-07-01T12:00:00Z");
        insert_at("openai",     0.75,  20, "2026-07-31T12:00:00Z");

        let rows = monthly_by_provider(&conn, &UsageFilter::default())
            .expect("monthly_by_provider");
        assert_eq!(rows.len(), 4, "four (provider, month) buckets");

        // Ordered by month ASC, then cost DESC.
        // June (3 rows total):
        //   - openrouter $1.50
        //   - anthropic  $0.25
        // July (2 rows):
        //   - openai     $0.75
        //   - openrouter $0.10
        assert_eq!(rows[0].month, "2026-06");
        assert_eq!(rows[0].provider_id, "openrouter");
        assert!((rows[0].total_cost_usd - 1.50).abs() < 1e-9);
        assert_eq!(rows[0].total_rows, 2);
        assert_eq!(rows[0].unique_requests, 2);
        assert_eq!(rows[0].total_prompt_tokens, 200);

        assert_eq!(rows[1].month, "2026-06");
        assert_eq!(rows[1].provider_id, "anthropic");
        assert!((rows[1].total_cost_usd - 0.25).abs() < 1e-9);

        assert_eq!(rows[2].month, "2026-07");
        assert_eq!(rows[2].provider_id, "openai");
        assert!((rows[2].total_cost_usd - 0.75).abs() < 1e-9);

        assert_eq!(rows[3].month, "2026-07");
        assert_eq!(rows[3].provider_id, "openrouter");
        assert!((rows[3].total_cost_usd - 0.10).abs() < 1e-9);

        // Date filter: only June.
        let f = UsageFilter {
            from: Some("2026-06-01T00:00:00Z".to_string()),
            to: Some("2026-07-01T00:00:00Z".to_string()),
            ..Default::default()
        };
        let june_only = monthly_by_provider(&conn, &f).expect("monthly_by_provider june");
        assert_eq!(june_only.len(), 2);
        assert!(june_only.iter().all(|r| r.month == "2026-06"));
    }

    // -----------------------------------------------------------------------
    // summary.rows_with_null_pricing counts rows where cost_usd = 0 AND
    // prompt_tokens > 0 (pricing was missing at record time).
    // -----------------------------------------------------------------------
    #[test]
    fn summary_counts_rows_with_null_pricing() {
        let (conn, _p) = fresh_conn();
        // Row 1: normal — has prompt_tokens and a non-zero cost.
        insert(&conn, &RequestId::new().to_string(), &TraceId::new().to_string(),
            "openrouter", "openai/gpt-4o", Some(1), 200, 100, 50, 0.01, Some(100), 600, false, None);
        // Row 2: NULL pricing — prompt_tokens > 0 but cost_usd = 0.
        insert(&conn, &RequestId::new().to_string(), &TraceId::new().to_string(),
            "openrouter", "openai/gpt-4o", Some(1), 200, 200, 50, 0.00, Some(100), 600, false, None);
        // Row 3: NULL pricing — same shape, different provider.
        insert(&conn, &RequestId::new().to_string(), &TraceId::new().to_string(),
            "anthropic", "claude-3.5-sonnet", Some(2), 200, 300, 0, 0.00, Some(100), 600, false, None);
        // Row 4: zero tokens AND zero cost — NOT null pricing (no tokens consumed).
        insert(&conn, &RequestId::new().to_string(), &TraceId::new().to_string(),
            "openrouter", "openai/gpt-4o", Some(1), 500, 0, 0, 0.00, Some(100), 600, false, Some("err"));

        let s = summary(&conn, &UsageFilter::default()).expect("summary");
        assert_eq!(s.total_rows, 4);
        assert_eq!(
            s.rows_with_null_pricing, 2,
            "rows 2 and 3 had prompt_tokens > 0 but cost_usd = 0"
        );

        // Filter to just anthropic — only row 3 matches, so count drops to 1.
        let f = UsageFilter {
            provider_id: Some(ProviderId::new("anthropic")),
            ..Default::default()
        };
        let s = summary(&conn, &f).expect("summary filtered");
        assert_eq!(s.total_rows, 1);
        assert_eq!(s.rows_with_null_pricing, 1);

        // Empty DB: counter is 0.
        let (conn2, _p2) = fresh_conn();
        let s_empty = summary(&conn2, &UsageFilter::default()).expect("summary empty");
        assert_eq!(s_empty.rows_with_null_pricing, 0);
    }
}
