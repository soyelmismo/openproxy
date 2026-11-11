//! Unit tests of the predictor's statistics engine: token buckets, the
//! `max(mean + 3σ, p99) + margin` formula, bucket widening, recency
//! weighting and the estimated-token population preference (corrections
//! 2, 5 and 6).

use super::stats::tokens_bucket;
use super::*;
use crate::latency_budget::testutil::*;

#[test]
fn tokens_bucket_log_scale() {
    assert_eq!(tokens_bucket(1), 0);
    assert_eq!(tokens_bucket(999), 0);
    assert_eq!(tokens_bucket(1_000), 1);
    assert_eq!(tokens_bucket(1_500), 1);
    assert_eq!(tokens_bucket(3_999), 1);
    assert_eq!(tokens_bucket(4_000), 2);
    assert_eq!(tokens_bucket(8_000), 2);
    assert_eq!(tokens_bucket(15_999), 2);
    assert_eq!(tokens_bucket(16_000), 3);
    assert_eq!(tokens_bucket(30_000), 3);
    assert_eq!(tokens_bucket(63_999), 3);
    assert_eq!(tokens_bucket(64_000), 4);
    assert_eq!(tokens_bucket(100_000), 4);
    assert_eq!(tokens_bucket(255_999), 4);
    assert_eq!(tokens_bucket(256_000), 5);
    assert_eq!(tokens_bucket(500_000), 5);
    assert_eq!(tokens_bucket(0), 0);
}

#[test]
fn steady_fast_target_is_cut_quickly() {
    // 30 identical 2s successes. mean=2s, sigma=0 -> 2+0+1=3s, floored
    // to 5s: a hung attempt of this target dies at 5s, not at 60s.
    let mconn = shared_conn(test_conn());
    {
        let c = mconn.lock();
        for _ in 0..30 {
            insert(&c, "fast", 7, 2_000, 5_000);
        }
        // Failures never produced bytes: they must not inflate anything.
        c.execute(
            "INSERT INTO usage (provider_id, model_row_id, upstream_model_id, status_code, ttft_ms, prompt_tokens, created_at) \
             VALUES ('fast', 7, 'm', 500, NULL, 5_000, datetime('now'))",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO usage (provider_id, model_row_id, upstream_model_id, status_code, ttft_ms, prompt_tokens, created_at) \
             VALUES ('fast', 7, 'm', 504, 59_000, 5_000, datetime('now'))",
            [],
        )
        .unwrap();
    }
    let budget = Arc::new(LatencyBudget::new());
    let b = budget_of(&budget, &mconn, "fast", 7, 5_000, 60_000).expect("budget from 30 samples");
    assert_eq!(b, 5_000, "3+0+1=3s under the floor: {b}");
}

#[test]
fn jittery_target_earns_its_spread() {
    // Same mean, different spread: the steady one gets a tight budget,
    // the jittery one (slow-but-successful outliers) keeps headroom.
    let mconn = shared_conn(test_conn());
    {
        let c = mconn.lock();
        for _ in 0..20 {
            insert(&c, "steady", 1, 2_000, 5_000);
        }
        for ttft in [500i64, 1_000, 1_500, 2_000, 2_500, 3_000, 4_000, 5_500] {
            insert(&c, "jittery", 2, ttft, 5_000);
            insert(&c, "jittery", 2, ttft, 5_000);
        }
    }
    let budget = Arc::new(LatencyBudget::new());
    let steady = budget_of(&budget, &mconn, "steady", 1, 5_000, 60_000).unwrap();
    let jittery = budget_of(&budget, &mconn, "jittery", 2, 5_000, 60_000).unwrap();
    assert_eq!(steady, 5_000, "zero sigma -> floor");
    // jittery: mean 2500, sigma ~1591 -> 2500 + 4774 + 1000 = 8274.
    assert!(
        (8_000..=9_000).contains(&jittery),
        "learned spread earns slack: {jittery}"
    );
    assert!(
        jittery > steady + 3_000,
        "the jittery target must earn clearly more headroom: {jittery} vs {steady}"
    );
}

#[test]
fn cached_and_cold_mixed_population_prices_the_cold_tail() {
    // 80% cached-fast (2s), 20% cold-slow (15s): the budget lands in the
    // cold tail, so a cold outlier is not cut and a cached request is
    // never endangered.
    let mconn = shared_conn(test_conn());
    {
        let c = mconn.lock();
        for _ in 0..40 {
            insert(&c, "mix", 5, 2_000, 100_000);
        }
        for _ in 0..10 {
            insert(&c, "mix", 5, 15_000, 100_000);
        }
    }
    let budget = Arc::new(LatencyBudget::new());
    let b = budget_of(&budget, &mconn, "mix", 5, 100_000, 60_000).expect("budget");
    // mean 4.6s, sigma 5.25s -> 4.6 + 15.75 + 1 = ~21.4s.
    assert!((20_000..=23_000).contains(&b), "cold tail protected: {b}");
}

/// Correction 2: a warm-dominated target with a rare but legitimate cold
/// mode (5% at 30s) must not have that mode cut: p99 >= cold mode.
#[test]
fn p99_covers_bimodal_cold_tail() {
    let mconn = shared_conn(test_conn());
    {
        let c = mconn.lock();
        for _ in 0..190 {
            insert(&c, "bimodal", 6, 2_000, 100_000);
        }
        for _ in 0..10 {
            insert(&c, "bimodal", 6, 30_000, 100_000);
        }
    }
    let budget = Arc::new(LatencyBudget::new());
    let b = budget_of(&budget, &mconn, "bimodal", 6, 100_000, 60_000).expect("budget");
    // mean+3sigma ~ 3.4k + 3*6.0k = 21.5k, but p99 = 30k -> 31k.
    assert!(b >= 30_000, "cold mode protected by p99: {b}");
    assert!(b <= 32_000, "and it is the p99, not runaway: {b}");
}

