//! Unit tests of the latency predictor's cache and lifecycle: snapshot
//! TTLs, cache caps, pool-mode refresh, session hierarchy and production
//! schema compatibility. All on in-memory DBs; production is never touched.

use super::loaders::parse_headers_timeout_ms;
use super::*;
use crate::latency_budget::testutil::*;
use openproxy_types::ids::ModelRowId;

#[tokio::test]
async fn cache_prevents_requery_within_ttl() {
    let mconn = shared_conn(test_conn());
    {
        let c = mconn.lock();
        for _ in 0..10 {
            insert(&c, "c", 6, 2_000, 5_000);
        }
    }
    let budget = Arc::new(LatencyBudget::new());
    let first = budget_of(&budget, &mconn, "c", 6, 5_000, 60_000).unwrap();
    {
        // New data arrives: the snapshot is still fresh, the answer must
        // not change until the TTL expires.
        let c = mconn.lock();
        for _ in 0..20 {
            insert(&c, "c", 6, 40_000, 5_000);
        }
    }
    let second = budget_of(&budget, &mconn, "c", 6, 5_000, 60_000).unwrap();
    assert_eq!(first, second, "TTL cache holds: {first} vs {second}");
}

#[test]
fn independent_instances_do_not_share_cache() {
    let mconn = shared_conn(test_conn());
    {
        let c = mconn.lock();
        for _ in 0..10 {
            insert(&c, "c", 6, 2_000, 5_000);
        }
    }
    let b1 = Arc::new(LatencyBudget::new());
    let b2 = Arc::new(LatencyBudget::new());
    assert!(budget_of(&b1, &mconn, "c", 6, 5_000, 60_000).is_some());
    assert!(budget_of(&b2, &mconn, "c", 6, 5_000, 60_000).is_some());
}

#[test]
fn ceiling_below_floor_leaves_the_override_intact() {
    // A resolved timeout tighter than the floor (e.g. a model override of
    // 2s): the learned budget (>= floor 5s) can only extend it, so the
    // predictor stays silent and the override stands untouched.
    let mconn = shared_conn(test_conn());
    {
        let c = mconn.lock();
        for _ in 0..10 {
            insert(&c, "f", 8, 2_000, 5_000);
        }
    }
    let budget = Arc::new(LatencyBudget::new());
    assert!(
        budget_of(&budget, &mconn, "f", 8, 5_000, 2_000).is_none(),
        "override tighter than the floor: no prediction"
    );
    // Ceiling of 0: nothing to shorten -> no prediction.
    assert!(budget_of(&budget, &mconn, "f", 8, 5_000, 0).is_none());
}

#[test]
fn single_legitimate_outlier_is_respected_not_cut() {
    // 5 fast + 1 slow-but-successful (40s): with 6 samples the learned
    // sigma is huge, so the budget is conservative - a provider that
    // sometimes takes 40s and answers must not be cut.
    let mconn = shared_conn(test_conn());
    {
        let c = mconn.lock();
        for _ in 0..5 {
            insert(&c, "o", 2, 2_000, 5_000);
        }
        insert(&c, "o", 2, 40_000, 5_000);
    }
    let budget = Arc::new(LatencyBudget::new());
    let b = budget_of(&budget, &mconn, "o", 2, 5_000, 120_000).expect("budget");
    // mean 8.3s, sigma ~15.4s -> 8.3 + 46.2 + 1 = ~55.5s.
    assert!(b >= 50_000, "outlier widens the learned spread: {b}");
}

#[test]
fn highest_bucket_without_samples_returns_none() {
    let mconn = shared_conn(test_conn());
    {
        let c = mconn.lock();
        for _ in 0..10 {
            insert(&c, "h", 11, 2_000, 1_000);
        }
    }
    let budget = Arc::new(LatencyBudget::new());
    assert!(budget_of(&budget, &mconn, "h", 11, 500_000, 60_000).is_none());
}

#[test]
fn exactly_min_samples_is_trusted_without_widening() {
    let mconn = shared_conn(test_conn());
    {
        let c = mconn.lock();
        for ttft in [2_000i64, 2_100, 2_200, 2_300, 2_400, 2_500] {
            insert(&c, "e", 12, ttft, 2_000);
        }
    }
    let budget = Arc::new(LatencyBudget::new());
    let b = budget_of(&budget, &mconn, "e", 12, 2_000, 60_000).expect("exactly-min bucket trusted");
    // mean 2250, sigma ~190 -> 2250 + 570 + 1000 = 3820 -> floor 5000.
    assert_eq!(b, 5_000);
}

