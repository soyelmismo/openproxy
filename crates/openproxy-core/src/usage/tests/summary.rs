use super::*;
use crate::ids::*;
use crate::usage::*;
use openproxy_types::ids::ProviderId;

#[test]
fn summary_basic() {
    let (conn, _p) = fresh_conn();
    let (req, t1, t2, t3) = (
        RequestId::new().to_string(),
        TraceId::new().to_string(),
        TraceId::new().to_string(),
        TraceId::new().to_string(),
    );
    insert(
        &conn,
        TestUsageParams {
            request_id: &req,
            trace_id: &t1,
            provider: "openrouter",
            model: "openai/gpt-4o",
            account: Some(1),
            status: 200,
            prompt: 100,
            completion: 50,
            cost: Some(0.01),
            ttft: Some(200),
            total: 1200,
            ..Default::default()
        },
    );
    insert(
        &conn,
        TestUsageParams {
            request_id: &req,
            trace_id: &t2,
            provider: "openrouter",
            model: "openai/gpt-4o",
            account: Some(2),
            status: 200,
            prompt: 100,
            completion: 50,
            cost: Some(0.01),
            ttft: Some(200),
            total: 1200,
            race_lost: true,
            ..Default::default()
        },
    );
    insert(
        &conn,
        TestUsageParams {
            request_id: &req,
            trace_id: &t3,
            provider: "openrouter",
            model: "openai/gpt-4o",
            account: Some(3),
            status: 200,
            prompt: 100,
            completion: 50,
            cost: Some(0.01),
            ttft: Some(200),
            total: 1200,
            race_lost: true,
            ..Default::default()
        },
    );

    let s = summary(&conn, &UsageFilter::default()).expect("summary");
    assert_eq!(s.unique_requests, 1);
    assert_eq!(s.total_rows, 3);
    assert_eq!(s.total_attempts, 3);
    assert_eq!(s.winners, 1);
    assert_eq!(s.losers, 2);
    assert_eq!(s.errors, 0);
    assert_eq!(s.total_prompt_tokens, 300);
    assert_eq!(s.total_completion_tokens, 150);
    assert!((s.total_cost_usd - 0.03).abs() < 1e-9);
    assert_eq!(s.avg_ttft_ms, Some(200.0));
    assert!((s.avg_total_ms - 1200.0).abs() < 1e-9);
}

#[test]
fn summary_with_provider_filter() {
    let (conn, _p) = fresh_conn();
    let (r1, r2, r3) = (
        RequestId::new().to_string(),
        RequestId::new().to_string(),
        RequestId::new().to_string(),
    );
    insert(
        &conn,
        TestUsageParams {
            request_id: &r1,
            trace_id: &TraceId::new().to_string(),
            provider: "openrouter",
            model: "openai/gpt-4o",
            account: Some(1),
            status: 200,
            prompt: 10,
            completion: 5,
            cost: Some(0.001),
            ttft: Some(100),
            total: 600,
            ..Default::default()
        },
    );
    insert(
        &conn,
        TestUsageParams {
            request_id: &r2,
            trace_id: &TraceId::new().to_string(),
            provider: "openrouter",
            model: "openai/gpt-4o",
            account: Some(2),
            status: 200,
            prompt: 10,
            completion: 5,
            cost: Some(0.001),
            ttft: Some(100),
            total: 600,
            ..Default::default()
        },
    );
    insert(
        &conn,
        TestUsageParams {
            request_id: &r3,
            trace_id: &TraceId::new().to_string(),
            provider: "anthropic",
            model: "claude-3.5-sonnet",
            account: Some(3),
            status: 200,
            prompt: 10,
            completion: 5,
            cost: Some(0.001),
            ttft: Some(100),
            total: 600,
            ..Default::default()
        },
    );

    let s = summary(
        &conn,
        &UsageFilter {
            provider_id: Some(ProviderId::new("openrouter")),
            ..Default::default()
        },
    )
    .expect("filtered");
    assert_eq!(s.unique_requests, 2);
    assert_eq!(s.total_rows, 2);
    assert_eq!(s.total_cost_usd, 0.002);

    let s_all = summary(&conn, &UsageFilter::default()).expect("unfiltered");
    assert_eq!(s_all.unique_requests, 3);
    assert_eq!(s_all.total_rows, 3);
}

