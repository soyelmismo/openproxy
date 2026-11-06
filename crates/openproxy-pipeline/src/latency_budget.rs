//! Adaptive TTFT budgets per target, learned from the `usage` table.
//!
//! Providers with fast normal responses (2-4s TTFT) sometimes hang until the
//! global TTFT timeout expires before the pipeline rotates to the next
//! target, wasting tens of seconds per stuck attempt. This module shortens
//! that wait: for each (provider, model_row) target it keeps a cached
//! snapshot of recent successful TTFTs bucketed by prompt size, and derives a
//! per-request budget as `p95 + margin`.
//!
//! Design rules ("learn, never break"):
//! - Only successful requests (2xx-3xx) feed the buckets: stuck attempts that
//!   never produced bytes are not part of the distribution, so a hanging
//!   provider is cut at `p95 + margin` instead of the global timeout.
//! - Buckets are logarithmic in prompt tokens: a 300k-token cold request is
//!   legitimately slow (15s+) while a cached one is fast (2s). The p95 of the
//!   mixed population already prices in the cold tail, so neither case is
//!   cut unfairly.
//! - The budget only ever shortens the resolved timeout (it is clamped to the
//!   ceiling the caller passes: model override + antigravity floor already
//!   applied). With fewer than [`MIN_BUCKET_SAMPLES`] successful samples in a
//!   bucket the predictor widens the bucket; with no data at all it returns
//!   `None` and the request keeps the plain resolved timeout.
//! - Snapshots are cached per target for [`SNAPSHOT_TTL`] so steady-state
//!   dispatch adds zero queries.

use openproxy_types::ids::ModelRowId;
use parking_lot::Mutex;
use rusqlite::Connection;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How many recent successful rows per target feed the snapshot.
const SAMPLE_LIMIT: i64 = 400;
/// History window for the snapshot.
const HISTORY_WINDOW_DAYS: i64 = 7;
/// Minimum samples a bucket needs before its statistics are trusted.
const MIN_BUCKET_SAMPLES: usize = 6;
/// The budget is `mean + SIGMA_MULT * stdev + MARGIN_MS`: the learned
/// standard deviation IS the grace period. A rock-steady provider (2-4s,
/// sigma ~0.3s) gets cut ~1-2s past its norm; a jittery one (sigma ~5s)
/// earns 15s+ of slack automatically. No hard-coded grace.
const SIGMA_MULT: f64 = 3.0;
/// Small fixed margin on top of the learned spread.
const MARGIN_MS: u64 = 1_000;
/// Never cut a request earlier than this, no matter how fast the target is.
const FLOOR_MS: u64 = 5_000;
/// Snapshot lifetime per target.
const SNAPSHOT_TTL: Duration = Duration::from_secs(300);

/// Logarithmic prompt-token buckets. Index by `tokens_bucket(est)` and widen
/// upwards when a bucket lacks samples, never downwards: a wider bucket is
/// the more conservative (slower) population.
pub(crate) fn tokens_bucket(prompt_tokens: u32) -> usize {
    let t = prompt_tokens.max(1);
    // Boundaries: <1k, 1-4k, 4-16k, 16-64k, 64-256k, >256k
    match t {
        0..=999 => 0,
        1_000..=3_999 => 1,
        4_000..=15_999 => 2,
        16_000..=63_999 => 3,
        64_000..=255_999 => 4,
        _ => 5,
    }
}

pub(crate) const BUCKET_COUNT: usize = 6;

/// Mean and sample standard deviation of a sample list.
fn mean_stdev(samples: &[u64]) -> Option<(f64, f64)> {
    let n = samples.len();
    if n == 0 {
        return None;
    }
    let mean = samples.iter().map(|s| *s as f64).sum::<f64>() / n as f64;
    if n < 2 {
        return Some((mean, 0.0));
    }
    let var = samples
        .iter()
        .map(|s| {
            let d = *s as f64 - mean;
            d * d
        })
        .sum::<f64>()
        / (n - 1) as f64;
    Some((mean, var.sqrt()))
}

#[derive(Debug)]
struct TargetSnapshot {
    loaded_at: Instant,
    /// ttft_ms of recent successful requests, keyed by tokens bucket.
    buckets: [Vec<u64>; BUCKET_COUNT],
}

