//! Adaptive TTFT budgets per target, learned from the `usage` table.
//!
//! `budget = max(mean + 3σ, empirical p99) + 1s`, floored and clamped to the
//! resolved timeout of the request. The learned spread IS the grace period: a
//! rock steady provider (2-4s, σ≈0.3s) is cut ~1-2s past its norm when it
//! hangs, while a jittery one earns 15s+ of slack automatically. Stuck
//! attempts never produced bytes, so they are not part of the distribution.
//!
//! Design rules ("learn, never break"):
//! - Only successful requests (2xx-3xx) feed the buckets, so a hanging
//!   provider is cut at its learned norm instead of the global timeout.
//! - Buckets are logarithmic in prompt tokens: a 300k-token cold request is
//!   legitimately slow (15s+) while a cached one is fast (2s).
//! - The budget only ever shortens the resolved timeout (it is clamped to the
//!   ceiling the caller passes: defaults + model overrides + provider
//!   floors). With fewer than [`MIN_BUCKET_SAMPLES`] successful samples in a
//!   bucket the predictor widens the bucket; with no data at all it returns
//!   `None` and the request keeps the plain resolved timeout.
//! - Snapshots are cached per target for [`SNAPSHOT_TTL`] so steady-state
//!   dispatch adds zero queries.
//!
//! Root-cause corrections for the known failure modes:
//! 1. **Survivorship bias / anti-starvation**: TTFT cuts are recorded on
//!    `usage` (`upstream timeout in phase ttft after Nms`; legacy rows say
//!    `headers`). Every budget
//!    is raised to `censored_max * 1.5`, so a target cut unjustly relaxes
//!    geometrically (8s → 12s → 18s...) until it covers its real regime or
//!    the relax reaches the resolved timeout, where the predictor silently
//!    turns itself off for that target and the global timeout takes over.
//! 2. **Bimodal populations**: `max(mean + 3σ, weighted p99)` guarantees no
//!    historically successful TTFT of the bucket sits above the budget
//!    (except the rarest 1%), so a legitimate cold mode (e.g. 5% of requests
//!    at 30s) is never cut on a warm-dominated target.
//! 3. **Unbounded cache**: the per-target snapshot map is capped
//!    (`cache_cap`); on overflow the oldest snapshots are evicted.
//! 4. **Hot-path lock contention**: with a DB pool configured, snapshots
//!    refresh in a background task (never under the shared pipeline `conn`
//!    mutex) and stale snapshots keep serving for `4 * SNAPSHOT_TTL`. The
//!    synchronous path only runs when no pool exists (tests / bare setups).
//! 5. **Token estimation drift**: buckets are built over rows whose prompt
//!    tokens were estimated by the same tracker estimator dispatch uses
//!    (`prompt_tokens_estimated`), when enough exist; real-upstream token
//!    rows (different tokenizers) only mix in when that population is thin.
//! 6. **Slow regime erosion**: samples are weighted by recency (48h
//!    half-life), so a provider degraded this week dominates the statistics
//!    within a couple of days instead of being diluted by up to 7 days of
//!    history.
//! 7. **Antigravity coverage**: no provider is excluded. Antigravity's
//!    resolved timeout already carries the 120s thinking floor, which acts
//!    as the natural ceiling; a target whose budget reaches it is simply not
//!    predicted (same behaviour as today), while fast non-thinking
//!    antigravity targets are cut promptly when they hang.
//! 8. **Retry amplification**: on a TTFT cut the executor retry loop raises
//!    the budget (`* 1.5`, capped at the resolved ceiling) before rotating,
//!    so a transient stall gets a genuinely larger second chance instead of
//!    re-cutting at the same budget. See `note_censored` and the executor
//!    retry loop.

//!
//! ## Hierarchical budgets: target-wide and per session
//!
//! One target-wide norm is blind to the regime a *session* is in: an agent
//! with a warm prompt cache answers in 2s, and after five idle minutes the
//! same conversation cold-starts in 20s. Two levels solve that:
//!
//! - **Global** (`provider`, `model_row`), TTL [`GLOBAL_TTL`] = 1h: the
//!   general norm of the target, built from every row (sessioned or not).
//! - **Session** (`provider`, `model_row`, `session_hash`), TTL
//!   [`SESSION_TTL`] = 5min: learns the regime of one conversation over its
//!   last hour and forgets it after five idle minutes, exactly when the
//!   provider cache is likely gone. The next request of that session then
//!   starts from the global norm instead of inheriting a warm-cache budget
//!   that would cut every cold start.
//!
//! Resolution order: session snapshot, then global, then `None`. Both levels
//! keep their own `censored_max`; the executor notes a cut on the session
//! entry and on the target entry (a headers stall is a symptom of the
//! target), while the learned statistics of each level stay isolated.
//!
//! ## Learned idle-chunk budgets (streaming)
//!
//! The same engine learns how long one streaming target legitimately goes
//! silent between content chunks: `chunk_budget` pools the `max_chunk_gap_ms`
//! telemetry of completed streams (see `ChunkGapTracker` in
//! `openproxy-adapters`) into one distribution per level and applies the same
//! `max(mean + 3σ, p99) + margin` formula with a higher floor. `None` keeps
//! the resolved `idle_chunk` timeout untouched.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use openproxy_types::ids::ModelRowId;
use parking_lot::Mutex;
use rusqlite::Connection;

