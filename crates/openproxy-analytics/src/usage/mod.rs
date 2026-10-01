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
//! clients. The sender is stored in a `std::sync::OnceLock` so it is

pub mod analytics;

pub use analytics::prune_expired_recording_bodies;
pub use analytics::prune_expired_usage_rows;
pub use analytics::*;

use std::sync::{LazyLock, OnceLock};

static USAGE_SENDER: OnceLock<tokio::sync::broadcast::Sender<openproxy_types::RecentUsageRow>> =
    OnceLock::new();
static STAGE_SENDER: OnceLock<tokio::sync::broadcast::Sender<openproxy_types::usage::StageEvent>> =
    OnceLock::new();

pub static INFLIGHT_REGISTRY: LazyLock<
    dashmap::DashMap<String, openproxy_types::usage::InflightAttempt>,
> = LazyLock::new(dashmap::DashMap::new);

pub fn get_active_inflight_attempts() -> Vec<openproxy_types::usage::InflightAttempt> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    // Descarta intentos de más de 30m
    INFLIGHT_REGISTRY.retain(|_, v| now.saturating_sub(v.updated_at_ms) < 1_800_000);

    let mut list: Vec<_> = INFLIGHT_REGISTRY
        .iter()
        .map(|r| r.value().to_owned())
        .collect();
    list.sort_by_key(|b| std::cmp::Reverse(b.started_at_ms));
    list
}

fn stage_rank(stage: &str) -> u8 {
    match stage {
        "started" => 0,
        "connecting" => 1,
        "waiting_ttft" => 2,
        "streaming" => 3,
        "completed" | "failed" | "cancelled" => 4,
        _ => 0,
    }
}

pub fn init_usage_broadcast() -> tokio::sync::broadcast::Sender<openproxy_types::RecentUsageRow> {
    let (tx, _rx) = tokio::sync::broadcast::channel(1024);
    let _ = USAGE_SENDER.set(tokio::sync::broadcast::Sender::clone(&tx));
    let _ = openproxy_types::usage::USAGE_ROW_PUBLISHER.set(publish_usage_global);
    tx
}

pub fn init_stage_broadcast() -> tokio::sync::broadcast::Sender<openproxy_types::usage::StageEvent>
{
    let (tx, _rx) = tokio::sync::broadcast::channel(200);
    let _ = STAGE_SENDER.set(tokio::sync::broadcast::Sender::clone(&tx));
    let _ = openproxy_types::usage::STAGE_EVENT_PUBLISHER.set(publish_stage_global);
    tx
}

#[inline]
fn with_attempt_key<R>(trace_id: &str, request_id: &str, f: impl FnOnce(&str) -> R) -> R {
    if !trace_id.is_empty() {
        f(trace_id)
    } else {
        let mut buf = [0u8; 96];
        if request_id.len() + 8 <= buf.len() {
            buf[..request_id.len()].copy_from_slice(request_id.as_bytes());
            buf[request_id.len()..request_id.len() + 8].copy_from_slice(b":unknown");
            if let Ok(s) = std::str::from_utf8(&buf[..request_id.len() + 8]) {
                return f(s);
            }
        }
        let mut s = String::with_capacity(request_id.len() + 8);
        use std::fmt::Write;
        let _ = write!(&mut s, "{request_id}:unknown");
        f(&s)
    }
}

fn publish_usage_global(row: openproxy_types::RecentUsageRow) {
    with_attempt_key(&row.trace_id, &row.request_id, |key| {
        INFLIGHT_REGISTRY.remove(key);
    });

    if let Some(tx) = USAGE_SENDER.get() {
        let _ = tx.send(openproxy_types::usage::redact_for_broadcast(row));
    }
}

fn is_terminal_stage_event(event: &openproxy_types::usage::StageEvent) -> bool {
    matches!(
        event.stage.as_str(),
        "completed" | "failed" | "cancelled" | "predict_skipped"
    ) || event.status_code.is_some_and(|s| s >= 400 && s != 0)
        || event.error.is_some()
}

fn update_inflight_attempt(
    item: &mut openproxy_types::usage::InflightAttempt,
    event: &openproxy_types::usage::StageEvent,
    rank: u8,
    now_ms: u64,
) {
    item.stage = event.stage.clone();
    item.stage_rank = rank;
    item.updated_at_ms = now_ms;
    item.elapsed_ms_at_event = event.elapsed_ms;
    if let Some(c) = event.connect_ms {
        item.connect_ms = Some(c);
    }
    if let Some(t) = event.ttft_ms {
        item.ttft_ms = Some(t);
    }
    if event.status_code.is_some() {
        item.status_code = event.status_code;
    }
    if event.error.is_some() {
        item.error = event.error.clone();
    }
    if let Some(p) = &event.provider_id
        && !p.is_empty()
    {
        item.provider_id = p.to_owned();
    }
    if let Some(m) = &event.upstream_model_id
        && !m.is_empty()
    {
        item.upstream_model_id = m.to_owned();
    }
    if event.endpoint_kind.is_some() {
        item.endpoint_kind = event.endpoint_kind;
    }
}

fn publish_stage_global(event: openproxy_types::usage::StageEvent) {
    if is_terminal_stage_event(&event) {
        with_attempt_key(&event.trace_id, &event.request_id, |key| {
            INFLIGHT_REGISTRY.remove(key);
        });
    } else {
        let rank = stage_rank(&event.stage);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let updated = with_attempt_key(&event.trace_id, &event.request_id, |key| {
            if let Some(mut item) = INFLIGHT_REGISTRY.get_mut(key) {
                update_inflight_attempt(&mut item, &event, rank, now_ms);
                true
            } else {
                false
            }
        });

        if !updated {
            let attempt_key = if event.trace_id.is_empty() {
                let mut s = String::with_capacity(event.request_id.len() + 8);
                s.push_str(&event.request_id);
                s.push_str(":unknown");
                s
            } else {
                event.trace_id.clone()
            };
            let started_at = now_ms.saturating_sub(event.elapsed_ms);
            let status_opt = event.status_code;

            INFLIGHT_REGISTRY.insert(
                attempt_key.clone(),
                openproxy_types::usage::InflightAttempt {
                    attempt_key,
                    request_id: event.request_id.clone(),
                    trace_id: event.trace_id.clone(),
                    provider_id: event.provider_id.as_deref().unwrap_or_default().to_string(),
                    upstream_model_id: event
                        .upstream_model_id
                        .as_deref()
                        .unwrap_or_default()
                        .to_string(),
                    started_at_ms: started_at,
                    updated_at_ms: now_ms,
                    stage: event.stage.clone(),
                    stage_seq: event.elapsed_ms as u32,
                    stage_rank: rank,
                    elapsed_ms_at_event: event.elapsed_ms,
                    connect_ms: event.connect_ms,
                    ttft_ms: event.ttft_ms,
                    status_code: status_opt,
                    terminal: false,
                    terminal_kind: None,
                    error: event.error.clone(),
                    row_id: None,
                    source: "live".into(),
                    endpoint_kind: event.endpoint_kind,
                },
            );
        }
    }

    if let Some(tx) = STAGE_SENDER.get() {
        let _ = tx.send(event);
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod inflight_tests {
    use super::*;

    #[test]
    fn test_with_attempt_key() {
        with_attempt_key("tr-123", "req-456", |k| assert_eq!(k, "tr-123"));
        with_attempt_key("", "req-456", |k| assert_eq!(k, "req-456:unknown"));
        let long_id = "a".repeat(120);
        with_attempt_key("", &long_id, |k| {
            assert_eq!(k, format!("{long_id}:unknown"))
        });
    }
}
