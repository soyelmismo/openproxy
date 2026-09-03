//! In-memory ring buffer of recent `tracing` events, exposed to the
//! dashboard via `GET /admin/debug/logs`.
//!
//! ## Why
//!
//! The user asked for "un registro en el dashboard de debug logs que
//! sean fácilmente copiables toda la info de depuración" — a way to
//! see detailed error logs in the dashboard, easily copiable, so when
//! something fails they can grab the full context and share it.
//!
//! The existing `usage` table stores per-request error messages (the
//! `error_msg` / `error_msg_redacted` columns), but it doesn't capture
//! the broader `tracing`-level context: discovery scheduler skips,
//! OAuth refresh failures, race cancellation reasons, etc. Those events
//! go to stdout via `tracing_subscriber::fmt`, which the operator can't
//! access from the dashboard.
//!
//! This module installs a custom `tracing_subscriber::Layer` that
//! captures every `WARN` / `ERROR` event (and optionally `INFO`) into
//! a bounded `parking_lot::Mutex<VecDeque<DebugLogEntry>>` (capacity
//! 1000). The dashboard polls `GET /admin/debug/logs?since=N` to read
//! the buffer, and a "Copy all" button serializes the visible subset
//! to the clipboard.
//!
//! ## Design
//!
//! - **In-memory only.** Tracing events are high-volume and don't
//!   belong in SQLite. The buffer is bounded at 1000 entries ×
//!   ~500 B avg ≈ 500 KB worst case — trivial.
//! - **Bounded VecDeque.** New entries push to the back; when full,
//!   the oldest entry is evicted. Each entry gets a monotonic `seq`
//!   so the frontend can poll with `?since=N` to fetch only new
//!   entries.
//! - **Span context extraction.** When a tracing event fires inside
//!   a span that carries `request_id` / `trace_id` fields (set by
//!   the pipeline's `info!(request_id = …, trace_id = …, …)` calls),
//!   the layer extracts them so the dashboard can correlate events
//!   with usage rows.
//! - **No redaction here.** The layer captures the formatted message
//!   string (post-`tracing` field formatting). The pipeline already
//!   redacts sensitive values before logging them (see `redact.rs`),
//!   so we don't re-redact. If a future caller logs raw secrets, the
//!   `cost::redact_error_msg` regex is available to apply here too.

use std::collections::VecDeque;
use std::sync::{LazyLock, OnceLock};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::Serialize;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::{Layer, layer::Context};

/// Maximum number of entries kept in the ring buffer. Older entries
/// are evicted when this is exceeded. 1000 entries × ~500 B avg ≈
/// 500 KB worst case — a trivial amount of memory for hours of
/// debugging context.
const BUFFER_CAPACITY: usize = 1000;
const FILE_LOG_CAPACITY: usize = 1000;

/// A single captured tracing event, ready to be serialized to the
/// dashboard via `GET /admin/debug/logs`.
#[derive(Debug, Clone, Serialize)]
pub struct DebugLogEntry {
    /// Monotonically increasing sequence number. The frontend polls
    /// with `?since=N` to fetch only entries with `seq > N`.
    pub seq: u64,
    /// UTC timestamp. Formatted as ISO-8601 when serialized.
    pub timestamp: DateTime<Utc>,
    /// `WARN`, `ERROR`, `INFO`, etc.
    pub level: &'static str,
    /// The tracing target (usually the module path, e.g.
    /// `openproxy_core::pipeline`).
    pub target: String,
    /// The formatted message string (post-`tracing` field
    /// formatting). Sensitive values are already redacted by the
    /// pipeline before logging.
    pub message: String,
    /// `request_id` extracted from the span context, when available.
    /// Used by the dashboard to correlate events with usage rows.
    pub request_id: Option<String>,
    /// `trace_id` extracted from the span context, when available.
    pub trace_id: Option<String>,
    /// The span hierarchy as a slash-separated path (e.g.
    /// `execute_single/dispatch_upstream_streaming`). Useful for
    /// understanding where in the pipeline the event fired.
    pub span_path: Option<String>,
}

/// The global ring buffer. Initialized once via [`init`] (called from
/// `telemetry::init`); accessed via [`snapshot`] and [`snapshot_since`].
static DEBUG_LOG_BUFFER: LazyLock<Mutex<DebugLogBuffer>> =
    LazyLock::new(|| Mutex::new(DebugLogBuffer::new()));
