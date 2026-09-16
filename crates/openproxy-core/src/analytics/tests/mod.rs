use rusqlite::{Connection, params};
use std::path::PathBuf;

mod latency;
mod races;

pub(crate) fn fresh_conn() -> (Connection, PathBuf) {
    let conn = openproxy_db::testing::open_in_memory();
    (conn, PathBuf::from(":memory:"))
}

#[derive(Default)]
pub(crate) struct TestUsageParams<'a> {
    pub(crate) request_id: &'a str,
    pub(crate) trace_id: &'a str,
    pub(crate) provider: &'a str,
    pub(crate) model: &'a str,
    pub(crate) connect_ms: Option<i64>,
    pub(crate) ttft_ms: Option<i64>,
    pub(crate) total_ms: i64,
    pub(crate) tokens_per_sec: Option<f64>,
    pub(crate) race_total: i64,
    pub(crate) race_lost: bool,
    pub(crate) combo_target_id: Option<i64>,
    pub(crate) status_code: Option<i64>,
}

pub(crate) fn insert(conn: &Connection, p: TestUsageParams<'_>) {
    let status = p.status_code.unwrap_or(200);
    let race_total = if p.race_total == 0 { 1 } else { p.race_total };
    conn.execute(
        "INSERT INTO usage (\
            request_id, trace_id, attempt, provider_id, account_id, \
            upstream_model_id, combo_target_id, prompt_tokens, \
            completion_tokens, cost_usd, connect_ms, ttft_ms, total_ms, \
            tokens_per_sec, status_code, error_msg, error_msg_redacted, \
            race_total, race_lost, created_at\
         ) VALUES (\
            ?1, ?2, 1, ?3, NULL, ?4, ?5, 0, 0, 0.0, ?6, ?7, ?8, ?9, ?10, \
            NULL, NULL, ?11, ?12, datetime('now')\
         )",
        params![
            p.request_id,
            p.trace_id,
            p.provider,
            p.model,
            p.combo_target_id,
            p.connect_ms,
            p.ttft_ms,
            p.total_ms,
            p.tokens_per_sec,
            status,
            race_total,
            i64::from(p.race_lost),
        ],
    )
    .expect("insert");
}