#[test]
fn budget_respects_ceiling_and_widens_bucket_when_thin() {
    let mconn = shared_conn(test_conn());
    {
        let c = mconn.lock();
        // Bucket 3: slow cold (20-22s) plus fast cached (10-12.5s); the
        // 5k estimate (bucket 2) is thin -> widen UP to bucket 3.
        for ttft in [20_000i64, 21_000, 22_000] {
            insert(&c, "w", 9, ttft, 30_000);
        }
        for ttft in [10_000i64, 11_000, 12_000, 10_500, 11_500, 12_500] {
            insert(&c, "w", 9, ttft, 40_000);
        }
    }
    let budget = Arc::new(LatencyBudget::new());
    let b = budget_of(&budget, &mconn, "w", 9, 5_000, 60_000).expect("widened bucket has samples");
    // mean 13444, sigma 5081 -> 13444 + 15243 + 1000 = 29687 (p99 22k
    // loses to the spread bound).
    assert!((29_000..=30_500).contains(&b), "widened budget: {b}");

    // Ceiling: the resolved timeout always wins when tighter - and when
    // the learned budget would only reach it, the predictor stays silent
    // (nothing to shorten).
    assert!(
        budget_of(&budget, &mconn, "w", 9, 5_000, 8_000).is_none(),
        "learned budget >= ceiling -> no prediction"
    );
}

#[test]
fn no_data_returns_none() {
    let mconn = shared_conn(test_conn());
    let budget = Arc::new(LatencyBudget::new());
    assert!(budget_of(&budget, &mconn, "empty", 1, 5_000, 60_000).is_none());
}

#[test]
fn missing_usage_table_does_not_panic() {
    // Boot path: a connection whose schema has no usage table yet must
    // yield None, never a panic.
    let mconn = shared_conn(Connection::open_in_memory().unwrap());
    let budget = Arc::new(LatencyBudget::new());
    assert!(budget_of(&budget, &mconn, "x", 1, 5_000, 60_000).is_none());
}

#[test]
fn window_excludes_rows_older_than_seven_days() {
    let mconn = shared_conn(test_conn());
    {
        let c = mconn.lock();
        for _ in 0..10 {
            insert_at(&c, "old", 1, 40_000, 5_000, "datetime('now', '-8 days')");
        }
        for _ in 0..8 {
            insert_at(&c, "old", 1, 2_000, 5_000, "datetime('now', '-6 days')");
        }
    }
    let budget = Arc::new(LatencyBudget::new());
    let b = budget_of(&budget, &mconn, "old", 1, 5_000, 60_000).expect("in-window samples exist");
    assert_eq!(
        b, 5_000,
        "only the recent fast samples feed the budget: {b}"
    );
}

#[test]
fn target_isolation_by_provider_and_model_row() {
    let mconn = shared_conn(test_conn());
    {
        let c = mconn.lock();
        for _ in 0..10 {
            insert(&c, "a", 1, 2_000, 5_000);
        }
    }
    let budget = Arc::new(LatencyBudget::new());
    assert!(budget_of(&budget, &mconn, "a", 2, 5_000, 60_000).is_none());
    assert!(budget_of(&budget, &mconn, "b", 1, 5_000, 60_000).is_none());
}

#[test]
fn null_and_zero_ttft_and_null_prompt_are_ignored() {
    let mconn = shared_conn(test_conn());
    {
        let c = mconn.lock();
        for sql in [
            "INSERT INTO usage (provider_id, model_row_id, upstream_model_id, status_code, ttft_ms, prompt_tokens, created_at) VALUES ('n', 3, 'm', 200, NULL, 5000, datetime('now'))",
            "INSERT INTO usage (provider_id, model_row_id, upstream_model_id, status_code, ttft_ms, prompt_tokens, created_at) VALUES ('n', 3, 'm', 200, 0, 5000, datetime('now'))",
            "INSERT INTO usage (provider_id, model_row_id, upstream_model_id, status_code, ttft_ms, prompt_tokens, created_at) VALUES ('n', 3, 'm', 200, -5, 5000, datetime('now'))",
        ] {
            c.execute(sql, []).unwrap();
        }
        for ttft in [500i64; 8] {
            insert(&c, "n", 3, ttft, 800);
        }
    }
    let budget = Arc::new(LatencyBudget::new());
    let b = budget_of(&budget, &mconn, "n", 3, 500, 60_000).expect("valid bucket-0 samples");
    assert_eq!(b, 5_000, "300+0+1=1.3s under the floor: {b}");
}

#[test]
fn sample_limit_keeps_most_recent_only() {
    let mconn = shared_conn(test_conn());
    {
        let c = mconn.lock();
        for _ in 0..50 {
            insert_at(&c, "lim", 4, 50_000, 5_000, "datetime('now', '-1 day')");
        }
        for _ in 0..400 {
            insert(&c, "lim", 4, 2_000, 5_000);
        }
    }
    let budget = Arc::new(LatencyBudget::new());
    let b = budget_of(&budget, &mconn, "lim", 4, 5_000, 60_000).expect("budget");
    assert_eq!(b, 5_000, "old slow rows fell out of the sample limit: {b}");
}
