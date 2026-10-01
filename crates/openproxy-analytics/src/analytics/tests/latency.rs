use super::super::*;
use super::{TestUsageParams, fresh_conn, insert};
use crate::usage::UsageFilter;

#[test]
fn latency_percentiles_uniform_distribution() {
    let (conn, _p) = fresh_conn();

    // 100 rows: connect_ms in 0..100, with ttft/total/tps absent.
    for v in 0..100i64 {
        insert(
            &conn,
            TestUsageParams {
                request_id: &format!("req-{v}"),
                trace_id: &format!("trace-{v}"),
                provider: "openrouter",
                model: "openai/gpt-4o",
                connect_ms: Some(v),
                ..Default::default()
            },
        );
    }

    let r = latency_percentiles(&conn, &UsageFilter::default()).expect("latency");
    assert_eq!(r.samples, 100, "100 winner rows scanned");
    // 0..100 is 0..=99 — p50 of 0..=99 is ~49.5, p95 is ~94.05. We give a
    // generous ±5 ms tolerance to absorb t-digest approximation error
    // (200 centroids over 100 samples is overkill but spec-prescribed).
    let p50 = r.p50_connect_ms.expect("p50 connect present");
    let p95 = r.p95_connect_ms.expect("p95 connect present");
    assert!((p50 - 49.5).abs() < 5.0, "p50 ≈ 49.5, got {p50}");
    assert!((p95 - 94.05).abs() < 5.0, "p95 ≈ 94.05, got {p95}");
    assert_eq!(r.p50_ttft_ms, None);
    assert_eq!(r.p95_ttft_ms, None);
    assert_eq!(r.p50_tokens_per_sec, None);
    assert_eq!(r.p95_tokens_per_sec, None);
    assert_eq!(r.p50_total_ms, Some(0.0));
    assert_eq!(r.p95_total_ms, Some(0.0));
}

#[test]
fn latency_percentiles_skips_losers() {
    let (conn, _p) = fresh_conn();

    // 10 winners with connect_ms = 1000..1010.
    for i in 0..10i64 {
        insert(
            &conn,
            TestUsageParams {
                request_id: &format!("w-{i}"),
                trace_id: &format!("wt-{i}"),
                provider: "openrouter",
                model: "openai/gpt-4o",
                connect_ms: Some(1000 + i),
                ..Default::default()
            },
        );
    }
    // 20 losers with connect_ms = 0..20 — they must not influence p50.
    for i in 0..20i64 {
        insert(
            &conn,
            TestUsageParams {
                request_id: &format!("l-{i}"),
                trace_id: &format!("lt-{i}"),
                provider: "openrouter",
                model: "openai/gpt-4o",
                connect_ms: Some(i),
                race_lost: true,
                ..Default::default()
            },
        );
    }

    let r = latency_percentiles(&conn, &UsageFilter::default()).expect("latency");
    assert_eq!(
        r.samples, 10,
        "only winners counted in `samples` (race_lost=0 filter)"
    );
    let p50 = r.p50_connect_ms.expect("p50 connect present");
    let p95 = r.p95_connect_ms.expect("p95 connect present");
    assert!(
        (p50 - 1004.5).abs() < 5.0,
        "p50 should reflect winners (~1004.5), got {p50}"
    );
    assert!(
        (p95 - 1009.05).abs() < 5.0,
        "p95 should reflect winners (~1009.05), got {p95}"
    );
}

#[test]
fn latency_percentiles_handles_nulls() {
    let (conn, _p) = fresh_conn();

    for i in 0..5i64 {
        insert(
            &conn,
            TestUsageParams {
                request_id: &format!("n-{i}"),
                trace_id: &format!("nt-{i}"),
                provider: "openrouter",
                model: "openai/gpt-4o",
                connect_ms: Some(50),
                total_ms: 1000,
                ..Default::default()
            },
        );
    }
    for i in 0..5i64 {
        insert(
            &conn,
            TestUsageParams {
                request_id: &format!("v-{i}"),
                trace_id: &format!("vt-{i}"),
                provider: "openrouter",
                model: "openai/gpt-4o",
                connect_ms: Some(50),
                ttft_ms: Some(200),
                total_ms: 1000,
                tokens_per_sec: Some(10.0),
                ..Default::default()
            },
        );
    }

    let r = latency_percentiles(&conn, &UsageFilter::default()).expect("latency");
    assert_eq!(r.samples, 10, "all 10 winner rows scanned");
    assert!(r.p50_connect_ms.is_some());
    assert!(r.p95_total_ms.is_some());
    let p50_ttft = r.p50_ttft_ms.expect("p50 ttft present");
    let p95_ttft = r.p95_ttft_ms.expect("p95 ttft present");
    assert!(
        (p50_ttft - 200.0).abs() < 1.0,
        "p50 ttft ≈ 200, got {p50_ttft}"
    );
    assert!(
        (p95_ttft - 200.0).abs() < 1.0,
        "p95 ttft ≈ 200, got {p95_ttft}"
    );
    let p50_tps = r.p50_tokens_per_sec.expect("p50 tps present");
    assert!((p50_tps - 10.0).abs() < 0.5, "p50 tps ≈ 10, got {p50_tps}");
}

#[test]
fn latency_percentiles_excludes_errors() {
    let (conn, _p) = fresh_conn();

    for i in 0..10i64 {
        insert(
            &conn,
            TestUsageParams {
                request_id: &format!("ok-{i}"),
                trace_id: &format!("t-{i}"),
                provider: "openrouter",
                model: "openai/gpt-4o",
                connect_ms: Some(100 + i),
                ttft_ms: Some(200 + i),
                total_ms: 500 + i,
                status_code: Some(200),
                ..Default::default()
            },
        );
    }
    for i in 0..5i64 {
        insert(
            &conn,
            TestUsageParams {
                request_id: &format!("err-{i}"),
                trace_id: &format!("et-{i}"),
                provider: "openrouter",
                model: "openai/gpt-4o",
                connect_ms: Some(10000),
                total_ms: 10000,
                status_code: Some(502),
                ..Default::default()
            },
        );
    }
    for i in 0..3i64 {
        insert(
            &conn,
            TestUsageParams {
                request_id: &format!("disc-{i}"),
                trace_id: &format!("dt-{i}"),
                provider: "openrouter",
                model: "openai/gpt-4o",
                connect_ms: Some(5000),
                total_ms: 5000,
                status_code: Some(499),
                ..Default::default()
            },
        );
    }

    let r = latency_percentiles(&conn, &UsageFilter::default()).expect("latency");
    assert_eq!(r.samples, 10, "error rows (502, 499) excluded from count");

    let p50 = r.p50_connect_ms.expect("p50 connect present");
    let p95 = r.p95_connect_ms.expect("p95 connect present");
    assert!(
        (p50 - 104.5).abs() < 5.0,
        "p50 should reflect only successes (~104.5), got {p50}"
    );
    assert!(
        (p95 - 109.05).abs() < 5.0,
        "p95 should reflect only successes (~109.05), got {p95}"
    );
}