#[test]
fn summary_with_combo_and_model_filter() {
    let (conn, _p) = fresh_conn();
    conn.execute("INSERT INTO usage (request_id, trace_id, attempt, provider_id, account_id, combo_id, upstream_model_id, prompt_tokens, completion_tokens, cost_usd, connect_ms, ttft_ms, total_ms, status_code, created_at) VALUES ('r1', 't1', 1, 'openrouter', 1, 7, 'openai/gpt-4o', 10, 5, 0.01, 50, 100, 600, 200, datetime('now'))", []).unwrap();
    conn.execute("INSERT INTO usage (request_id, trace_id, attempt, provider_id, account_id, combo_id, upstream_model_id, prompt_tokens, completion_tokens, cost_usd, connect_ms, ttft_ms, total_ms, status_code, created_at) VALUES ('r2', 't2', 1, 'openrouter', 1, 8, 'openai/gpt-4o-mini', 10, 5, 0.02, 50, 100, 600, 200, datetime('now'))", []).unwrap();

    let s = summary(
        &conn,
        &UsageFilter {
            combo_id: Some(ComboId(7)),
            model_id: Some("openai/gpt-4o".to_string()),
            ..Default::default()
        },
    )
    .expect("summary");
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
    assert_eq!(s.avg_ttft_ms, None);
    assert_eq!(s.avg_total_ms, 0.0);
}

#[test]
fn avg_ttft_skips_null_rows() {
    let (conn, _p) = fresh_conn();
    insert(
        &conn,
        TestUsageParams {
            request_id: "req1",
            trace_id: "tr1",
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
            request_id: "req2",
            trace_id: "tr2",
            provider: "openrouter",
            model: "openai/gpt-4o",
            account: Some(1),
            status: 200,
            prompt: 10,
            completion: 5,
            cost: Some(0.01),
            ttft: None,
            total: 600,
            race_lost: true,
            err: Some("race lost"),
        },
    );
    let s = summary(&conn, &UsageFilter::default()).expect("summary");
    assert_eq!(s.avg_ttft_ms, Some(100.0));
}

#[test]
fn summary_counts_rows_with_null_pricing() {
    let (conn, _p) = fresh_conn();
    insert(
        &conn,
        TestUsageParams {
            request_id: "r1",
            trace_id: "t1",
            provider: "openrouter",
            model: "openai/gpt-4o",
            account: Some(1),
            status: 200,
            prompt: 100,
            completion: 50,
            cost: Some(0.01),
            ttft: Some(100),
            total: 600,
            ..Default::default()
        },
    );
    insert(
        &conn,
        TestUsageParams {
            request_id: "r2",
            trace_id: "t2",
            provider: "openrouter",
            model: "openai/gpt-4o",
            account: Some(1),
            status: 200,
            prompt: 200,
            completion: 50,
            cost: None,
            ttft: Some(100),
            total: 600,
            ..Default::default()
        },
    );
    insert(
        &conn,
        TestUsageParams {
            request_id: "r3",
            trace_id: "t3",
            provider: "anthropic",
            model: "claude-3.5-sonnet",
            account: Some(2),
            status: 200,
            prompt: 300,
            completion: 0,
            cost: None,
            ttft: Some(100),
            total: 600,
            ..Default::default()
        },
    );
    insert(
        &conn,
        TestUsageParams {
            request_id: "r4",
            trace_id: "t4",
            provider: "openrouter",
            model: "openai/gpt-4o",
            account: Some(1),
            status: 500,
            prompt: 0,
            completion: 0,
            cost: Some(0.00),
            ttft: Some(100),
            total: 600,
            err: Some("err"),
            ..Default::default()
        },
    );

    let s = summary(&conn, &UsageFilter::default()).expect("summary");
    assert_eq!(s.total_rows, 4);
    assert_eq!(s.rows_with_null_pricing, 2);

    let s_filt = summary(
        &conn,
        &UsageFilter {
            provider_id: Some(ProviderId::new("anthropic")),
            ..Default::default()
        },
    )
    .expect("filtered");
    assert_eq!(s_filt.total_rows, 1);
    assert_eq!(s_filt.rows_with_null_pricing, 1);

    let (conn2, _p2) = fresh_conn();
    assert_eq!(
        summary(&conn2, &UsageFilter::default())
            .expect("empty")
            .rows_with_null_pricing,
        0
    );
}

#[test]
fn date_filter_matches_space_separated_created_at() {
    let (conn, _p) = fresh_conn();
    conn.execute("INSERT INTO usage (request_id, trace_id, attempt, provider_id, account_id, upstream_model_id, prompt_tokens, completion_tokens, cost_usd, connect_ms, ttft_ms, total_ms, status_code, created_at) VALUES ('r1', 't1', 1, 'openrouter', 1, 'm', 100, 50, 0.10, 50, 200, 1200, 200, '2026-06-15 12:00:00')", []).unwrap();

    let f = UsageFilter {
        from: Some("2026-06-15T00:00:00Z".to_string()),
        to: Some("2026-06-16T00:00:00Z".to_string()),
        ..Default::default()
    };
    let s = summary(&conn, &f).expect("summary");
    assert_eq!(s.total_rows, 1);
    assert_eq!(s.unique_requests, 1);
    assert!((s.total_cost_usd - 0.10).abs() < 1e-9);

    let f_other = UsageFilter {
        from: Some("2026-06-16T00:00:00Z".to_string()),
        to: Some("2026-06-17T00:00:00Z".to_string()),
        ..Default::default()
    };
    assert_eq!(summary(&conn, &f_other).expect("other day").total_rows, 0);
}