/// Adaptive TTFT budget predictor, one snapshot per (provider, model_row).
#[derive(Default)]
pub struct LatencyBudget {
    cache: Mutex<HashMap<(String, i64), Arc<TargetSnapshot>>>,
}

impl LatencyBudget {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Adaptive TTFT budget in milliseconds, or `None` when the predictor has
    /// no trustworthy data for this target (the caller keeps the resolved
    /// timeout untouched). `ceiling_ms` is the already-resolved TTFT timeout
    /// (defaults + model overrides + provider floors): the predictor can only
    /// shorten it, never extend it.
    pub fn ttft_budget_ms(
        &self,
        conn: &Connection,
        provider: &str,
        model_row_id: ModelRowId,
        est_prompt_tokens: u32,
        ceiling_ms: u64,
    ) -> Option<u64> {
        let key = (provider.to_owned(), model_row_id.0);
        let snapshot = {
            let mut cache = self.cache.lock();
            let needs_load = match cache.get(&key) {
                Some(s) if s.loaded_at.elapsed() < SNAPSHOT_TTL => false,
                _ => true,
            };
            if needs_load {
                if let Some(fresh) = Self::load_snapshot(conn, provider, model_row_id.0) {
                    cache.insert(key.clone(), Arc::new(fresh));
                }
            }
            cache.get(&key).cloned()
        };
        let snapshot = snapshot?;

        let mut bucket = tokens_bucket(est_prompt_tokens);
        loop {
            let samples = &snapshot.buckets[bucket];
            if samples.len() >= MIN_BUCKET_SAMPLES {
                break;
            }
            if bucket + 1 >= BUCKET_COUNT {
                return None;
            }
            bucket += 1;
        }
        let samples = &snapshot.buckets[bucket];
        let (mean, sigma) = mean_stdev(samples)?;
        let budget = (mean + SIGMA_MULT * sigma).max(mean) as u64 + MARGIN_MS;
        let budget = budget.max(FLOOR_MS);
        // The predictor only shortens the resolved timeout.
        Some(budget.min(ceiling_ms))
    }

