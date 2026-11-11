//! Unit tests of the learned idle-chunk budgets (`chunk_budget`): pooled
//! gap distribution, sample gate, ceiling clamp and the anti-starvation
//! relax driven by recorded `idle_chunk` cuts.

use super::stats::MIN_BUCKET_SAMPLES;
use super::*;
use crate::latency_budget::testutil::*;
use openproxy_types::ids::ModelRowId;

// Learned idle-chunk budgets (chunk_budget).
// ----------

/// Inserts a completed streaming row with its largest survived gap.
fn insert_gap(
    c: &Connection,
    provider: &str,
    model_row: i64,
    gap_ms: i64,
    session_hash: Option<i64>,
) {
    c.execute(
        "INSERT INTO usage (provider_id, model_row_id, upstream_model_id, status_code, ttft_ms, prompt_tokens, session_hash, max_chunk_gap_ms, avg_chunk_gap_ms, created_at) \
         VALUES (?1, ?2, 'm', 200, 900, 5000, ?3, ?4, ?4/2, datetime('now'))",
        rusqlite::params![provider, model_row, session_hash, gap_ms],
    )
    .unwrap();
}

fn gap_budget_of(
    b: &Arc<LatencyBudget>,
    c: &parking_lot::Mutex<Connection>,
    provider: &str,
    row: i64,
    session_hash: Option<u64>,
    ceiling: u64,
) -> Option<u64> {
    b.idle_chunk_budget_ms(provider, ModelRowId(row), session_hash, ceiling, Some(c))
}

#[test]
fn gap_budget_learned_from_survived_gaps() {
    let mconn = shared_conn(test_conn());
    {
        let c = mconn.lock();
        for _ in 0..12 {
            insert_gap(&c, "gap", 7, 4_000, None);
        }
    }
    let budget = Arc::new(LatencyBudget::new());
    // mean=4s, sigma≈0 -> 4+0+2=6s under the 10s gap floor.
    let b = gap_budget_of(&budget, &mconn, "gap", 7, None, 30_000).expect("gap budget");
    assert_eq!(b, 10_000, "floored to FLOOR_CHUNK_MS: {b}");
    // And it actually shortens the resolved timeout.
    let none_ceiling = gap_budget_of(&budget, &mconn, "gap", 7, None, 5_000);
    assert!(
        none_ceiling.is_none(),
        "ceiling 5s below the floor must keep the resolved timeout"
    );
}

#[test]
fn gap_budget_none_without_telemetry() {
    let mconn = shared_conn(test_conn());
    {
        let c = mconn.lock();
        // TTFT successes only: no gap telemetry, no prediction.
        for _ in 0..20 {
            insert(&c, "nogap", 8, 2_000, 5_000);
        }
    }
    let budget = Arc::new(LatencyBudget::new());
    assert!(
        gap_budget_of(&budget, &mconn, "nogap", 8, None, 30_000).is_none(),
        "no gap rows -> resolved timeout untouched"
    );
}

#[test]
fn gap_budget_none_below_sample_gate() {
    let mconn = shared_conn(test_conn());
    {
        let c = mconn.lock();
        for _ in 0..(MIN_BUCKET_SAMPLES - 1) {
            insert_gap(&c, "thin", 9, 4_000, None);
        }
    }
    let budget = Arc::new(LatencyBudget::new());
    assert!(
        gap_budget_of(&budget, &mconn, "thin", 9, None, 30_000).is_none(),
        "5 samples < MIN_BUCKET_SAMPLES: no prediction"
    );
}

#[test]
fn idle_chunk_cut_relaxes_the_gap_budget() {
    let mconn = shared_conn(test_conn());
    {
        let c = mconn.lock();
        for _ in 0..12 {
            insert_gap(&c, "cutgap", 10, 4_000, None);
        }
        // A recorded idle_chunk cut at 25s: the next budget must be at
        // least 25 * 1.5 = 37.5s (anti-starvation).
        insert_cut_named(&c, "cutgap", 10, 25_000, "idle_chunk");
    }
    let budget = Arc::new(LatencyBudget::new());
    let b = gap_budget_of(&budget, &mconn, "cutgap", 10, None, 60_000).expect("relaxed");
    assert_eq!(b, 37_500, "cut relaxes the gap budget: {b}");
}

#[test]
fn relax_reaching_ceiling_turns_gap_predictor_off() {
    let mconn = shared_conn(test_conn());
    {
        let c = mconn.lock();
        for _ in 0..12 {
            insert_gap(&c, "offgap", 11, 4_000, None);
        }
        insert_cut_named(&c, "offgap", 11, 25_000, "idle_chunk");
    }
    let budget = Arc::new(LatencyBudget::new());
    // 25s cut * 1.5 = 37.5s >= 30s ceiling: the predictor reverts to the
    // resolved timeout instead of predicting a budget it cannot honor.
    assert!(
        gap_budget_of(&budget, &mconn, "offgap", 11, None, 30_000).is_none(),
        "relax past the ceiling disables the gap predictor for this target"
    );
}

#[test]
fn ttft_cuts_do_not_touch_the_gap_budget() {
    let mconn = shared_conn(test_conn());
    {
        let c = mconn.lock();
        for _ in 0..12 {
            insert_gap(&c, "isolated", 12, 4_000, None);
        }
        // A headers/ttft cut must relax the TTFT budget only.
        insert_cut_named(&c, "isolated", 12, 25_000, "ttft");
    }
    let budget = Arc::new(LatencyBudget::new());
    let b = gap_budget_of(&budget, &mconn, "isolated", 12, None, 60_000).expect("gap budget");
    assert_eq!(b, 10_000, "ttft cut must not inflate the gap budget: {b}");
    let t = budget_of(&budget, &mconn, "isolated", 12, 5_000, 60_000).expect("ttft budget");
    assert_eq!(t, 37_500, "ttft cut relaxes the ttft budget: {t}");
}

#[test]
fn zero_ceiling_keeps_the_resolved_timeout() {
    let mconn = shared_conn(test_conn());
    let budget = Arc::new(LatencyBudget::new());
    assert!(
        gap_budget_of(&budget, &mconn, "anything", 13, None, 0).is_none(),
        "a zero ceiling is not a prediction"
    );
}
