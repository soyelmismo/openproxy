#![cfg(test)]

use rusqlite::Connection;
use std::sync::Arc;

use super::{LatencyBudget, ModelRowId};

/// Minimal usage schema mirroring the production columns the query
/// touches, plus prompt_tokens_estimated (correction 5) and error_msg.
pub(crate) fn test_conn() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE usage (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            request_id TEXT, trace_id TEXT, attempt INTEGER DEFAULT 1,
            provider_id TEXT NOT NULL, account_id INTEGER,
            combo_id INTEGER, model_row_id INTEGER,
            upstream_model_id TEXT NOT NULL DEFAULT '', combo_target_id INTEGER,
            prompt_tokens INTEGER, prompt_tokens_estimated INTEGER,
            session_hash INTEGER,
            completion_tokens INTEGER, cached_tokens INTEGER,
            cost_usd REAL, status_code INTEGER NOT NULL,
            error_msg TEXT, created_at TEXT NOT NULL DEFAULT (datetime('now')),
            connect_ms INTEGER, ttft_ms INTEGER, total_ms INTEGER,
            max_chunk_gap_ms INTEGER, avg_chunk_gap_ms INTEGER
         );",
    )
    .unwrap();
    conn
}

/// Shared mutex connection: the sync-mode predictor locks it itself.
pub(crate) fn shared_conn(conn: Connection) -> Arc<parking_lot::Mutex<Connection>> {
    Arc::new(parking_lot::Mutex::new(conn))
}

pub(crate) fn insert(c: &Connection, provider: &str, model_row: i64, ttft: i64, prompt: i64) {
    insert_at(c, provider, model_row, ttft, prompt, "datetime('now')");
}

pub(crate) fn insert_est(
    c: &Connection,
    provider: &str,
    model_row: i64,
    ttft: i64,
    prompt: i64,
    estimated: i64,
) {
    c.execute(
        "INSERT INTO usage (provider_id, model_row_id, upstream_model_id, status_code, ttft_ms, prompt_tokens, prompt_tokens_estimated, created_at) \
         VALUES (?1, ?2, 'm', 200, ?3, ?4, ?5, datetime('now'))",
        rusqlite::params![provider, model_row, ttft, prompt, estimated],
    )
    .unwrap();
}

pub(crate) fn insert_at(
    c: &Connection,
    provider: &str,
    model_row: i64,
    ttft: i64,
    prompt: i64,
    created_at: &str,
) {
    c.execute(
        &format!(
            "INSERT INTO usage (provider_id, model_row_id, upstream_model_id, status_code, ttft_ms, prompt_tokens, created_at) \
             VALUES (?1, ?2, 'm', 200, ?3, ?4, {created_at})"
        ),
        rusqlite::params![provider, model_row, ttft, prompt],
    )
    .unwrap();
}

pub(crate) fn insert_sess(
    c: &Connection,
    provider: &str,
    model_row: i64,
    session_hash: i64,
    ttft: i64,
    prompt: i64,
) {
    insert_sess_at(
        c,
        provider,
        model_row,
        session_hash,
        ttft,
        prompt,
        "datetime('now')",
    );
}

pub(crate) fn insert_sess_at(
    c: &Connection,
    provider: &str,
    model_row: i64,
    session_hash: i64,
    ttft: i64,
    prompt: i64,
    created_at: &str,
) {
    c.execute(
        &format!(
            "INSERT INTO usage (provider_id, model_row_id, upstream_model_id, status_code, \
                                ttft_ms, prompt_tokens, session_hash, created_at) \
             VALUES (?1, ?2, 'm', 200, ?3, ?4, ?5, {created_at})"
        ),
        rusqlite::params![provider, model_row, ttft, prompt, session_hash],
    )
    .unwrap();
}

/// Inserts a headers-phase timeout row exactly as the usage tracker
/// records it (correction 1): status 0, NULL ttft, parseable error_msg.
/// `phase` is `"ttft"` (streaming, current) or `"headers"` (legacy/unary).
pub(crate) fn insert_cut_named(
    c: &Connection,
    provider: &str,
    model_row: i64,
    waited_ms: u64,
    phase: &str,
) {
    c.execute(
        "INSERT INTO usage (provider_id, model_row_id, upstream_model_id, status_code, ttft_ms, prompt_tokens, error_msg, created_at) \
         VALUES (?1, ?2, 'm', 0, NULL, 5000, ?3, datetime('now'))",
        rusqlite::params![
            provider,
            model_row,
            format!("upstream timeout in phase {phase} after {waited_ms}ms")
        ],
    )
    .unwrap();
}

pub(crate) fn insert_cut(c: &Connection, provider: &str, model_row: i64, waited_ms: u64) {
    insert_cut_named(c, provider, model_row, waited_ms, "headers");
}

pub(crate) fn budget_of(
    b: &Arc<LatencyBudget>,
    c: &parking_lot::Mutex<Connection>,
    provider: &str,
    row: i64,
    est: u32,
    ceiling: u64,
) -> Option<u64> {
    budget_of_sess(b, c, provider, row, None, est, ceiling)
}

pub(crate) fn budget_of_sess(
    b: &Arc<LatencyBudget>,
    c: &parking_lot::Mutex<Connection>,
    provider: &str,
    row: i64,
    session_hash: Option<u64>,
    est: u32,
    ceiling: u64,
) -> Option<u64> {
    b.ttft_budget_ms(
        provider,
        ModelRowId(row),
        session_hash,
        est,
        ceiling,
        Some(c),
    )
}