#[test]
fn five_samples_widen_up() {
    let mconn = shared_conn(test_conn());
    {
        let c = mconn.lock();
        for _ in 0..5 {
            insert(&c, "v", 13, 2_000, 2_000);
        }
        for _ in 0..10 {
            insert(&c, "v", 13, 9_000, 10_000);
        }
    }
    let budget = Arc::new(LatencyBudget::new());
    let b = budget_of(&budget, &mconn, "v", 13, 2_500, 60_000).expect("widened");
    // Widened to bucket 2: 9s + 0 + 1 = 10s (above the floor).
    assert_eq!(b, 10_000, "widened to the slower population: {b}");
}

#[test]
fn stress_many_targets_isolation_and_no_panic() {
    let mconn = shared_conn(test_conn());
    {
        let c = mconn.lock();
        for target in 0..100i64 {
            for i in 0..12i64 {
                insert(&c, "stress", target, 1_000 + (i * 50) % 400, 3_000);
            }
        }
    }
    let budget = Arc::new(LatencyBudget::new());
    for target in 0..100i64 {
        let b = budget_of(&budget, &mconn, "stress", target, 3_000, 60_000)
            .expect("every target has data");
        assert!((5_000..=8_000).contains(&b), "target {target} budget {b}");
    }
    // Unknown target still None.
    assert!(budget_of(&budget, &mconn, "stress", 999, 3_000, 60_000).is_none());
}

// ------------------------------------------------------------------
// Correction 1: anti-starvation relax from recorded cuts.
// ------------------------------------------------------------------

#[test]
fn recorded_cut_relaxes_the_next_budget() {
    let mconn = shared_conn(test_conn());
    {
        let c = mconn.lock();
        for _ in 0..20 {
            insert(&c, "cut", 7, 2_000, 5_000);
        }
        // One recorded headers-phase cut at 8274ms (what the budget did).
        insert_cut(&c, "cut", 7, 8_274);
    }
    let budget = Arc::new(LatencyBudget::new());
    let b = budget_of(&budget, &mconn, "cut", 7, 5_000, 60_000).expect("budget");
    // Statistical budget would be the 5s floor; the cut relaxes to
    // 8274 * 1.5 = 12411.
    assert_eq!(b, 12_411, "cut relaxes the next budget: {b}");
}

/// Regression (production, 2026-11-09): streaming cuts are recorded as
/// `in phase ttft after Nms` — the streaming mapper renames `Headers` to
/// its config hint. Gating the censored query and parser on the legacy
/// `headers` spelling left the anti-starvation relax dead for all
/// streaming traffic: targets whose learned budget sat just below their
/// real need were re-cut at the same budget forever (40+ identical cuts
/// on one target in 3 days).
#[test]
fn recorded_ttft_named_cut_relaxes_the_next_budget() {
    let mconn = shared_conn(test_conn());
    {
        let c = mconn.lock();
        for _ in 0..20 {
            insert(&c, "ttftcut", 10, 2_000, 5_000);
        }
        insert_cut_named(&c, "ttftcut", 10, 8_274, "ttft");
    }
    let budget = Arc::new(LatencyBudget::new());
    let b = budget_of(&budget, &mconn, "ttftcut", 10, 5_000, 60_000).expect("budget");
    assert_eq!(b, 12_411, "ttft-named cut relaxes the next budget: {b}");
}

#[test]
fn cut_messages_parse_in_both_phase_spellings() {
    assert_eq!(
        parse_headers_timeout_ms("upstream timeout in phase ttft after 20000ms"),
        Some(20_000)
    );
    assert_eq!(
        parse_headers_timeout_ms("upstream timeout in phase headers after 8274ms"),
        Some(8_274)
    );
    // Any other error text is not a cut.
    assert_eq!(
        parse_headers_timeout_ms("rate limited: provider=x retry_after_ms=300000"),
        None
    );
    assert_eq!(
        parse_headers_timeout_ms("upstream timeout in phase idle_chunk after 30000ms"),
        None
    );
}

