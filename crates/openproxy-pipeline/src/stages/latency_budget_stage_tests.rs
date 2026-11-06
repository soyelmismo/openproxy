//! Integration tests for `apply_predictive_ttft_budget` (the pure function
//! behind `TimeoutResolutionStage`). All on an in-memory DB - production is
//! never touched.

use crate::pipeline::{Pipeline, PipelineRequest};
use crate::stages::target::apply_predictive_ttft_budget;
use crate::test_utils;
use openproxy_types::ids::{ComboId, ModelRowId, ProviderId};
use std::sync::Arc;
use std::time::Duration;

fn stage_with_defaults() -> crate::timeouts::Timeouts {
    crate::timeouts::Timeouts {
        connect: Duration::from_millis(5_000),
        request_send: Duration::from_millis(10_000),
        ttft: Duration::from_secs(60),
        idle_chunk: Duration::from_secs(30),
        total: Duration::from_secs(120),
    }
}

fn make_model(provider: &str, row_id: i64) -> openproxy_types::models::Model {
    openproxy_types::models::Model {
        row_id: ModelRowId::new(row_id),
        provider_id: ProviderId::new(provider),
        ..Default::default()
    }
}

fn make_target(provider: &str, row_id: i64) -> openproxy_types::combos::ComboTarget {
    openproxy_types::combos::ComboTarget {
        id: openproxy_types::ids::ComboTargetId::new(1),
        combo_id: ComboId::new(1),
        provider_id: ProviderId::new(provider),
        model_row_id: Some(ModelRowId::new(row_id)),
        ..Default::default()
    }
}

fn seed_usage(conn: &rusqlite::Connection, provider: &str, model_row: i64, ttfts: &[i64]) {
    for ttft in ttfts {
        conn.execute(
            "INSERT INTO usage (request_id, trace_id, attempt, provider_id, upstream_model_id, \
                 status_code, ttft_ms, prompt_tokens, created_at) \
             VALUES ('r', 't', 1, ?1, 'm', 200, ?2, 2000, datetime('now'))",
            rusqlite::params![provider, ttft],
        )
        .unwrap();
    }
    conn.execute(
        &format!(
            "UPDATE usage SET model_row_id = {model_row} WHERE provider_id = '{provider}' AND model_row_id IS NULL"
        ),
        [],
    )
    .unwrap();
}

fn setup(pool_tag: &str, provider: &str, row_id: i64, ttfts: &[i64]) -> (Pipeline, PipelineRequest) {
    let pool = openproxy_db::DbPool::test_pool_with_prefix(&format!("openproxy-{pool_tag}"))
        .expect("pool");
    let conn = Arc::new(parking_lot::Mutex::new(
        pool.open_connection().expect("conn"),
    ));
    if !ttfts.is_empty() {
        let c = conn.lock();
        seed_usage(&c, provider, row_id, ttfts);
    }
    let master_key = Arc::new(openproxy_db::secrets::MasterKey::generate().expect("master key"));
    let mut config = test_utils::test_config(master_key);
    config.defaults = stage_with_defaults();
    let pipeline = test_utils::test_pipeline_with_pool(pool, config);
    let (req, _tx) = test_utils::make_request(ComboId(1));
    (pipeline, req)
}

#[test]
fn applies_learned_budget_when_shorter() {
    let (pipeline, req) = setup("lb-apply", "fastprov", 42, &[2_000i64; 20]);
    let out = apply_predictive_ttft_budget(
        &pipeline,
        &req,
        "fastprov",
        &make_target("fastprov", 42),
        &make_model("fastprov", 42),
        stage_with_defaults(),
    );
    // mean 2s, sigma 0 -> 3s, floored to 5s: 60s -> 5s.
    assert_eq!(out.ttft, Duration::from_secs(5), "learned budget applied");
}

#[test]
fn keeps_resolved_timeout_when_no_data() {
    let (pipeline, req) = setup("lb-nodata", "unknownprov", 7, &[]);
    let out = apply_predictive_ttft_budget(
        &pipeline,
        &req,
        "unknownprov",
        &make_target("unknownprov", 7),
        &make_model("unknownprov", 7),
        stage_with_defaults(),
    );
    assert_eq!(out.ttft, Duration::from_secs(60), "no data -> untouched");
}

#[test]
fn never_extends_below_resolved_timeout() {
    let (pipeline, req) = setup("lb-slow", "slowprov", 9, &[40_000i64; 20]);
    let mut resolved = stage_with_defaults();
    resolved.ttft = Duration::from_secs(8); // model override ceiling
    let out = apply_predictive_ttft_budget(
        &pipeline,
        &req,
        "slowprov",
        &make_target("slowprov", 9),
        &make_model("slowprov", 9),
        resolved,
    );
    assert_eq!(out.ttft, Duration::from_secs(8), "override is the ceiling");
}

#[test]
fn antigravity_is_never_predicted() {
    let (pipeline, req) = setup("lb-agy", "antigravity", 5, &[2_000i64; 20]);
    let mut resolved = crate::timeouts::apply_antigravity_floor(stage_with_defaults());
    let out = apply_predictive_ttft_budget(
        &pipeline,
        &req,
        "antigravity",
        &make_target("antigravity", 5),
        &make_model("antigravity", 5),
        resolved,
    );
    resolved.ttft = out.ttft;
    assert_eq!(
        out.ttft,
        Duration::from_millis(crate::timeouts::ANTIGRAVITY_MIN_TIMEOUT_MS),
        "antigravity floor untouched"
    );
}

#[test]
fn jittery_target_gets_more_headroom_than_steady() {
    let (pipeline, req) = setup("lb-jit", "jit", 3, &[500i64, 1_000, 1_500, 2_000, 2_500, 3_000, 4_000, 5_500]);
    let out = apply_predictive_ttft_budget(
        &pipeline,
        &req,
        "jit",
        &make_target("jit", 3),
        &make_model("jit", 3),
        stage_with_defaults(),
    );
    // mean 2500, sigma ~1591 -> 2500 + 4774 + 1000 = 8274ms.
    assert!(out.ttft >= Duration::from_millis(8_000) && out.ttft <= Duration::from_millis(9_000));
}