static FILE_LOG_SENDER: OnceLock<mpsc::Sender<DebugLogEntry>> = OnceLock::new();

/// Internal struct holding the VecDeque + the monotonic seq counter.
struct DebugLogBuffer {
    entries: VecDeque<DebugLogEntry>,
    next_seq: u64,
}

impl DebugLogBuffer {
    fn new() -> Self {
        Self {
            entries: VecDeque::with_capacity(BUFFER_CAPACITY),
            next_seq: 1,
        }
    }

    fn push(&mut self, mut entry: DebugLogEntry) {
        entry.seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        if self.entries.len() >= BUFFER_CAPACITY {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }
}

fn resolve_debug_log_path() -> std::path::PathBuf {
    let home = std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok())
        .unwrap_or_else(|| ".".to_string());
    std::path::PathBuf::from(home)
        .join(".openproxy")
        .join("debug.log")
}

fn spawn_file_logger_task(path: std::path::PathBuf, mut rx: mpsc::Receiver<DebugLogEntry>) {
    tokio::spawn(async move {
        if let Some(parent) = path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }

        let Ok(mut file) = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .await
        else {
            return;
        };

        let mut rotation_interval = tokio::time::interval(std::time::Duration::from_hours(12));
        rotation_interval.tick().await;

        loop {
            tokio::select! {
                _ = rotation_interval.tick() => {
                    if let Ok(new_file) = tokio::fs::OpenOptions::new()
                        .create(true)
                        .write(true)
                        .truncate(true)
                        .open(&path)
                        .await
                    {
                        file = new_file;
                    }
                }
                msg_opt = rx.recv() => {
                    match msg_opt {
                        Some(entry) => {
                            let mut batch = vec![entry];
                            while batch.len() < 50 {
                                match rx.try_recv() {
                                    Ok(e) => batch.push(e),
                                    Err(_) => break,
                                }
                            }
                            for item in batch {
                                if let Ok(mut json) = serde_json::to_string(&item) {
                                    json.push('\n');
                                    let _ = file.write_all(json.as_bytes()).await;
                                }
                            }
                            let _ = file.flush().await;
                        }
                        None => break,
                    }
                }
            }
        }
    });
}

/// Initialize the global ring buffer. Must be called once at startup
/// (from `telemetry::init`) before any [`DebugLogLayer`] is installed.
/// Idempotent: subsequent calls are no-ops.
pub fn init() {
    let _ = &*DEBUG_LOG_BUFFER;
    let _ = FILE_LOG_SENDER.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<DebugLogEntry>(FILE_LOG_CAPACITY);
        let path = resolve_debug_log_path();
        spawn_file_logger_task(path, rx);
        tx
    });
}

/// Snapshot ALL entries currently in the buffer, in insertion order
/// (oldest first). Used by the dashboard's initial fetch.
pub fn snapshot() -> Vec<DebugLogEntry> {
    let guard = DEBUG_LOG_BUFFER.lock();
    guard.entries.iter().cloned().collect()
}

/// Snapshot entries with `seq > since`, in insertion order. Used by
/// the dashboard's polling fetch.
pub fn snapshot_since(since: u64) -> Vec<DebugLogEntry> {
    let guard = DEBUG_LOG_BUFFER.lock();
    guard
        .entries
        .iter()
        .filter(|e| e.seq > since)
        .cloned()
        .collect()
}

/// The highest `seq` currently in the buffer. The frontend uses this
/// to know what `since` value to pass on the next poll.
pub fn latest_seq() -> u64 {
    let guard = DEBUG_LOG_BUFFER.lock();
    guard.entries.back().map_or(0, |e| e.seq)
}

/// Clear all entries from the buffer. Used by `POST /admin/debug/clear`
/// for "reproduce then capture" workflows.
pub fn clear() {
    let mut guard = DEBUG_LOG_BUFFER.lock();
    guard.entries.clear();
    guard.next_seq = 1;
}

/// A `tracing_subscriber::Layer` that captures every event into the
/// global ring buffer. Installed by `telemetry::init` alongside the
/// existing `fmt::layer()`.
pub struct DebugLogLayer;