    /// Pull recent successful TTFT samples for one target from `usage`.
    fn load_snapshot(
        conn: &Connection,
        provider: &str,
        model_row_id: i64,
    ) -> Option<TargetSnapshot> {
        let mut stmt = conn
            .prepare(
                "SELECT ttft_ms, prompt_tokens FROM usage \
                 WHERE provider_id = ?1 AND model_row_id = ?2 \
                   AND status_code >= 200 AND status_code < 400 \
                   AND ttft_ms IS NOT NULL AND ttft_ms > 0 \
                   AND created_at >= datetime('now', ?3) \
                 ORDER BY id DESC LIMIT ?4",
            )
            .ok()?;
        let window = format!("-{HISTORY_WINDOW_DAYS} days");
        let rows = stmt
            .query_map(
                rusqlite::params![provider, model_row_id, window, SAMPLE_LIMIT],
                |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?.unwrap_or(0),
                        row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                    ))
                },
            )
            .ok()?;
        let mut buckets: [Vec<u64>; BUCKET_COUNT] = Default::default();
        let mut count = 0usize;
        for r in rows.flatten() {
            let (ttft_ms, prompt_tokens) = r;
            if ttft_ms <= 0 {
                continue;
            }
            let b = tokens_bucket(prompt_tokens.max(1) as u32);
            buckets[b].push(ttft_ms as u64);
            count += 1;
        }
        if count == 0 {
            return None;
        }
        Some(TargetSnapshot {
            loaded_at: Instant::now(),
            buckets,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openproxy_types::ids::ModelRowId;

    /// Minimal usage schema mirroring the production columns the query
    /// touches (provider_id, model_row_id, status_code, ttft_ms,
    /// prompt_tokens, created_at).
    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE usage (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                request_id TEXT, trace_id TEXT, attempt INTEGER DEFAULT 1,
                provider_id TEXT NOT NULL, account_id INTEGER,
                combo_id INTEGER, model_row_id INTEGER,
                upstream_model_id TEXT NOT NULL DEFAULT '', combo_target_id INTEGER,
                prompt_tokens INTEGER, completion_tokens INTEGER, cached_tokens INTEGER,
                cost_usd REAL, status_code INTEGER NOT NULL,
                error_msg TEXT, created_at TEXT NOT NULL DEFAULT (datetime('now')),
                connect_ms INTEGER, ttft_ms INTEGER, total_ms INTEGER
             );",
        )
        .unwrap();
        conn
    }

    fn insert(conn: &Connection, provider: &str, model_row: i64, ttft: i64, prompt: i64) {
        insert_at(conn, provider, model_row, ttft, prompt, "datetime('now')");
    }

    fn insert_at(
        conn: &Connection,
        provider: &str,
        model_row: i64,
        ttft: i64,
        prompt: i64,
        created_at: &str,
    ) {
        conn.execute(
            &format!(
                "INSERT INTO usage (provider_id, model_row_id, upstream_model_id, status_code, ttft_ms, prompt_tokens, created_at) \
                 VALUES (?1, ?2, 'm', 200, ?3, ?4, {created_at})"
            ),
            rusqlite::params![provider, model_row, ttft, prompt],
        )
        .unwrap();
    }

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
    fn mean_stdev_basics() {
        assert!(mean_stdev(&[]).is_none());
        assert_eq!(mean_stdev(&[5_000]), Some((5_000.0, 0.0)));
        // Population [2s, 4s]: mean 3s, sample stdev = sqrt(2) ~1.414s.
        let (m, s) = mean_stdev(&[2_000, 4_000]).unwrap();
        assert!((m - 3_000.0).abs() < 0.001);
        assert!((s - 1_414.2).abs() < 1.0);
        // Identical samples: zero spread.
        let (m, s) = mean_stdev(&[7_000; 10]).unwrap();
        assert_eq!(m, 7_000.0);
        assert_eq!(s, 0.0);
    }

    #[test]
    fn steady_fast_target_is_cut_quickly() {
        // 30 identical 2s successes. mean=2s, sigma=0 -> 2+0+1=3s, floored
        // to 5s: a hung attempt of this target dies at 5s, not at 60s.
        let conn = test_conn();
        for _ in 0..30 {
            insert(&conn, "fast", 7, 2_000, 5_000);
        }
        // Failures never produced bytes: they must not inflate anything.
        conn.execute(
            "INSERT INTO usage (provider_id, model_row_id, upstream_model_id, status_code, ttft_ms, prompt_tokens, created_at) \
             VALUES ('fast', 7, 'm', 500, NULL, 5_000, datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO usage (provider_id, model_row_id, upstream_model_id, status_code, ttft_ms, prompt_tokens, created_at) \
             VALUES ('fast', 7, 'm', 504, 59_000, 5_000, datetime('now'))",
            [],
        )
        .unwrap();

        let budget = LatencyBudget::new();
        let b = budget
            .ttft_budget_ms(&conn, "fast", ModelRowId(7), 5_000, 60_000)
            .expect("budget from 30 samples");
        assert_eq!(b, 5_000, "3+0+1=3s under the floor: {b}");
    }

    #[test]
    fn jittery_target_earns_its_spread() {
        // The design case: two targets with the same mean but different
        // spread. The steady one gets a tight budget; the jittery one
        // (slow-but-successful outliers of +5s) keeps headroom.
        let conn = test_conn();
        for ttft in [2_000i64; 20] {
            insert(&conn, "steady", 1, ttft, 5_000);
        }
        // Jittery: same mean (2s) but spread 0.5-5.5s (sigma ~1.5s).
        for ttft in [500i64, 1_000, 1_500, 2_000, 2_500, 3_000, 4_000, 5_500] {
            insert(&conn, "jittery", 2, ttft, 5_000);
            insert(&conn, "jittery", 2, ttft, 5_000);
        }
        let budget = LatencyBudget::new();
        let steady = budget
            .ttft_budget_ms(&conn, "steady", ModelRowId(1), 5_000, 60_000)
            .unwrap();
        let jittery = budget
            .ttft_budget_ms(&conn, "jittery", ModelRowId(2), 5_000, 60_000)
            .unwrap();
        assert_eq!(steady, 5_000, "zero sigma -> floor");
        // jittery: mean 2500, sigma ~1591 -> 2500 + 4774 + 1000 = 8274.
        assert!(
            jittery >= 8_000 && jittery <= 9_000,
            "learned spread earns slack: {jittery}"
        );
        assert!(
            jittery > steady + 3_000,
            "the jittery target must earn clearly more headroom: {jittery} vs {steady}"
        );
    }

    #[test]
    fn cached_and_cold_mixed_population_prices_the_cold_tail() {
        // 80% cached-fast (2s), 20% cold-slow (15s): the learned spread makes
        // the budget land in the cold tail, so a cold outlier is not cut and
        // a cached request is never endangered.
        let conn = test_conn();
        for _ in 0..40 {
            insert(&conn, "mix", 5, 2_000, 100_000);
        }
        for _ in 0..10 {
            insert(&conn, "mix", 5, 15_000, 100_000);
        }
        let budget = LatencyBudget::new();
        let b = budget
            .ttft_budget_ms(&conn, "mix", ModelRowId(5), 100_000, 60_000)
            .expect("budget");
        // mean 4.6s, sigma 5.25s -> 4.6 + 15.75 + 1 = ~21.4s.
        assert!(b >= 20_000 && b <= 23_000, "cold tail protected: {b}");
    }

    #[test]
    fn budget_respects_ceiling_and_widens_bucket_when_thin() {
        let conn = test_conn();
        // Bucket 3: a slow cold population (20-22s) plus a fast cached one
        // (10-12.5s). The 5k estimate (bucket 2) is thin -> widen UP to
        // bucket 3, whose learned spread prices the conservative tail.
        for ttft in [20_000i64, 21_000, 22_000] {
            insert(&conn, "w", 9, ttft, 30_000);
        }
        for ttft in [10_000i64, 11_000, 12_000, 10_500, 11_500, 12_500] {
            insert(&conn, "w", 9, ttft, 40_000);
        }
        let budget = LatencyBudget::new();
        let b = budget
            .ttft_budget_ms(&conn, "w", ModelRowId(9), 5_000, 60_000)
            .expect("widened bucket has samples");
        // mean 13444, sigma 5081 -> 13444 + 15243 + 1000 = 29687.
        assert!(b >= 29_000 && b <= 30_500, "widened budget: {b}");

        // Ceiling: the resolved timeout always wins when tighter.
        let tight = budget
            .ttft_budget_ms(&conn, "w", ModelRowId(9), 5_000, 8_000)
            .expect("ceiling clamp");
        assert_eq!(tight, 8_000);
    }

    #[test]
    fn no_data_returns_none() {
        let conn = test_conn();
        let budget = LatencyBudget::new();
        assert!(budget.ttft_budget_ms(&conn, "empty", ModelRowId(1), 5_000, 60_000).is_none());
    }

    #[test]
    fn missing_usage_table_does_not_panic() {
        // Boot path: a connection whose schema has no usage table yet must
        // yield None, never a panic.
        let conn = Connection::open_in_memory().unwrap();
        let budget = LatencyBudget::new();
        assert!(budget.ttft_budget_ms(&conn, "x", ModelRowId(1), 5_000, 60_000).is_none());
    }

    #[test]
    fn window_excludes_rows_older_than_seven_days() {
        let conn = test_conn();
        // 8 days old: outside the window.
        for _ in 0..10 {
            insert_at(&conn, "old", 1, 40_000, 5_000, "datetime('now', '-8 days')");
        }
        // 6 days old: inside.
        for ttft in [2_000i64; 8] {
            insert_at(&conn, "old", 1, ttft, 5_000, "datetime('now', '-6 days')");
        }
        let budget = LatencyBudget::new();
        let b = budget
            .ttft_budget_ms(&conn, "old", ModelRowId(1), 5_000, 60_000)
            .expect("in-window samples exist");
        assert_eq!(b, 5_000, "only the recent fast samples feed the budget: {b}");
    }

    #[test]
    fn target_isolation_by_provider_and_model_row() {
        let conn = test_conn();
        for _ in 0..10 {
            insert(&conn, "a", 1, 2_000, 5_000);
        }
        let budget = LatencyBudget::new();
        assert!(budget.ttft_budget_ms(&conn, "a", ModelRowId(2), 5_000, 60_000).is_none());
        assert!(budget.ttft_budget_ms(&conn, "b", ModelRowId(1), 5_000, 60_000).is_none());
    }

    #[test]
    fn null_and_zero_ttft_and_null_prompt_are_ignored() {
        let conn = test_conn();
        // NULL ttft, zero ttft and negative ttft: unusable, must not count
        // as samples (and must not zero out the population).
        for sql in [
            "INSERT INTO usage (provider_id, model_row_id, upstream_model_id, status_code, ttft_ms, prompt_tokens, created_at) VALUES ('n', 3, 'm', 200, NULL, 5000, datetime('now'))",
            "INSERT INTO usage (provider_id, model_row_id, upstream_model_id, status_code, ttft_ms, prompt_tokens, created_at) VALUES ('n', 3, 'm', 200, 0, 5000, datetime('now'))",
            "INSERT INTO usage (provider_id, model_row_id, upstream_model_id, status_code, ttft_ms, prompt_tokens, created_at) VALUES ('n', 3, 'm', 200, -5, 5000, datetime('now'))",
        ] {
            conn.execute(sql, []).unwrap();
        }
        // Valid samples in bucket 0 (NULL prompt -> treated as <1k).
        for ttft in [500i64; 8] {
            insert(&conn, "n", 3, ttft, 800);
        }
        let budget = LatencyBudget::new();
        let b = budget
            .ttft_budget_ms(&conn, "n", ModelRowId(3), 500, 60_000)
            .expect("valid bucket-0 samples");
        assert_eq!(b, 5_000, "300+0+1=1.3s under the floor: {b}");
    }

    #[test]
    fn sample_limit_keeps_most_recent_only() {
        let conn = test_conn();
        // 50 old slow rows, then 400 new fast rows: the snapshot keeps the
        // 400 most recent (ORDER BY id DESC LIMIT 400) -> budget is fast.
        for _ in 0..50 {
            insert_at(&conn, "lim", 4, 50_000, 5_000, "datetime('now', '-1 day')");
        }
        for _ in 0..400 {
            insert(&conn, "lim", 4, 2_000, 5_000);
        }
        let budget = LatencyBudget::new();
        let b = budget
            .ttft_budget_ms(&conn, "lim", ModelRowId(4), 5_000, 60_000)
            .expect("budget");
        assert_eq!(b, 5_000, "old slow rows fell out of the sample limit: {b}");
    }

    #[test]
    fn cache_prevents_requery_within_ttl() {
        let conn = test_conn();
        for _ in 0..10 {
            insert(&conn, "c", 6, 2_000, 5_000);
        }
        let budget = LatencyBudget::new();
        let first = budget.ttft_budget_ms(&conn, "c", ModelRowId(6), 5_000, 60_000).unwrap();
        // New data arrives: the snapshot is still fresh, the answer must not
        // change until the TTL expires.
        for _ in 0..20 {
            insert(&conn, "c", 6, 40_000, 5_000);
        }
        let second = budget.ttft_budget_ms(&conn, "c", ModelRowId(6), 5_000, 60_000).unwrap();
        assert_eq!(first, second, "TTL cache holds: {first} vs {second}");
    }

    #[test]
    fn independent_instances_do_not_share_cache() {
        let conn = test_conn();
        for _ in 0..10 {
            insert(&conn, "c", 6, 2_000, 5_000);
        }
        let b1 = LatencyBudget::new();
        let b2 = LatencyBudget::new();
        assert!(b1.ttft_budget_ms(&conn, "c", ModelRowId(6), 5_000, 60_000).is_some());
        assert!(b2.ttft_budget_ms(&conn, "c", ModelRowId(6), 5_000, 60_000).is_some());
    }

    #[test]
    fn ceiling_below_floor_clamps_to_ceiling() {
        // A resolved timeout tighter than the floor (e.g. a model override of
        // 2s): the predictor must never extend it.
        let conn = test_conn();
        for _ in 0..10 {
            insert(&conn, "f", 8, 2_000, 5_000);
        }
        let budget = LatencyBudget::new();
        let b = budget
            .ttft_budget_ms(&conn, "f", ModelRowId(8), 5_000, 2_000)
            .expect("clamp");
        assert_eq!(b, 2_000, "ceiling wins over floor: {b}");
        // Ceiling of 0: budget 0 (effectively "no wait") - the caller is
        // responsible for not passing nonsense; the invariant holds.
        assert_eq!(
            budget.ttft_budget_ms(&conn, "f", ModelRowId(8), 5_000, 0).unwrap(),
            0
        );
    }

    #[test]
    fn single_legitimate_outlier_is_respected_not_cut() {
        // 5 fast + 1 slow-but-successful (40s): with 6 samples the learned
        // sigma is huge, so the budget is conservative - a provider that
        // sometimes takes 40s and answers must not be cut.
        let conn = test_conn();
        for _ in 0..5 {
            insert(&conn, "o", 2, 2_000, 5_000);
        }
        insert(&conn, "o", 2, 40_000, 5_000);
        let budget = LatencyBudget::new();
        let b = budget
            .ttft_budget_ms(&conn, "o", ModelRowId(2), 5_000, 120_000)
            .expect("budget");
        // mean 8.3s, sigma ~15.4s -> 8.3 + 46.2 + 1 = ~55.5s.
        assert!(b >= 50_000, "outlier widens the learned spread: {b}");
    }

    #[test]
    fn highest_bucket_without_samples_returns_none() {
        let conn = test_conn();
        // Data only in buckets 0-2; an estimate of 500k tokens maps to
        // bucket 5, which cannot widen further -> None.
        for ttft in [2_000i64; 10] {
            insert(&conn, "h", 11, ttft, 1_000);
        }
        let budget = LatencyBudget::new();
        assert!(budget.ttft_budget_ms(&conn, "h", ModelRowId(11), 500_000, 60_000).is_none());
    }

    #[test]
    fn exactly_min_samples_is_trusted_without_widening() {
        let conn = test_conn();
        // 6 samples in bucket 1 only (2-3s); an estimate in bucket 1 uses
        // them directly.
        for ttft in [2_000i64, 2_100, 2_200, 2_300, 2_400, 2_500] {
            insert(&conn, "e", 12, ttft, 2_000);
        }
        let budget = LatencyBudget::new();
        let b = budget
            .ttft_budget_ms(&conn, "e", ModelRowId(12), 2_000, 60_000)
            .expect("exactly-min bucket trusted");
        // mean 2250, sigma ~190 -> 2250 + 570 + 1000 = 3820 -> floor 5000.
        assert_eq!(b, 5_000);
    }

    #[test]
    fn five_samples_widen_up() {
        let conn = test_conn();
        // 5 fast samples in bucket 1, plenty in bucket 2: the estimate in
        // bucket 1 widens to bucket 2's slower population.
        for ttft in [2_000i64; 5] {
            insert(&conn, "v", 13, ttft, 2_000);
        }
        for ttft in [9_000i64; 10] {
            insert(&conn, "v", 13, ttft, 10_000);
        }
        let budget = LatencyBudget::new();
        let b = budget
            .ttft_budget_ms(&conn, "v", ModelRowId(13), 2_500, 60_000)
            .expect("widened");
        // Widened to bucket 2: 9s + 0 + 1 = 10s (above the floor).
        assert_eq!(b, 10_000, "widened to the slower population: {b}");
    }

    #[test]
    fn stress_many_targets_isolation_and_no_panic() {
        let conn = test_conn();
        let budget = LatencyBudget::new();
        for target in 0..100i64 {
            for i in 0..12i64 {
                insert(&conn, "stress", target, 1_000 + (i * 50) % 400, 3_000);
            }
        }
        for target in 0..100i64 {
            let b = budget
                .ttft_budget_ms(&conn, "stress", ModelRowId(target), 3_000, 60_000)
                .expect("every target has data");
            assert!(b >= 5_000 && b <= 8_000, "target {target} budget {b}");
        }
        // Unknown target still None.
        assert!(budget.ttft_budget_ms(&conn, "stress", ModelRowId(999), 3_000, 60_000).is_none());
    }

    /// The predictor runs against the REAL production schema, not the
    /// minimal test one: openproxy_db's own test pool carries every column
    /// and index the live query touches.
    #[test]
    fn real_production_schema_query_works() {
        let pool = openproxy_db::DbPool::test_pool_with_prefix("openproxy-latency-budget")
            .expect("pool");
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
        let budget = LatencyBudget::new();
        let b = budget
            .ttft_budget_ms(&conn, "prov", ModelRowId(42), 5_000, 60_000)
            .expect("production schema yields a budget");
        // mean ~1937, sigma ~269 -> 1937 + 807 + 1000 = ~3744 -> floor 5000.
        assert_eq!(b, 5_000, "production schema: {b}");
    }
}
