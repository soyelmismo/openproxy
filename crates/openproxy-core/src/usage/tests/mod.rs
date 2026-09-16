pub(crate) mod breakdowns;
pub(crate) mod by_status_errors;
pub(crate) mod recent_feed;
pub(crate) mod summary;

use rusqlite::{Connection, params};
use std::path::PathBuf;

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
    pub(crate) account: Option<i64>,
    pub(crate) status: u16,
    pub(crate) prompt: i64,
    pub(crate) completion: i64,
    pub(crate) cost: Option<f64>,
    pub(crate) ttft: Option<i64>,
    pub(crate) total: i64,
    pub(crate) race_lost: bool,
    pub(crate) err: Option<&'a str>,
}

pub(crate) fn insert(conn: &Connection, p: TestUsageParams<'_>) {
    let status = if p.status == 0 { 200 } else { p.status };
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
            p.request_id,
            p.trace_id,
            p.provider,
            p.account,
            p.model,
            p.prompt,
            p.completion,
            p.cost,
            p.ttft,
            p.total,
            i64::from(status),
            p.err,
            i64::from(p.race_lost),
        ],
    )
    .expect("insert");
}