#[test]
fn relax_reaching_ceiling_turns_the_predictor_off() {
    let mconn = shared_conn(test_conn());
    {
        let c = mconn.lock();
        for _ in 0..20 {
            insert(&c, "off", 8, 2_000, 5_000);
        }
        // A cut near the global timeout: relax 50k*1.5 = 75k > 60k.
        insert_cut(&c, "off", 8, 50_000);
    }
    let budget = Arc::new(LatencyBudget::new());
    assert!(
        budget_of(&budget, &mconn, "off", 8, 5_000, 60_000).is_none(),
        "the predictor must revert to the global timeout for this target"
    );
}

#[test]
fn note_censored_relaxes_without_db_row() {
    // The executor notes a cut in memory: the very next budget for the
    // target is relaxed without any DB write or refresh (corrections 1+8).
    let mconn = shared_conn(test_conn());
    {
        let c = mconn.lock();
        for _ in 0..20 {
            insert(&c, "instant", 9, 2_000, 5_000);
        }
    }
    let budget = Arc::new(LatencyBudget::new());
    let before = budget_of(&budget, &mconn, "instant", 9, 5_000, 60_000).unwrap();
    budget.note_censored("instant", ModelRowId(9), None, 9_000);
    let after = budget_of(&budget, &mconn, "instant", 9, 5_000, 60_000).unwrap();
    assert_eq!(before, 5_000);
    assert_eq!(after, 13_500, "9s cut * 1.5 relaxes immediately: {after}");
}

// ------------------------------------------------------------------
// Correction 5: estimated-token population preference.
// ------------------------------------------------------------------

#[test]
fn estimated_population_preferred_for_buckets() {
    let mconn = shared_conn(test_conn());
    {
        let c = mconn.lock();
        // 20 rows with REAL upstream tokens at 20s (different tokenizer:
        // not comparable with the dispatch estimate).
        for _ in 0..20 {
            insert_est(&c, "est", 4, 20_000, 30_000, 0);
        }
        // 10 rows estimated the same way dispatch does: 3s.
        for _ in 0..10 {
            insert_est(&c, "est", 4, 3_000, 30_000, 1);
        }
    }
    let budget = Arc::new(LatencyBudget::new());
    let b = budget_of(&budget, &mconn, "est", 4, 30_000, 60_000).expect("budget");
    assert!(b <= 6_000, "estimated population drives the bucket: {b}");
}

#[test]
fn thin_estimated_population_falls_back_to_all_rows() {
    let mconn = shared_conn(test_conn());
    {
        let c = mconn.lock();
        // Only 5 estimated rows (below MIN_BUCKET_SAMPLES): all rows mix.
        for _ in 0..5 {
            insert_est(&c, "thin", 5, 3_000, 30_000, 1);
        }
        for _ in 0..20 {
            insert_est(&c, "thin", 5, 20_000, 30_000, 0);
        }
    }
    let budget = Arc::new(LatencyBudget::new());
    let b = budget_of(&budget, &mconn, "thin", 5, 30_000, 60_000).expect("budget");
    assert!(b >= 20_000, "mixed population when estimates are thin: {b}");
}

// ------------------------------------------------------------------
// Correction 6: recency weighting counters regime erosion.
// ------------------------------------------------------------------

#[test]
fn recency_weighting_pulls_toward_the_current_regime() {
    let mconn = shared_conn(test_conn());
    {
        let c = mconn.lock();
        // Old regime: 50 slow samples from 4 days ago.
        for _ in 0..50 {
            insert_at(&c, "erode", 6, 30_000, 5_000, "datetime('now', '-4 days')");
        }
        // Current regime: 50 fast samples now.
        for _ in 0..50 {
            insert(&c, "erode", 6, 2_000, 5_000);
        }
    }
    let budget = Arc::new(LatencyBudget::new());
    let b = budget_of(&budget, &mconn, "erode", 6, 5_000, 60_000).expect("budget");
    // Unweighted mean is 16s -> budget ~59k. With the 48h half-life the
    // current fast regime dominates the mean (7.6k); p99 keeps a 30k
    // lower bound, so the budget lands well below the unweighted one.
    assert!(b < 45_000, "recency pulls the budget down: {b}");
    assert!(b >= 30_000, "the historical tail still bounds it: {b}");
}

