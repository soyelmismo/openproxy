use super::*;
use crate::ids::*;
use crate::usage::*;
use openproxy_types::ids::ProviderId;

#[test]
fn by_model_groups_by_provider_and_model() {
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
            cost: Some(0.5),
            ttft: Some(200),
            total: 1200,
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
            prompt: 100,
            completion: 50,
            cost: Some(0.5),
            ttft: Some(200),
            total: 1200,
            ..Default::default()
        },
    );
    insert(
        &conn,
        TestUsageParams {
            request_id: "r3",
            trace_id: "t3",
            provider: "openrouter",
            model: "openai/gpt-4o-mini",
            account: Some(1),
            status: 200,
            prompt: 100,
            completion: 50,
            cost: Some(0.05),
            ttft: Some(200),
            total: 600,
            ..Default::default()
        },
    );
    insert(
        &conn,
        TestUsageParams {
            request_id: "r4",
            trace_id: "t4",
            provider: "anthropic",
            model: "claude-3.5-sonnet",
            account: Some(2),
            status: 200,
            prompt: 100,
            completion: 50,
            cost: Some(1.0),
            ttft: Some(200),
            total: 1500,
            ..Default::default()
        },
    );

    let rows = by_model(&conn, &UsageFilter::default()).expect("by_model");
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].upstream_model_id, "claude-3.5-sonnet");
    assert!((rows[0].total_cost_usd - 1.0).abs() < 1e-9);
    assert_eq!(rows[0].unique_requests, 1);
    assert_eq!(rows[0].winners, 1);

    let gpt = rows
        .iter()
        .find(|r| r.upstream_model_id == "openai/gpt-4o")
        .unwrap();
    assert_eq!(gpt.unique_requests, 2);
    assert_eq!(gpt.total_rows, 2);
    assert!((gpt.total_cost_usd - 1.0).abs() < 1e-9);
    assert_eq!(gpt.provider_id, ProviderId::new("openrouter"));

    let mini = rows
        .iter()
        .find(|r| r.upstream_model_id == "openai/gpt-4o-mini")
        .unwrap();
    assert_eq!(mini.total_rows, 1);
    assert!((mini.total_cost_usd - 0.05).abs() < 1e-9);
}

#[test]
fn by_account_groups_by_account() {
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
            prompt: 10,
            completion: 5,
            cost: Some(0.10),
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
            prompt: 10,
            completion: 5,
            cost: Some(0.10),
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
            provider: "openrouter",
            model: "openai/gpt-4o",
            account: Some(2),
            status: 200,
            prompt: 10,
            completion: 5,
            cost: Some(0.05),
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
            account: Some(2),
            status: 500,
            prompt: 10,
            completion: 5,
            cost: Some(0.0),
            ttft: Some(100),
            total: 600,
            err: Some("upstream 500"),
            ..Default::default()
        },
    );

    let rows = by_account(&conn, &UsageFilter::default()).expect("by_account");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].account_id, AccountId::new(1));
    assert_eq!(rows[0].total_rows, 2);
    assert_eq!(rows[0].errors, 0);
    assert!((rows[0].total_cost_usd - 0.20).abs() < 1e-9);
    assert_eq!(rows[1].account_id, AccountId::new(2));
    assert_eq!(rows[1].total_rows, 2);
    assert_eq!(rows[1].errors, 1);
    assert!((rows[1].total_cost_usd - 0.05).abs() < 1e-9);
}

#[test]
fn by_provider_groups_by_provider_id() {
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
            cost: Some(0.25),
            ttft: Some(200),
            total: 1200,
            ..Default::default()
        },
    );
    insert(
        &conn,
        TestUsageParams {
            request_id: "r2",
            trace_id: "t2",
            provider: "openrouter",
            model: "openai/gpt-4o-mini",
            account: Some(1),
            status: 200,
            prompt: 100,
            completion: 50,
            cost: Some(0.25),
            ttft: Some(200),
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
            prompt: 100,
            completion: 50,
            cost: Some(1.00),
            ttft: Some(200),
            total: 1500,
            ..Default::default()
        },
    );
    insert(
        &conn,
        TestUsageParams {
            request_id: "r4",
            trace_id: "t4",
            provider: "openai",
            model: "gpt-4o",
            account: Some(3),
            status: 200,
            prompt: 100,
            completion: 50,
            cost: Some(0.10),
            ttft: Some(200),
            total: 800,
            race_lost: true,
            ..Default::default()
        },
    );

    let rows = by_provider(&conn, &UsageFilter::default()).expect("by_provider");
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].provider_id, "anthropic");
    assert!((rows[0].total_cost_usd - 1.00).abs() < 1e-9);
    assert_eq!(rows[1].provider_id, "openrouter");
    assert!((rows[1].total_cost_usd - 0.50).abs() < 1e-9);
    assert_eq!(rows[2].provider_id, "openai");
    assert!((rows[2].total_cost_usd - 0.10).abs() < 1e-9);
    assert_eq!(rows[2].winners, 0);

    let filtered = by_provider(
        &conn,
        &UsageFilter {
            provider_id: Some(ProviderId::new("openrouter")),
            ..Default::default()
        },
    )
    .expect("filtered");
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].provider_id, "openrouter");
}

#[test]
fn monthly_by_provider_groups_by_month() {
    let (conn, _p) = fresh_conn();
    let insert_at = |provider: &str, cost: f64, prompt: i64, created_at: &str| {
        conn.execute(
            "INSERT INTO usage (request_id, trace_id, attempt, provider_id, account_id, upstream_model_id, prompt_tokens, completion_tokens, cost_usd, connect_ms, ttft_ms, total_ms, status_code, created_at) VALUES ('r', 't', 1, ?1, 1, 'm', ?2, 0, ?3, 50, 200, 1200, 200, ?4)",
            params![provider, prompt, cost, created_at],
        ).unwrap();
    };

    insert_at("openrouter", 1.00, 100, "2026-06-01T12:00:00Z");
    insert_at("openrouter", 0.50, 100, "2026-06-15T12:00:00Z");
    insert_at("anthropic", 0.25, 50, "2026-06-20T12:00:00Z");
    insert_at("openrouter", 0.10, 10, "2026-07-01T12:00:00Z");
    insert_at("openai", 0.75, 20, "2026-07-31T12:00:00Z");

    let rows = monthly_by_provider(&conn, &UsageFilter::default()).expect("monthly_by_provider");
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0].month, "2026-06");
    assert_eq!(rows[0].provider_id, "openrouter");
    assert!((rows[0].total_cost_usd - 1.50).abs() < 1e-9);
    assert_eq!(rows[1].month, "2026-06");
    assert_eq!(rows[1].provider_id, "anthropic");
    assert_eq!(rows[2].month, "2026-07");
    assert_eq!(rows[2].provider_id, "openai");
    assert_eq!(rows[3].month, "2026-07");
    assert_eq!(rows[3].provider_id, "openrouter");

    let f = UsageFilter {
        from: Some("2026-06-01T00:00:00Z".to_string()),
        to: Some("2026-07-01T00:00:00Z".to_string()),
        ..Default::default()
    };
    let june_only = monthly_by_provider(&conn, &f).expect("june");
    assert_eq!(june_only.len(), 2);
}