impl<S> Layer<S> for DebugLogLayer
where
    S: Subscriber,
    for<'lookup> S: tracing_subscriber::registry::LookupSpan<'lookup>,
{
    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        // Capture the level, target, and timestamp.
        let level = event.metadata().level().as_str();
        let target = event.metadata().target().to_string();
        let timestamp = Utc::now();

        // Visit the event's fields to build the formatted message.
        // The visitor also opportunistically extracts `request_id`
        // and `trace_id` if they're set directly on the event (not
        // on a parent span).
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        // Extract the fields we care about BEFORE calling
        // `into_message()` (which consumes the visitor).
        let request_id = visitor.request_id.take();
        let trace_id = visitor.trace_id.take();
        let raw_message = visitor.into_message();
        let (message, _) = openproxy_core::cost::redact_error_msg(&raw_message);

        // Walk the span hierarchy (from the current span up to the
        // root) to build the span path. `ctx.event_scope(event)`
        // returns an iterator from the current span up through all
        // parents.
        //
        // We DON'T extract request_id / trace_id from span fields
        // here because the tracing-subscriber 0.3 API for reading
        // stored span values (`SpanRef::values()` + a custom
        // `Visit`) is unstable across versions. Instead, we rely on
        // the pipeline's existing practice of stamping
        // `request_id` / `trace_id` directly on each `tracing::event!`
        // call (via `info!(request_id = %rid, …)` etc.), which
        // `MessageVisitor` captures via the event's own fields.
        let mut span_path_parts: Vec<String> = Vec::new();
        if let Some(scope) = ctx.event_scope(event) {
            for span in scope.from_root() {
                span_path_parts.push(span.name().to_string());
            }
        }

        let span_path = if span_path_parts.is_empty() {
            None
        } else {
            Some(span_path_parts.join("/"))
        };

        let entry = DebugLogEntry {
            seq: 0, // filled in by `push`
            timestamp,
            level,
            target,
            message,
            request_id,
            trace_id,
            span_path,
        };

        // Push into the global buffer.
        let file_entry = {
            let mut guard = DEBUG_LOG_BUFFER.lock();
            let to_send = entry.clone();
            guard.push(entry);
            to_send
        };

        // Push to the file log task.
        if let Some(tx) = FILE_LOG_SENDER.get() {
            let _ = tx.try_send(file_entry);
        }
    }
}

/// Visit the event's fields to build a human-readable message. The
/// `tracing` macro formats fields as `name=value` pairs, with the
/// special `"message"` field treated as the primary message text.
///
/// Also opportunistically extracts `request_id` and `trace_id` when
/// they're set directly on the event (not on a parent span — those
/// are handled by `SpanFieldVisitor`).
#[derive(Default)]
struct MessageVisitor {
    parts: Vec<(String, String)>,
    message: Option<String>,
    request_id: Option<String>,
    trace_id: Option<String>,
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let name = field.name();
        // For request_id / trace_id, the tracing macro emits them
        // via `record_debug` with a `DisplayValue` wrapper when the
        // caller uses `%value` syntax. The Debug formatting of
        // `DisplayValue` wraps the string in quotes — we strip them
        // so the extracted value matches what the caller passed.
        let value_str = format!("{value:?}");
        let cleaned = strip_debug_quotes(&value_str);
        match name {
            "message" => self.message = Some(cleaned.to_string()),
            "request_id" => {
                if self.request_id.is_none() {
                    self.request_id = Some(cleaned.to_string());
                }
            }
            "trace_id" => {
                if self.trace_id.is_none() {
                    self.trace_id = Some(cleaned.to_string());
                }
            }
            _ => self.parts.push((name.to_string(), value_str)),
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        let name = field.name();
        match name {
            "message" => self.message = Some(value.to_string()),
            "request_id" => {
                if self.request_id.is_none() {
                    self.request_id = Some(value.to_string());
                }
            }
            "trace_id" => {
                if self.trace_id.is_none() {
                    self.trace_id = Some(value.to_string());
                }
            }
            _ => self.parts.push((name.to_string(), value.to_string())),
        }
    }
}