// ------------------------------------------------------------------
// Correction 3: the snapshot map is capped and evicts the oldest.
// ------------------------------------------------------------------

#[test]
fn cache_cap_evicts_oldest_snapshots() {
    let mconn = shared_conn(test_conn());
    {
        let c = mconn.lock();
        for target in 0..130i64 {
            for _ in 0..6 {
                insert(&c, "cap", target, 2_000, 5_000);
            }
        }
    }
    // Small caps so the eviction triggers in-test.
    let budget = Arc::new({
        let mut b = LatencyBudget::new();
        b.cache_cap = 120;
        b.cache_evict = 30;
        b
    });
    // Warm all 130 targets: the map overflows and evicts the oldest 30.
    {
        let c = mconn.lock();
        for target in 0..130i64 {
            budget.warm_target(&c, "cap", target);
        }
        // Targets evicted: delete their rows. A later call must reload
        // from the DB (now empty) -> None.
        c.execute(
            "DELETE FROM usage WHERE provider_id = 'cap' AND model_row_id IN (0, 1, 2, 3, 4)",
            [],
        )
        .unwrap();
    }
    assert!(
        budget_of(&budget, &mconn, "cap", 3, 5_000, 60_000).is_none(),
        "evicted target with no rows left must reload to None"
    );
    // A recently-loaded target stays cached even after its rows vanish
    // (TTL freshness), proving the cap did not nuke the whole map.
    {
        let c = mconn.lock();
        c.execute(
            "DELETE FROM usage WHERE provider_id = 'cap' AND model_row_id = 129",
            [],
        )
        .unwrap();
    }
    assert!(
        budget_of(&budget, &mconn, "cap", 129, 5_000, 60_000).is_some(),
        "recent snapshots survive the eviction (TTL-fresh cache)"
    );
}

// ------------------------------------------------------------------
// Correction 7: no provider is special-cased.
// ------------------------------------------------------------------

#[test]
fn antigravity_targets_are_predicted_like_any_other() {
    let mconn = shared_conn(test_conn());
    {
        let c = mconn.lock();
        for _ in 0..20 {
            insert(&c, "antigravity", 5, 3_000, 5_000);
        }
    }
    let budget = Arc::new(LatencyBudget::new());
    // Ceiling = antigravity's resolved timeout with the 120s thinking
    // floor already applied: the predictor shortens inside it.
    let b = budget_of(&budget, &mconn, "antigravity", 5, 5_000, 120_000)
        .expect("antigravity gets predictions");
    // mean 3k + 0 + 1k = 4k -> floor 5s.
    assert_eq!(b, 5_000);
    // And "agy" alias too.
    {
        let c = mconn.lock();
        for _ in 0..20 {
            insert(&c, "agy", 6, 3_000, 5_000);
        }
    }
    let b2 =
        budget_of(&budget, &mconn, "agy", 6, 5_000, 120_000).expect("agy alias gets predictions");
    assert_eq!(b2, 5_000);
}

// ------------------------------------------------------------------
// Correction 4: pool-mode background refresh + serve-stale.
// ------------------------------------------------------------------

#[tokio::test]
async fn pool_mode_refreshes_in_background() {
    let pool =
        openproxy_db::DbPool::test_pool_with_prefix("openproxy-lb-pool-refresh").expect("pool");
    {
        let conn = pool.open_connection().expect("conn");
        for _ in 0..20 {
            conn.execute(
                "INSERT INTO usage (request_id, trace_id, attempt, provider_id, upstream_model_id, \
                     status_code, ttft_ms, prompt_tokens, created_at) \
                 VALUES ('r', 't', 1, 'poolprov', 'm', 200, 3000, 5000, datetime('now'))",
                [],
            )
            .unwrap();
        }
        conn.execute(
            "UPDATE usage SET model_row_id = 7 WHERE model_row_id IS NULL",
            [],
        )
        .unwrap();
    }
    let budget = Arc::new(LatencyBudget::with_pool(pool));
    // First call: no snapshot yet -> None, and the background refresh is
    // scheduled (no shared-conn lock, no panic outside tests).
    assert!(
        budget
            .ttft_budget_ms("poolprov", ModelRowId(7), None, 5_000, 60_000, None)
            .is_none(),
        "cold cache without sync conn answers None"
    );
    // Let the background refresh land.
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if budget
            .ttft_budget_ms("poolprov", ModelRowId(7), None, 5_000, 60_000, None)
            .is_some()
        {
            return;
        }
    }
    panic!("background refresh never landed a snapshot");
}