pub(crate) mod chunk_budget;
mod loaders;
mod stats;

#[cfg(test)]
#[path = "latency_budget/testutil.rs"]
mod testutil;

#[cfg(test)]
#[path = "latency_budget/stats_tests.rs"]
mod stats_tests;

#[cfg(test)]
#[path = "latency_budget/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "latency_budget/chunk_tests.rs"]
mod chunk_tests;

use loaders::{load_censored_max, load_success_rows};
use stats::{BUCKET_COUNT, BucketStats, budget_from_stats, build_buckets};

/// Global (target-wide) snapshot lifetime: 1 hour.
const GLOBAL_TTL: Duration = Duration::from_secs(3_600);
/// Session snapshot lifetime: 5 minutes. After five idle minutes the
/// provider cache is likely cold again, so the learned session regime is
/// dropped and the next request re-learns (or falls back to the global norm).
const SESSION_TTL: Duration = Duration::from_secs(300);
/// Serve-stale horizon as a multiple of the level TTL: an expired snapshot
/// keeps answering while its background refresh is in flight (correction 4).
const STALE_SERVE_MULT: u32 = 4;
/// Session recency half-life (5 minutes): inside the window the last couple
/// of minutes dominate the statistics.
const SESSION_HALF_LIFE_H: f64 = 5.0 / 60.0;
/// A cut relaxes the next budget by this factor (corrections 1 and 8).
const CENSORED_RELAX_MULT: f64 = 1.5;
/// Recency weighting half-life (correction 6): a sample from 48h ago
/// weighs half of a fresh one.
const RECENCY_HALF_LIFE_H: f64 = 48.0;

/// Learned statistics for one cache key: the target (global level) or one
/// session of that target.
#[derive(Debug)]
pub(crate) struct TargetStats {
    loaded_at: Instant,
    /// Per token-bucket statistics; `None` = too few samples.
    buckets: [Option<BucketStats>; BUCKET_COUNT],
    /// Largest `headers`-phase timeout recorded for this target within the
    /// window (ms). Drives the anti-starvation relax (correction 1).
    /// Atomic: the executor notes cuts while readers hold the Arc.
    censored_max_ms: AtomicU64,
    /// Pooled statistics of survived inter-chunk gaps (idle-chunk norm);
    /// `None` until enough gap telemetry exists. Gaps are not bucketed by
    /// prompt size: chunk pacing is an upstream property, not a request-size
    /// property (see `chunk_budget`).
    gap_stats: Option<BucketStats>,
    /// Largest `idle_chunk`-phase timeout recorded within the window (ms);
    /// anti-starvation relax for the learned gap budget.
    chunk_censored_max_ms: AtomicU64,
}

/// Cache key of a learned snapshot. `session_hash == None` is the global
/// level (the target norm); `Some(_)` is one session of that target.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct BudgetKey {
    provider: String,
    model_row_id: i64,
    session_hash: Option<u64>,
}

impl BudgetKey {
    fn global(provider: &str, model_row_id: i64) -> Self {
        Self {
            provider: provider.to_string(),
            model_row_id,
            session_hash: None,
        }
    }

    fn session(provider: &str, model_row_id: i64, session_hash: u64) -> Self {
        Self {
            provider: provider.to_string(),
            model_row_id,
            session_hash: Some(session_hash),
        }
    }

    fn is_session(&self) -> bool {
        self.session_hash.is_some()
    }
}

/// Adaptive TTFT budget predictor, one snapshot per (provider, model_row).
/// Cheap per request: after the first load the answer is a cache read;
/// refreshes run in the background from the DB pool (correction 4).
pub struct LatencyBudget {
    cache: Mutex<HashMap<BudgetKey, Arc<TargetStats>>>,
    /// Keys with a background refresh in flight (prevents stampedes).
    inflight: Mutex<HashSet<BudgetKey>>,
    /// Present in production: background refreshes use the pool instead of
    /// the shared pipeline connection.
    pool: Option<openproxy_db::DbPool>,
    /// Snapshot map cap (correction 3). Configurable for tests.
    cache_cap: usize,
    /// How many oldest snapshots to evict when the cap is exceeded.
    cache_evict: usize,
    /// Global-level snapshot lifetime (1h in production).
    global_ttl: Duration,
    /// Session-level snapshot lifetime (5min in production).
    session_ttl: Duration,
}

