use super::*;
use crate::ids::*;
use crate::usage::*;
use openproxy_types::ids::ProviderId;
use openproxy_types::usage::USAGE_FLAG_RACE_LOST;

#[test]
fn recent_returns_rows_after_since_id() {
    let (conn, _p) = fresh_conn();
    let (r1, t1, r2, t2, r3, t3) = (
        RequestId::new().to_string(),
        TraceId::new().to_string(),
        RequestId::new().to_string(),
        TraceId::new().to_string(),
        RequestId::new().to_string(),
        TraceId::new().to_string(),
    );
    insert(
        &conn,
        TestUsageParams {
            request_id: &r1,
            trace_id: &t1,
            provider: "openrouter",
            model: "openai/gpt-4o",
            account: Some(1),
            status: 200,
            prompt: 10,
            completion: 5,
            cost: Some(0.01),
            ttft: Some(100),
            total: 600,
            ..Default::default()
        },
    );
    insert(
        &conn,
        TestUsageParams {
            request_id: &r2,
            trace_id: &t2,
            provider: "openrouter",
            model: "openai/gpt-4o",
            account: Some(2),
            status: 200,
            prompt: 20,
            completion: 10,
            cost: Some(0.02),
            ttft: Some(150),
            total: 700,
            race_lost: true,
            ..Default::default()
        },
    );
    insert(
        &conn,
        TestUsageParams {
            request_id: &r3,
            trace_id: &t3,
            provider: "anthropic",
            model: "claude-3.5-sonnet",
            account: Some(3),
            status: 500,
            prompt: 0,
            completion: 0,
            cost: Some(0.0),
            total: 800,
            err: Some("oops"),
            ..Default::default()
        },
    );

    let all = recent(&conn, 0, 50).expect("recent");
    assert_eq!(all.len(), 3);
    assert!(all.iter().map(|r| r.id.0).eq(1..=3));
    assert!(
        !all[0].has_flag(USAGE_FLAG_RACE_LOST)
            && all[1].has_flag(USAGE_FLAG_RACE_LOST)
            && !all[2].has_flag(USAGE_FLAG_RACE_LOST)
    );
    assert_eq!(all[0].prompt_tokens, Some(10));
    assert_eq!(all[0].completion_tokens, Some(5));
    assert!((all[0].cost_usd.unwrap() - 0.01).abs() < 1e-9);
    assert_eq!(all[2].cost_usd, Some(0.0));
    assert_eq!(all[2].status_code, 500);

    let after = recent(&conn, 1, 50).expect("recent");
    assert_eq!(after.len(), 2);
    assert!(after.iter().map(|r| r.id.0).eq(2..=3));
    assert!(recent(&conn, 999, 50).expect("recent").is_empty());
    let capped = recent(&conn, 0, 2).expect("recent");
    assert_eq!(capped.len(), 2);
    assert!(capped.iter().map(|r| r.id.0).eq(1..=2));
}

#[test]
fn recent_desc_returns_newest_first() {
    let (conn, _p) = fresh_conn();
    let (r1, t1, r2, t2, r3, t3) = (
        RequestId::new().to_string(),
        TraceId::new().to_string(),
        RequestId::new().to_string(),
        TraceId::new().to_string(),
        RequestId::new().to_string(),
        TraceId::new().to_string(),
    );
    insert(
        &conn,
        TestUsageParams {
            request_id: &r1,
            trace_id: &t1,
            provider: "openrouter",
            model: "openai/gpt-4o",
            account: Some(1),
            status: 200,
            prompt: 10,
            completion: 5,
            cost: Some(0.01),
            ttft: Some(100),
            total: 600,
            ..Default::default()
        },
    );
    insert(
        &conn,
        TestUsageParams {
            request_id: &r2,
            trace_id: &t2,
            provider: "openrouter",
            model: "openai/gpt-4o",
            account: Some(2),
            status: 200,
            prompt: 20,
            completion: 10,
            cost: Some(0.02),
            ttft: Some(150),
            total: 700,
            race_lost: true,
            ..Default::default()
        },
    );
    insert(
        &conn,
        TestUsageParams {
            request_id: &r3,
            trace_id: &t3,
            provider: "anthropic",
            model: "claude-3.5-sonnet",
            account: Some(3),
            status: 500,
            prompt: 0,
            completion: 0,
            cost: Some(0.0),
            total: 800,
            err: Some("oops"),
            ..Default::default()
        },
    );

    let rows = recent_desc(&conn, 2).expect("recent_desc");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].id.0, 3);
    assert_eq!(rows[1].id.0, 2);
}