/// The predictor runs against the REAL production schema, not the
/// minimal test one: openproxy_db's own test pool carries every column
/// and index the live query touches.
#[test]
fn real_production_schema_query_works() {
    let pool =
        openproxy_db::DbPool::test_pool_with_prefix("openproxy-latency-budget").expect("pool");
    let conn = pool.open_connection().expect("conn");
    for ttft in [1_500i64, 1_800, 2_200, 1_900, 2_100, 2_000, 1_700, 2_300] {
        conn.execute(
            "INSERT INTO usage (request_id, trace_id, attempt, provider_id, upstream_model_id, \
                 status_code, ttft_ms, prompt_tokens, created_at) \
             VALUES ('r', 't', 1, 'prov', 'm', 200, ?1, 5000, datetime('now'))",
            rusqlite::params![ttft],
        )
        .expect("insert usage row");
    }
    conn.execute(
        "UPDATE usage SET model_row_id = 42 WHERE model_row_id IS NULL",
        [],
    )
    .expect("set model_row_id");
    let mconn = shared_conn(conn);
    let budget = Arc::new(LatencyBudget::new());
    let b = budget_of(&budget, &mconn, "prov", 42, 5_000, 60_000)
        .expect("production schema yields a budget");
    // mean ~1937, sigma ~269 -> 1937 + 807 + 1000 = ~3744 -> floor 5000.
    assert_eq!(b, 5_000, "production schema: {b}");
}

// ----------
// Hierarchical predictor: global (1h) + session (5min)
// ----------

#[test]
fn session_level_overrides_global_when_fresher() {
    // Warm global first (20 samples at 2s -> 5s floor, cached), then a
    // session whose own regime is a 20s cold start. The session level
    // answers with ITS population, not the inherited global norm.
    let mconn = shared_conn(test_conn());
    let budget = Arc::new(LatencyBudget::new());
    {
        let c = mconn.lock();
        for _ in 0..20 {
            insert(&c, "mix", 1, 2_000, 5_000);
        }
    }
    let global = budget_of(&budget, &mconn, "mix", 1, 5_000, 60_000).unwrap();
    assert_eq!(global, 5_000);
    {
        let c = mconn.lock();
        for _ in 0..10 {
            insert_sess(&c, "mix", 1, 12345, 20_000, 5_000);
        }
    }
    let sess = budget_of_sess(&budget, &mconn, "mix", 1, Some(12345), 5_000, 60_000).unwrap();
    assert_eq!(
        sess, 21_000,
        "session budget reflects its cold regime: {sess}"
    );
    assert!(
        sess != global,
        "the session level answers with its own statistics: {sess} vs {global}"
    );
}

#[test]
fn session_without_data_falls_back_to_global() {
    // Global: 20 samples at 2s.
    // Session: no rows -> must fall back to global.
    let mconn = shared_conn(test_conn());
    {
        let c = mconn.lock();
        for _ in 0..20 {
            insert(&c, "fb", 2, 2_000, 5_000);
        }
    }
    let budget = Arc::new(LatencyBudget::new());
    let global = budget_of(&budget, &mconn, "fb", 2, 5_000, 60_000).unwrap();
    let sess = budget_of_sess(&budget, &mconn, "fb", 2, Some(999), 5_000, 60_000);
    assert_eq!(global, 5_000);
    assert_eq!(sess, Some(global), "empty session falls back to global");
}

