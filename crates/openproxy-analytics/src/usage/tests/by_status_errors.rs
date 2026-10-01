use super::*;
use crate::ids::*;
use crate::usage::*;

#[test]
fn by_status_groups_by_code() {
    let (conn, _p) = fresh_conn();
    for _ in 0..3 {
        let req = RequestId::new().to_string();
        let trace = TraceId::new().to_string();
        insert(
            &conn,
            TestUsageParams {
                request_id: &req,
                trace_id: &trace,
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
    }
    for _ in 0..2 {
        let req = RequestId::new().to_string();
        let trace = TraceId::new().to_string();
        insert(
            &conn,
            TestUsageParams {
                request_id: &req,
                trace_id: &trace,
                provider: "openrouter",
                model: "openai/gpt-4o",
                account: Some(1),
                status: 429,
                prompt: 10,
                completion: 5,
                cost: Some(0.0),
                ttft: Some(100),
                total: 600,
                err: Some("rate limited"),
                ..Default::default()
            },
        );
    }
    let req = RequestId::new().to_string();
    let trace = TraceId::new().to_string();
    insert(
        &conn,
        TestUsageParams {
            request_id: &req,
            trace_id: &trace,
            provider: "openrouter",
            model: "openai/gpt-4o",
            account: Some(1),
            status: 500,
            prompt: 10,
            completion: 5,
            cost: Some(0.0),
            ttft: Some(100),
            total: 600,
            err: Some("oops"),
            ..Default::default()
        },
    );

    let rows = by_status(&conn, &UsageFilter::default()).expect("by_status");
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].status_code, 200);
    assert_eq!(rows[0].count, 3);
    assert_eq!(rows[1].status_code, 429);
    assert_eq!(rows[1].count, 2);
    assert_eq!(rows[2].status_code, 500);
    assert_eq!(rows[2].count, 1);
}

#[test]
fn errors_returns_only_4xx_5xx() {
    let (conn, _p) = fresh_conn();
    let r1 = RequestId::new().to_string();
    let t1 = TraceId::new().to_string();
    let r2 = RequestId::new().to_string();
    let t2 = TraceId::new().to_string();
    let r3 = RequestId::new().to_string();
    let t3 = TraceId::new().to_string();
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
            account: Some(1),
            status: 400,
            prompt: 10,
            completion: 5,
            cost: Some(0.0),
            ttft: Some(100),
            total: 600,
            err: Some("bad request"),
            ..Default::default()
        },
    );
    insert(
        &conn,
        TestUsageParams {
            request_id: &r3,
            trace_id: &t3,
            provider: "openrouter",
            model: "openai/gpt-4o",
            account: Some(1),
            status: 502,
            prompt: 10,
            completion: 5,
            cost: Some(0.0),
            ttft: Some(100),
            total: 600,
            err: Some("upstream down"),
            ..Default::default()
        },
    );

    let rows = errors(&conn, &UsageFilter::default(), 50).expect("errors");
    assert_eq!(rows.len(), 2);
    let codes: Vec<u16> = rows.iter().map(|r| r.status_code).collect();
    assert!(codes.contains(&400));
    assert!(codes.contains(&502));
    for r in &rows {
        assert!(r.status_code >= 400);
        assert!(r.error_msg_redacted.is_some());
    }
}

#[test]
fn errors_respects_limit() {
    let (conn, _p) = fresh_conn();
    for i in 0..5 {
        let req = format!("req-{i}");
        let trace = format!("trace-{i}");
        insert(
            &conn,
            TestUsageParams {
                request_id: &req,
                trace_id: &trace,
                provider: "openrouter",
                model: "openai/gpt-4o",
                account: Some(1),
                status: 500,
                prompt: 10,
                completion: 5,
                cost: Some(0.0),
                ttft: Some(100),
                total: 600,
                err: Some("err"),
                ..Default::default()
            },
        );
    }
    let rows = errors(&conn, &UsageFilter::default(), 2).expect("errors");
    assert_eq!(rows.len(), 2, "limit caps the result set");
}