#[test]
fn recent_aggregates_retry_attempts_by_request_id() {
    let (conn, _p) = fresh_conn();
    let shared_req = RequestId::new().to_string();
    insert(
        &conn,
        TestUsageParams {
            request_id: &shared_req,
            trace_id: "trace-a",
            provider: "openrouter",
            model: "openai/gpt-4o",
            account: Some(1),
            status: 502,
            prompt: 10,
            completion: 0,
            cost: Some(0.0),
            ttft: Some(100),
            total: 600,
            err: Some("upstream 502"),
            ..Default::default()
        },
    );
    insert(
        &conn,
        TestUsageParams {
            request_id: &shared_req,
            trace_id: "trace-b",
            provider: "openrouter",
            model: "openai/gpt-4o",
            account: Some(2),
            status: 429,
            prompt: 10,
            completion: 0,
            cost: Some(0.0),
            ttft: Some(100),
            total: 600,
            err: Some("rate limited"),
            ..Default::default()
        },
    );
    insert(
        &conn,
        TestUsageParams {
            request_id: &shared_req,
            trace_id: "trace-c",
            provider: "openrouter",
            model: "openai/gpt-4o",
            account: Some(3),
            status: 200,
            prompt: 100,
            completion: 50,
            cost: Some(0.03),
            ttft: Some(150),
            total: 700,
            race_lost: true,
            ..Default::default()
        },
    );

    let rows = recent(&conn, 0, 50).expect("recent");
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].trace_id, "trace-a");
    assert_eq!(rows[0].status_code, 502);
    assert_eq!(rows[1].trace_id, "trace-b");
    assert_eq!(rows[1].status_code, 429);
    assert_eq!(rows[2].trace_id, "trace-c");
    assert_eq!(rows[2].status_code, 200);
    assert_eq!(rows[2].prompt_tokens, Some(100));
    assert_eq!(rows[2].completion_tokens, Some(50));
    assert!((rows[2].cost_usd.unwrap() - 0.03).abs() < 1e-9);
}

#[test]
fn recent_desc_aggregates_retry_attempts_by_request_id() {
    let (conn, _p) = fresh_conn();
    let shared_req = RequestId::new().to_string();
    insert(
        &conn,
        TestUsageParams {
            request_id: &shared_req,
            trace_id: "trace-a",
            provider: "openrouter",
            model: "openai/gpt-4o",
            account: Some(1),
            status: 502,
            prompt: 10,
            completion: 0,
            cost: Some(0.0),
            ttft: Some(100),
            total: 600,
            err: Some("upstream 502"),
            ..Default::default()
        },
    );
    insert(
        &conn,
        TestUsageParams {
            request_id: &shared_req,
            trace_id: "trace-b",
            provider: "openrouter",
            model: "openai/gpt-4o",
            account: Some(2),
            status: 200,
            prompt: 100,
            completion: 50,
            cost: Some(0.03),
            ttft: Some(150),
            total: 700,
            race_lost: true,
            ..Default::default()
        },
    );
    let rows = recent_desc(&conn, 50).expect("recent_desc");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].trace_id, "trace-b");
    assert_eq!(rows[0].status_code, 200);
    assert_eq!(rows[1].trace_id, "trace-a");
    assert_eq!(rows[1].status_code, 502);
}

#[test]
fn recent_desc_includes_compression_columns() {
    let (conn, _p) = fresh_conn();
    conn.execute(
        "INSERT INTO usage (request_id, trace_id, attempt, provider_id, account_id, upstream_model_id, prompt_tokens, completion_tokens, cost_usd, connect_ms, ttft_ms, total_ms, status_code, error_msg, error_msg_redacted, race_total, race_lost, created_at, compression_savings_pct, compression_techniques) \
         VALUES (?1, ?2, 1, 'openrouter', 1, 'openai/gpt-4o', 100, 50, 0.01, 50, 200, 1200, 200, NULL, NULL, 1, 0, datetime('now'), 42.0, 'lite::collapse_whitespace')",
        params![RequestId::new().to_string(), TraceId::new().to_string()],
    ).expect("insert compression");
    let row_id = conn.last_insert_rowid();

    let rows = recent_desc(&conn, 10).expect("recent_desc");
    let row = rows.iter().find(|r| r.id.0 == row_id).expect("found row");
    assert_eq!(row.compression_savings_pct, Some(42.0));
    assert_eq!(
        row.compression_techniques.as_deref(),
        Some("lite::collapse_whitespace")
    );
}

