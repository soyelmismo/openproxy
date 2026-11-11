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
                 status_code, ttft_ms, prompt_tokens, prompt_tokens_estimated, created_at) \
             VALUES ('r', 't', 1, ?1, 'm', 200, ?2, 2000, 1, datetime('now'))",
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

/// Builds the pipeline with its own test DB. The rows are seeded directly
/// through the pipeline's predictor (`warm_target`-style) so the sync test
/// path is deterministic: the pipeline runs in pool mode, whose hot path
/// refreshes in the background.
fn setup(
    pool_tag: &str,
    provider: &str,
    row_id: i64,
    ttfts: &[i64],
) -> (Pipeline, PipelineRequest) {
    let pool = openproxy_db::DbPool::test_pool_with_prefix(&format!("openproxy-{pool_tag}"))
        .expect("pool");
    {
        let conn = pool.open_connection().expect("conn");
        if !ttfts.is_empty() {
            seed_usage(&conn, provider, row_id, ttfts);
        }
    }
    let master_key = Arc::new(openproxy_db::secrets::MasterKey::generate().expect("master key"));
    let mut config = test_utils::test_config(master_key);
    config.defaults = stage_with_defaults();
    let pipeline = test_utils::test_pipeline_with_pool(pool, config);
    // Warm the predictor's snapshot from the seeded rows (test helper; the
    // production hot path uses the background refresh instead).
    if !ttfts.is_empty() {
        let conn = pipeline
            .db_pool
            .as_ref()
            .expect("pipeline carries its pool")
            .open_connection()
            .expect("conn");
        pipeline.latency_budget.warm_target(&conn, provider, row_id);
    }
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

/// Correction 7: antigravity is no longer excluded. Its resolved timeout
/// already carries the 120s thinking floor, which acts as the natural
/// ceiling - a fast non-thinking antigravity target gets its learned budget
/// applied inside it.
#[test]
fn antigravity_fast_targets_get_predicted_inside_the_floor() {
    let (pipeline, req) = setup("lb-agy", "antigravity", 5, &[2_000i64; 20]);
    let resolved = crate::timeouts::apply_antigravity_floor(stage_with_defaults());
    assert_eq!(
        resolved.ttft,
        Duration::from_millis(crate::timeouts::ANTIGRAVITY_MIN_TIMEOUT_MS),
        "floor applied before the predictor"
    );
    let out = apply_predictive_ttft_budget(
        &pipeline,
        &req,
        "antigravity",
        &make_target("antigravity", 5),
        &make_model("antigravity", 5),
        resolved,
    );
    // mean 2s, sigma 0 -> 3s, floored to 5s: cut from 120s to 5s. A hung
    // non-thinking antigravity attempt no longer waits the whole floor.
    assert_eq!(
        out.ttft,
        Duration::from_secs(5),
        "fast antigravity target predicted inside its floor"
    );
}

/// A target whose learned budget would reach the ceiling is not predicted
/// (thinking antigravity): same behaviour as today.
#[test]
fn antigravity_thinking_budget_reaching_floor_is_not_predicted() {
    let (pipeline, req) = setup("lb-agy-think2", "antigravity", 7, &[125_000i64; 20]);
    let resolved = crate::timeouts::apply_antigravity_floor(stage_with_defaults());
    let out = apply_predictive_ttft_budget(
        &pipeline,
        &req,
        "antigravity",
        &make_target("antigravity", 7),
        &make_model("antigravity", 7),
        resolved,
    );
    // 125s norm + 1s margin > 120s ceiling -> the budget would extend the
    // timeout, so the predictor stays silent and the floor stands.
    assert_eq!(
        out.ttft,
        Duration::from_millis(crate::timeouts::ANTIGRAVITY_MIN_TIMEOUT_MS),
        "a 125s norm reaches the ceiling: predictor silent, floor intact"
    );
}

#[test]
fn jittery_target_gets_more_headroom_than_steady() {
    let (pipeline, req) = setup(
        "lb-jit",
        "jit",
        3,
        &[500i64, 1_000, 1_500, 2_000, 2_500, 3_000, 4_000, 5_500],
    );
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

/// An empty database (fresh boot) must not panic and must not predict.
#[test]
fn empty_database_never_panics() {
    let (pipeline, req) = setup("lb-empty", "ghost", 1, &[]);
    let out = apply_predictive_ttft_budget(
        &pipeline,
        &req,
        "ghost",
        &make_target("ghost", 1),
        &make_model("ghost", 1),
        stage_with_defaults(),
    );
    assert_eq!(out.ttft, Duration::from_secs(60));
}

// ----------
// Learned idle-chunk budgets (same stage, gap distribution).
// ----------

fn seed_gaps(pool: &openproxy_db::DbPool, provider: &str, model_row: i64, gaps: &[i64]) {
    let conn = pool.open_connection().expect("conn");
    for gap in gaps {
        conn.execute(
            "INSERT INTO usage (request_id, trace_id, attempt, provider_id, upstream_model_id, \
                 status_code, ttft_ms, model_row_id, max_chunk_gap_ms, avg_chunk_gap_ms, created_at) \
             VALUES ('r', 't', 1, ?1, 'm', 200, 900, ?2, ?3, ?3/2, datetime('now'))",
            rusqlite::params![provider, model_row, gap],
        )
        .unwrap();
    }
}

fn stage_with_gaps(
    tag: &str,
    provider: &str,
    row_id: i64,
    gaps: &[i64],
) -> (Pipeline, PipelineRequest) {
    let pool =
        openproxy_db::DbPool::test_pool_with_prefix(&format!("openproxy-{tag}")).expect("pool");
    seed_gaps(&pool, provider, row_id, gaps);
    let master_key = Arc::new(openproxy_db::secrets::MasterKey::generate().expect("master key"));
    let mut config = test_utils::test_config(master_key);
    config.defaults = stage_with_defaults();
    let pipeline = test_utils::test_pipeline_with_pool(pool, config);
    let conn = pipeline
        .db_pool
        .as_ref()
        .expect("pipeline carries its pool")
        .open_connection()
        .expect("conn");
    pipeline.latency_budget.warm_target(&conn, provider, row_id);
    let (req, _tx) = test_utils::make_request(ComboId(1));
    (pipeline, req)
}

/// Completed streams with steady 4s gaps teach the predictor a 10s gap
/// budget (4s + margin 2s = 6s, floored): a stream that goes silent mid
/// generation dies at 10s instead of 30s.
#[test]
fn applies_learned_idle_chunk_budget() {
    let (pipeline, req) = stage_with_gaps("lb-gap", "gapprov", 11, &[4_000i64; 12]);
    let out = apply_predictive_ttft_budget(
        &pipeline,
        &req,
        "gapprov",
        &make_target("gapprov", 11),
        &make_model("gapprov", 11),
        stage_with_defaults(),
    );
    assert_eq!(
        out.idle_chunk,
        Duration::from_secs(10),
        "learned gap budget applied (floored to 10s)"
    );
}

/// No gap telemetry: the resolved idle_chunk timeout is untouched.
#[test]
fn keeps_idle_chunk_timeout_without_gap_data() {
    let (pipeline, req) = stage_with_gaps("lb-nogap", "nogapprov", 12, &[]);
    let out = apply_predictive_ttft_budget(
        &pipeline,
        &req,
        "nogapprov",
        &make_target("nogapprov", 12),
        &make_model("nogapprov", 12),
        stage_with_defaults(),
    );
    assert_eq!(
        out.idle_chunk,
        Duration::from_secs(30),
        "no gap data -> resolved timeout untouched"
    );
}
