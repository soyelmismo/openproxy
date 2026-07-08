use super::*;
use crate::error::*;
use once_cell::sync::OnceCell;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
// ---------------------------------------------------------------------------
// Broadcast channel (global, initialized once)
// ---------------------------------------------------------------------------

/// Channel capacity for the usage broadcast. Rows older than this are
/// dropped for slow receivers.
///
/// 1024 (was 256). The 256 figure was tuned for "~50 concurrent
/// requests", but in practice a single failing request produces a
/// burst of two renders in the dashboard (stage `failed` + the
/// terminal usage row), and a few failures in a row can easily
/// push a busy proxy past 256 in-flight events before a slow
/// browser tab drains its queue. 1024 × sizeof(RecentUsageRow) ≈
/// ~150 KB — trivial — and it raises the ceiling far enough that
/// the only way to lag is a genuinely stuck consumer, which the
/// decoupled sender task (see `stream_usage_rows` in admin.rs)
/// now handles.
const BROADCAST_CAPACITY: usize = 1024;

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
/// ~5 stage events (started, connecting, waiting_ttft, streaming,
/// completed). 1024 is enough for ~200 concurrent requests without
/// lagging, while bounding memory to ~1024 × sizeof(StageEvent) ≈
/// ~80 KB. Was 256 (~20 KB), which was too tight — a slow browser
/// tab doing a full-DOM rebuild on every WS message could easily
/// lag past 256 in-flight events during a failure burst, dropping
/// the `started`/`connecting` events of subsequent requests and
/// making them invisible until their terminal usage row landed.
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
    /// The endpoint kind (chat, audio, etc.). Defaults to Chat.
    pub endpoint_kind: crate::endpoint::EndpointKind,
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
    // Bug fix: do NOT skip when receiver_count == 0. The previous
    // optimization returned early when no dashboard was connected,
    // but that meant if the dashboard disconnected briefly (e.g.
    // WebSocket reconnect after an error), all stage events for
    // in-flight requests were silently dropped. The dashboard then
    // only saw the final usage row (published by publish_usage_row,
    // which does NOT have this check) — so rows appeared "stuck" in
    // their last known stage until they completed.
    //
    // The broadcast channel has capacity 1024; sending with no
    // receivers is a no-op (send returns Err, which we ignore).
    // The only cost of always sending is the timestamp formatting
    // below, which is cheap.
    event.timestamp = chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string();

    // Also emit a tracing::info! so the DebugLogLayer (installed in
    // openproxy-server::telemetry) captures the stage transition in
    // the in-memory ring buffer exposed via GET /admin/debug/logs.
    // Without this dual-publishing, the Debug Logs panel can't
    // correlate events to a specific request_id — the stage events
    // only flow through the broadcast channel (visible in the Live
    // Logs panel) and never reach the tracing subscriber.
    //
    // The fields (request_id, trace_id, stage, status_code, etc.)
    // are emitted as structured tracing fields so the
    // MessageVisitor in debug_log.rs can extract them for
    // correlation.
    let stage = event.stage.as_str();
    let rid = event.request_id.as_str();
    let tid = event.trace_id.as_str();
    let pid = event.provider_id.as_str();
    let mid = event.upstream_model_id.as_str();
    let elapsed = event.elapsed_ms;
    let sc = event.status_code;
    let connect = event.connect_ms;
    let ttft = event.ttft_ms;
    match stage {
        "completed" => {
            tracing::info!(
                request_id = %rid, trace_id = %tid, provider_id = %pid,
                upstream_model_id = %mid, stage = %stage, status_code = sc,
                elapsed_ms = elapsed, connect_ms = ?connect, ttft_ms = ?ttft,
                "request completed"
            );
        }
        "failed" | "cancelled" => {
            let err = event.error.as_deref().unwrap_or("");
            tracing::warn!(
                request_id = %rid, trace_id = %tid, provider_id = %pid,
                upstream_model_id = %mid, stage = %stage, status_code = sc,
                elapsed_ms = elapsed, connect_ms = ?connect, ttft_ms = ?ttft,
                error = %err,
                "request {}", stage
            );
        }
        _ => {
            tracing::info!(
                request_id = %rid, trace_id = %tid, provider_id = %pid,
                upstream_model_id = %mid, stage = %stage, status_code = sc,
                elapsed_ms = elapsed, connect_ms = ?connect, ttft_ms = ?ttft,
                "request stage: {}", stage
            );
        }
    }

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
            params![
                chrono::Utc::now().to_rfc3339(),
                format!("-{} seconds", ttl_secs)
            ],
        )
        .map_err(crate::error::map_db_error)?;
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
            params![
                chrono::Utc::now().to_rfc3339(),
                format!("-{} seconds", ttl_secs)
            ],
        )
        .map_err(crate::error::map_db_error)?;
    Ok(n)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