#[test]
fn session_forgets_after_ttl_and_falls_back() {
    // Session: 10 samples at 20s, loaded long ago.
    // Global: 20 samples at 2s, fresh.
    // After the session TTL expires, the session falls back to global.
    let mconn = shared_conn(test_conn());
    let mut budget = LatencyBudget::new();
    budget.session_ttl = Duration::from_millis(50); // short TTL for the test
    budget.global_ttl = Duration::from_secs(300);
    let budget = Arc::new(budget);
    {
        let c = mconn.lock();
        for _ in 0..20 {
            insert(&c, "ttl", 3, 2_000, 5_000);
        }
    }
    // Warm the global level first: its 5s floor is the fallback value.
    assert_eq!(
        budget_of(&budget, &mconn, "ttl", 3, 5_000, 60_000),
        Some(5_000)
    );
    {
        let c = mconn.lock();
        for _ in 0..10 {
            insert_sess(&c, "ttl", 3, 54321, 20_000, 5_000);
        }
        budget.warm_session(&c, "ttl", 3, 54321);
    }
    // Immediate: session answers from its own rows.
    let sess_now = budget_of_sess(&budget, &mconn, "ttl", 3, Some(54321), 5_000, 60_000).unwrap();
    assert_eq!(sess_now, 21_000, "session fresh: {sess_now}");
    // Rows vanish (the conversation moved on): within the TTL the
    // snapshot still answers, past it the level reloads and falls back.
    {
        let c = mconn.lock();
        c.execute("DELETE FROM usage WHERE session_hash IS NOT NULL", [])
            .unwrap();
    }
    let sess_cached =
        budget_of_sess(&budget, &mconn, "ttl", 3, Some(54321), 5_000, 60_000).unwrap();
    assert_eq!(sess_cached, 21_000, "TTL holds the session snapshot");
    std::thread::sleep(Duration::from_millis(80));
    let sess_later = budget_of_sess(&budget, &mconn, "ttl", 3, Some(54321), 5_000, 60_000).unwrap();
    assert_eq!(sess_later, 5_000, "expired session falls back to global");
}

#[test]
fn session_censored_max_is_isolated() {
    // Global: 20 samples at 2s, no cuts.
    // Session A: 10 samples at 2s, one cut at 9s -> relax to 13.5s.
    // Session B: 10 samples at 2s, no cuts -> floor.
    let mconn = shared_conn(test_conn());
    {
        let c = mconn.lock();
        for _ in 0..20 {
            insert(&c, "iso", 4, 2_000, 5_000);
        }
        let hash_a: u64 = 111;
        let hash_b: u64 = 222;
        for _ in 0..10 {
            insert_sess(&c, "iso", 4, hash_a as i64, 2_000, 5_000);
            insert_sess(&c, "iso", 4, hash_b as i64, 2_000, 5_000);
        }
        insert_sess(&c, "iso", 4, hash_a as i64, 0, 5_000); // dummy for cut note
    }
    let budget = Arc::new(LatencyBudget::new());
    {
        let c = mconn.lock();
        budget.warm_session(&c, "iso", 4, 111);
        budget.warm_session(&c, "iso", 4, 222);
    }
    // Note a cut on session A only.
    budget.note_censored("iso", ModelRowId(4), Some(111), 9_000);
    let sess_a = budget_of_sess(&budget, &mconn, "iso", 4, Some(111), 5_000, 60_000).unwrap();
    let sess_b = budget_of_sess(&budget, &mconn, "iso", 4, Some(222), 5_000, 60_000).unwrap();
    let global = budget_of(&budget, &mconn, "iso", 4, 5_000, 60_000).unwrap();
    assert_eq!(sess_a, 13_500, "session A relaxed by its own cut: {sess_a}");
    assert_eq!(sess_b, 5_000, "session B untouched: {sess_b}");
    assert_eq!(global, 5_000, "global untouched: {global}");
}

#[test]
fn session_window_forgets_the_previous_regime() {
    // The session ran a cold regime 20 minutes ago (20s) and is warm
    // now (2s). The window only keeps the last ten minutes, so the old
    // regime is forgotten and the budget drops back to the floor.
    let mconn = shared_conn(test_conn());
    {
        let c = mconn.lock();
        for _ in 0..20 {
            insert_sess_at(
                &c,
                "win",
                5,
                777,
                20_000,
                5_000,
                "datetime('now', '-20 minutes')",
            );
        }
        for _ in 0..10 {
            insert_sess(&c, "win", 5, 777, 2_000, 5_000);
        }
    }
    let budget = Arc::new(LatencyBudget::new());
    let sess = budget_of_sess(&budget, &mconn, "win", 5, Some(777), 5_000, 60_000).unwrap();
    assert_eq!(
        sess, 5_000,
        "stale regime outside the window is forgotten: {sess}"
    );
}

// ----------