/// Strip surrounding quotes from a `{:?}`-formatted string. The
/// `tracing` macro's `%value` syntax emits the value via
/// `DisplayValue` whose `Debug` impl wraps the string in quotes:
/// `"req-abc123"` → `req-abc123`. This is a best-effort strip —
/// if the string doesn't start AND end with a quote, return it
/// unchanged.
fn strip_debug_quotes(s: &str) -> &str {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

fn format_parts(parts: &[(String, String)]) -> String {
    parts
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(" ")
}

impl MessageVisitor {
    fn into_message(self) -> String {
        let formatted_parts = format_parts(&self.parts);
        match (self.message, !formatted_parts.is_empty()) {
            (Some(msg), true) => format!("{msg}  {formatted_parts}"),
            (Some(msg), false) => msg,
            (None, true) => formatted_parts,
            (None, false) => "(no fields)".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tracing_subscriber::filter::LevelFilter;
    use tracing_subscriber::prelude::*;

    static TEST_MUTEX: Mutex<()> = Mutex::new(());

    #[tokio::test]
    async fn buffer_evicts_oldest_when_full() {
        let _test_lock = TEST_MUTEX.lock().unwrap();
        init();
        // Push BUFFER_CAPACITY + 10 entries; verify only the last
        // BUFFER_CAPACITY are kept. The snapshot is taken INSIDE
        // the same lock to avoid races with other tests that share
        // the global buffer.
        let snap = {
            let mut guard = DEBUG_LOG_BUFFER.lock();
            guard.entries.clear();
            guard.next_seq = 1;
            for i in 0..(BUFFER_CAPACITY + 10) {
                guard.push(DebugLogEntry {
                    seq: 0,
                    timestamp: DateTime::from_timestamp_millis(
                        1_767_225_600_000 + (i as i64 * 1000),
                    )
                    .unwrap(),
                    level: "WARN",
                    target: "test".into(),
                    message: format!("entry {i}"),
                    request_id: None,
                    trace_id: None,
                    span_path: None,
                });
            }
            guard.entries.iter().cloned().collect::<Vec<_>>()
        };
        assert_eq!(snap.len(), BUFFER_CAPACITY);
        // The oldest entry should be entry 10 (entries 0-9 were evicted).
        assert!(snap[0].message.contains("entry 10"));
        // The newest should be entry BUFFER_CAPACITY + 9.
        assert!(
            snap[snap.len() - 1]
                .message
                .contains(&format!("entry {}", BUFFER_CAPACITY + 9))
        );
    }

    #[tokio::test]
    async fn snapshot_since_filters_by_seq() {
        let _test_lock = TEST_MUTEX.lock().unwrap();
        init();
        let snap = {
            let mut guard = DEBUG_LOG_BUFFER.lock();
            guard.entries.clear();
            guard.next_seq = 1;
            for i in 0..5 {
                guard.push(DebugLogEntry {
                    seq: 0,
                    timestamp: DateTime::from_timestamp_millis(
                        1_767_225_600_000 + (i as i64 * 1000),
                    )
                    .unwrap(),
                    level: "INFO",
                    target: "test".into(),
                    message: format!("entry {i}"),
                    request_id: None,
                    trace_id: None,
                    span_path: None,
                });
            }
            // Take snapshot inside the lock to avoid races.
            guard
                .entries
                .iter()
                .filter(|e| e.seq > 2)
                .cloned()
                .collect::<Vec<_>>()
        };
        // Should return entries with seq > 2, i.e. seq 3, 4, 5.
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].seq, 3);
        assert_eq!(snap[2].seq, 5);
    }

    #[test]
    fn test_debug_log_entry_serialization() {
        let entry = DebugLogEntry {
            seq: 42,
            timestamp: chrono::DateTime::from_timestamp_millis(1_700_000_000_000).unwrap(),
            level: "WARN",
            target: "openproxy_pipeline::racing".to_string(),
            message: "race failed".to_string(),
            request_id: None,
            trace_id: None,
            span_path: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"timestamp\":\"2023-11-14T22:13:20Z\""));
        assert!(json.contains("\"level\":\"WARN\""));
        assert!(json.contains("\"seq\":42"));
    }

    // B1 (Bug 3): verify the DebugLogLayer captures WARN events
    // when wired into a real subscriber. This test does NOT call
    // `telemetry::init` (which would `try_init` a global subscriber
    // and pollute other tests); instead it uses
    // `tracing_subscriber::registry().with(...).set_default(...)`
    // to install a thread-local subscriber for the duration of the
    // scope. The previous tests pushed entries directly into the
    // buffer; this one exercises the full path from
    // `tracing::warn!(...)` → `DebugLogLayer::on_event` → buffer.
    //
    // We also verify that INFO events are NOT captured when the
    // layer is wrapped in a `LevelFilter::WARN` per-layer filter
    // (the production wiring in `telemetry::init`). This guards
    // against a future refactor that accidentally drops the
    // per-layer filter, which would re-introduce the bug where
    // `RUST_LOG=error` silences WARN from the ring buffer.
    #[tokio::test]
    async fn debug_log_layer_captures_warn_and_error_via_subscriber() {
        let _test_lock = TEST_MUTEX.lock().unwrap();
        init();
        // Reset the buffer to a known-empty state inside the lock
        // so we don't see entries from other tests that ran first.
        {
            let mut guard = DEBUG_LOG_BUFFER.lock();
            guard.entries.clear();
            guard.next_seq = 1;
        }
        let before = latest_seq();

        // Install a thread-local subscriber with the DebugLogLayer
        // wrapped in the same `LevelFilter::WARN` filter used by
        // `telemetry::init`. `set_default` returns a guard that
        // uninstalls the subscriber on drop — keeping the test
        // hermetic. The `tracing_subscriber::prelude::*` import
        // pulls in `SubscriberExt` (for `Registry::with`) and
        // `Layer` (for `with_filter`).
        let _guard = tracing_subscriber::registry()
            .with(DebugLogLayer.with_filter(LevelFilter::WARN))
            .set_default();

        tracing::warn!(
            target: "test::bug3",
            key = "warn-event",
            "simulated discovery tick failure",
        );
        tracing::error!(
            target: "test::bug3",
            key = "error-event",
            "simulated hard failure",
        );
        tracing::info!(
            target: "test::bug3",
            key = "info-event",
            "this should be filtered out by LevelFilter::WARN",
        );

        // Only WARN + ERROR should be in the buffer (INFO filtered out).
        let snap = snapshot_since(before)
            .into_iter()
            .filter(|s| s.target == "test::bug3")
            .collect::<Vec<_>>();
        assert_eq!(
            snap.len(),
            2,
            "expected exactly 2 entries (WARN + ERROR), got {snap:?}",
        );
        // Oldest-first ordering: WARN comes before ERROR.
        assert_eq!(snap[0].level, "WARN");
        assert_eq!(snap[1].level, "ERROR");
        assert!(snap[0].message.contains("simulated discovery tick failure"));
        assert!(snap[0].message.contains("key=warn-event"));
        assert!(snap[1].message.contains("simulated hard failure"));
        assert!(snap[1].message.contains("key=error-event"));
        assert_eq!(snap[0].target, "test::bug3");
    }

    #[tokio::test]
    async fn debug_log_layer_redacts_secrets() {
        let _test_lock = TEST_MUTEX.lock().unwrap();
        init();

        {
            let mut guard = DEBUG_LOG_BUFFER.lock();
            guard.entries.clear();
            guard.next_seq = 1;
        }
        let before = latest_seq();

        let _guard = tracing_subscriber::registry()
            .with(DebugLogLayer.with_filter(LevelFilter::WARN))
            .set_default();

        let secret = "sk-abcdefghij1234567890";
        tracing::error!(
            target: "test::redaction",
            "Error with secret: {}",
            secret
        );
        tracing::warn!(
            target: "test::redaction",
            header = format!("Authorization: Bearer {}", secret),
            "Failed to fetch",
        );

        let snap = snapshot_since(before)
            .into_iter()
            .filter(|s| s.target == "test::redaction")
            .collect::<Vec<_>>();

        assert_eq!(snap.len(), 2);

        // Ensure that the secret itself is NOT in the logs
        assert!(!snap[0].message.contains(secret));
        assert!(!snap[1].message.contains(secret));

        // Ensure that the message was redacted
        assert!(snap[0].message.contains("sk-[REDACTED]"));
        assert!(snap[1].message.contains("Bearer [REDACTED]"));
    }
}