impl Default for LatencyBudget {
    fn default() -> Self {
        Self::new()
    }
}

impl LatencyBudget {
    /// Sync-only predictor (no pool): refreshes happen inline on whatever
    /// connection the caller provides. Used by tests and bare setups.
    pub fn new() -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
            inflight: Mutex::new(HashSet::new()),
            pool: None,
            cache_cap: 10_000,
            cache_evict: 1_000,
            global_ttl: GLOBAL_TTL,
            session_ttl: SESSION_TTL,
        }
    }

    /// Production predictor: background refreshes run against the pool.
    pub fn with_pool(pool: openproxy_db::DbPool) -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
            inflight: Mutex::new(HashSet::new()),
            pool: Some(pool),
            cache_cap: 10_000,
            cache_evict: 1_000,
            global_ttl: GLOBAL_TTL,
            session_ttl: SESSION_TTL,
        }
    }

    /// Pool-mode when a pool exists, sync-mode otherwise (tests / bare
    /// setups). The pool owns the background refresh path (correction 4).
    pub(crate) fn with_pool_if_present(db_pool: Option<&openproxy_db::DbPool>) -> Arc<Self> {
        Arc::new(match db_pool {
            Some(pool) => Self::with_pool(pool.clone()),
            None => Self::new(),
        })
    }

    /// Adaptive TTFT budget in milliseconds, or `None` when the predictor has
    /// no trustworthy data for this target (the caller keeps the resolved
    /// timeout untouched). `ceiling_ms` is the already-resolved TTFT timeout
    /// (defaults + model overrides + provider floors): the predictor can only
    /// shorten it, never extend it.
    ///
    /// Takes `&Arc<Self>` because the pool-mode refresh spawns a background
    /// task tied to this instance's cache. `sync_conn` is only locked in sync
    /// mode (no DB pool): in production the hot path never touches the shared
    /// pipeline connection (correction 4).
    /// `session_hash` selects the session level first; `None` (no session
    /// identity) or a session without fresh data falls back to the global
    /// level of the target.
    pub fn ttft_budget_ms(
        self: &Arc<Self>,
        provider: &str,
        model_row_id: ModelRowId,
        session_hash: Option<u64>,
        est_prompt_tokens: u32,
        ceiling_ms: u64,
        sync_conn: Option<&parking_lot::Mutex<Connection>>,
    ) -> Option<u64> {
        if ceiling_ms == 0 {
            // Nothing to shorten; a zero ceiling is not a prediction.
            return None;
        }
        // Session level first: it knows the regime this conversation is in
        // right now (warm cache vs cold start). The global level answers
        // when the session has no fresh data of its own.
        if let Some(hash) = session_hash {
            let key = BudgetKey::session(provider, model_row_id.0, hash);
            if let Some(stats) = self.snapshot(&key, sync_conn)
                && let Some(budget) = budget_from_stats(&stats, est_prompt_tokens, ceiling_ms)
            {
                return Some(budget);
            }
        }
        let key = BudgetKey::global(provider, model_row_id.0);
        let stats = self.snapshot(&key, sync_conn)?;
        budget_from_stats(&stats, est_prompt_tokens, ceiling_ms)
    }

    /// Record a TTFT cut seen by the executor so the very next budget for
    /// this target is relaxed without waiting for the DB row or a refresh
    /// (corrections 1 and 8).
    pub(crate) fn note_censored(
        &self,
        provider: &str,
        model_row_id: ModelRowId,
        session_hash: Option<u64>,
        waited_ms: u64,
    ) {
        let cache = self.cache.lock();
        if let Some(hash) = session_hash
            && let Some(stats) = cache.get(&BudgetKey::session(provider, model_row_id.0, hash))
        {
            stats
                .censored_max_ms
                .fetch_max(waited_ms, Ordering::Relaxed);
        }
        if let Some(stats) = cache.get(&BudgetKey::global(provider, model_row_id.0)) {
            stats
                .censored_max_ms
                .fetch_max(waited_ms, Ordering::Relaxed);
        }
    }

    fn ttl_for(&self, key: &BudgetKey) -> Duration {
        if key.is_session() {
            self.session_ttl
        } else {
            self.global_ttl
        }
    }

    /// Cache lookup with background refresh and serve-stale (correction 4).
    fn snapshot(
        self: &Arc<Self>,
        key: &BudgetKey,
        sync_conn: Option<&parking_lot::Mutex<Connection>>,
    ) -> Option<Arc<TargetStats>> {
        let ttl = self.ttl_for(key);
        let existing = self.cache.lock().get(key).cloned();
        if let Some(stats) = &existing
            && stats.loaded_at.elapsed() < ttl
        {
            return Some(Arc::clone(stats));
        }
        if self.pool.is_some() {
            self.kick_background_refresh(key.clone());
            // Serve stale while the refresh lands; past the stale horizon
            // answer without prediction (the refresh will repopulate soon).
            let stale_horizon = ttl * STALE_SERVE_MULT;
            return existing.filter(|s| s.loaded_at.elapsed() < stale_horizon);
        }
        // Sync mode (no pool): refresh inline on the caller's connection.
        let guard = sync_conn?.lock();
        self.load_sync(&guard, key.clone())
    }

    /// Schedules a background refresh from the pool if none is in flight.
    /// Falls back to the inline sync path when no tokio runtime is available.
    fn kick_background_refresh(self: &Arc<Self>, key: BudgetKey) {
        {
            let mut inflight = self.inflight.lock();
            if !inflight.insert(key.clone()) {
                return;
            }
        }
        // Outside a runtime (rare embeds), degrade to sync on next call.
        if tokio::runtime::Handle::try_current().is_err() {
            self.inflight.lock().remove(&key);
            return;
        }
        let this = Arc::clone(self);
        let pool = self.pool.clone();
        let key_for_task = key.clone();
        tokio::spawn(async move {
            let loaded = match pool {
                Some(pool) => {
                    let this_for_task = Arc::clone(&this);
                    tokio::task::spawn_blocking(move || {
                        pool.open_connection()
                            .ok()
                            .and_then(|conn| this_for_task.load_sync(&conn, key_for_task.clone()))
                    })
                    .await
                    .ok()
                    .flatten()
                }
                None => None,
            };
            if let Some(stats) = loaded {
                this.cache.lock().insert(key.clone(), stats);
            }
            this.inflight.lock().remove(&key);
        });
    }

    /// Loads (or reloads) the snapshot for one target synchronously.
    /// Returns the new stats, also landing them in the cache. Called inline
    /// (sync mode / tests) or from the background refresh task.
    pub(crate) fn load_sync(&self, conn: &Connection, key: BudgetKey) -> Option<Arc<TargetStats>> {
        let successes = load_success_rows(conn, &key)?;
        let censored_max_ms = load_censored_max(conn, &key);
        let half_life_h = if key.is_session() {
            SESSION_HALF_LIFE_H
        } else {
            RECENCY_HALF_LIFE_H
        };
        let buckets = build_buckets(&successes, half_life_h);
        let gap_stats = chunk_budget::load_gap_stats(conn, &key, half_life_h);
        let chunk_censored_max_ms = chunk_budget::load_chunk_censored_max(conn, &key);
        let stats = Arc::new(TargetStats {
            loaded_at: Instant::now(),
            buckets,
            censored_max_ms: AtomicU64::new(censored_max_ms),
            gap_stats,
            chunk_censored_max_ms: AtomicU64::new(chunk_censored_max_ms),
        });
        self.insert_and_purge(key, Arc::clone(&stats));
        Some(stats)
    }

    /// Warms the cache for one target from the given connection. Test and
    /// embedding helper; production relies on the background refresh.
    #[cfg(test)]
    pub(crate) fn warm_target(&self, conn: &Connection, provider: &str, model_row_id: i64) {
        self.load_sync(conn, BudgetKey::global(provider, model_row_id));
    }

    #[cfg(test)]
    pub(crate) fn warm_session(
        &self,
        conn: &Connection,
        provider: &str,
        model_row_id: i64,
        session_hash: u64,
    ) {
        self.load_sync(
            conn,
            BudgetKey::session(provider, model_row_id, session_hash),
        );
    }

    fn insert_and_purge(&self, key: BudgetKey, stats: Arc<TargetStats>) {
        let mut cache = self.cache.lock();
        // Session snapshots are ephemeral by design: drop the ones past
        // their stale horizon so conversations never accumulate until the
        // LRU cap and evict a live target norm.
        let stale_horizon = self.session_ttl * STALE_SERVE_MULT;
        cache.retain(|k, v| !k.is_session() || v.loaded_at.elapsed() < stale_horizon);
        if cache.len() >= self.cache_cap && !cache.contains_key(&key) {
            let mut oldest: Vec<(BudgetKey, Instant)> = cache
                .iter()
                .map(|(k, v)| (k.clone(), v.loaded_at))
                .collect();
            oldest.sort_by_key(|(_, at)| *at);
            for (k, _) in oldest.into_iter().take(self.cache_evict) {
                cache.remove(&k);
            }
        }
        cache.insert(key, stats);
    }
}