#[test]
fn detail_by_id_returns_full_usage_row() {
    let (conn, _p) = fresh_conn();
    let (req, trace) = (RequestId::new().to_string(), TraceId::new().to_string());
    conn.execute(
        "INSERT INTO usage (request_id, trace_id, attempt, provider_id, account_id, combo_id, model_row_id, upstream_model_id, combo_target_id, prompt_tokens, completion_tokens, connect_ms, ttft_ms, total_ms, tokens_per_sec, status_code, error_msg, error_msg_redacted, race_total, race_lost, api_key_id, created_at) \
         VALUES (?1, ?2, 2, 'openrouter', 1, 7, 9, 'openai/gpt-4o', 11, 100, 50, 25, 200, 1200, 50.0, 500, 'raw secret', 'raw secret', 3, 1, NULL, datetime('now'))",
        params![req, trace],
    ).expect("insert detail");
    let id = conn.last_insert_rowid();

    let row = detail_by_id(&conn, id)
        .expect("detail_by_id")
        .expect("row exists");
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
    assert_eq!(row.race_total, 3);
    assert!(row.has_flag(USAGE_FLAG_RACE_LOST));
    assert!(detail_by_id(&conn, id + 1).expect("missing").is_none());
}

#[test]
fn prune_expired_recording_bodies_clears_old_rows() {
    let (conn, _p) = fresh_conn();
    conn.execute("INSERT INTO usage (request_id, trace_id, attempt, provider_id, upstream_model_id, prompt_tokens, completion_tokens, cost_usd, connect_ms, ttft_ms, total_ms, status_code, race_total, race_lost, created_at, request_body_json, response_body_json, request_headers, response_headers) VALUES ('req1', 'trace1', 1, 'openrouter', 'openai/gpt-4o', 100, 50, 0.01, 50, 200, 1200, 200, 1, 0, datetime('now'), '{\"q\":\"hello\"}', '{\"a\":\"world\"}', '{\"ct\":\"text/plain\"}', '{\"ct\":\"text/plain\"}')", []).expect("insert recent");
    conn.execute("INSERT INTO usage (request_id, trace_id, attempt, provider_id, upstream_model_id, prompt_tokens, completion_tokens, cost_usd, connect_ms, ttft_ms, total_ms, status_code, race_total, race_lost, created_at, request_body_json, response_body_json, request_headers, response_headers) VALUES ('req2', 'trace2', 1, 'openrouter', 'openai/gpt-4o', 100, 50, 0.01, 50, 200, 1200, 200, 1, 0, datetime('now', '-600 seconds'), '{\"q\":\"old\"}', '{\"a\":\"old\"}', '{\"ct\":\"old\"}', '{\"ct\":\"old\"}')", []).expect("insert old");

    assert_eq!(
        prune_expired_recording_bodies(&conn, 300).expect("prune"),
        1
    );
    let recent: Option<String> = conn
        .query_row(
            "SELECT request_body_json FROM usage WHERE request_id = 'req1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let old: Option<String> = conn
        .query_row(
            "SELECT request_body_json FROM usage WHERE request_id = 'req2'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(recent.is_some() && old.is_none());
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM usage WHERE status_code = 200",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 2);
}

#[test]
fn prune_expired_recording_bodies_zero_ttl_clears_all() {
    let (conn, _p) = fresh_conn();
    conn.execute("INSERT INTO usage (request_id, trace_id, attempt, provider_id, upstream_model_id, prompt_tokens, completion_tokens, cost_usd, connect_ms, ttft_ms, total_ms, status_code, race_total, race_lost, created_at, request_body_json, response_body_json) VALUES ('req1', 'trace1', 1, 'openrouter', 'openai/gpt-4o', 100, 50, 0.01, 50, 200, 1200, 200, 1, 0, datetime('now'), '{\"q\":\"hi\"}', '{\"a\":\"ok\"}')", []).unwrap();
    assert_eq!(prune_expired_recording_bodies(&conn, 0).expect("prune"), 1);
    let recent_now: Option<String> = conn
        .query_row(
            "SELECT request_body_json FROM usage WHERE request_id = 'req1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(recent_now.is_none());
}
