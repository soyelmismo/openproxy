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
//!    `usage` (`upstream timeout in phase headers after Nms`). Every budget
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

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use openproxy_types::ids::ModelRowId;
use parking_lot::Mutex;
use rusqlite::Connection;

/// How many recent successful rows per target feed the snapshot.
const SAMPLE_LIMIT: i64 = 400;
/// History window for successes and cuts alike.
const HISTORY_WINDOW_DAYS: i64 = 7;
/// Minimum samples a bucket needs before its statistics are trusted.
const MIN_BUCKET_SAMPLES: usize = 6;
/// The budget is `max(mean + SIGMA_MULT*σ, p99) + MARGIN_MS`: the learned
/// standard deviation IS the grace period, never a hard-coded margin.
const SIGMA_MULT: f64 = 3.0;
/// Small fixed margin on top of the statistical bound.
const MARGIN_MS: u64 = 1_000;
/// Never cut a request earlier than this, no matter how fast the target is.
const FLOOR_MS: u64 = 5_000;
/// Global (target-wide) snapshot lifetime: 1 hour.
const GLOBAL_TTL: Duration = Duration::from_secs(3_600);
/// Session snapshot lifetime: 5 minutes. After five idle minutes the
/// provider cache is likely cold again, so the learned session regime is
/// dropped and the next request re-learns (or falls back to the global norm).
const SESSION_TTL: Duration = Duration::from_secs(300);
/// Serve-stale horizon as a multiple of the level TTL: an expired snapshot
/// keeps answering while its background refresh is in flight (correction 4).
const STALE_SERVE_MULT: u32 = 4;
/// Session history window: only the last ten minutes of that session feed
/// it, so a cache that expired six minutes ago stops paying for the warm
/// regime it had before.
const SESSION_WINDOW_MINUTES: i64 = 10;
/// Session recency half-life (5 minutes): inside the window the last couple
/// of minutes dominate the statistics.
const SESSION_HALF_LIFE_H: f64 = 5.0 / 60.0;
/// Cut rows (`headers`-phase timeouts) considered for the relax.
const CENSORED_LIMIT: i64 = 100;
/// A cut relaxes the next budget by this factor (corrections 1 and 8).
const CENSORED_RELAX_MULT: f64 = 1.5;
/// Recency weighting half-life (correction 6): a sample from 48h ago
/// weighs half of a fresh one.
const RECENCY_HALF_LIFE_H: f64 = 48.0;

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

#[derive(Debug, Clone, Copy)]
struct BucketStats {
    mean: f64,
    sd: f64,
    p99: f64,
}

impl BucketStats {
    /// Statistical budget of the bucket: the spread bound and the empirical
    /// tail bound, whichever is larger (correction 2), plus the fixed
    /// margin, floored.
    fn budget_ms(&self) -> u64 {
        let spread_bound = self.mean + SIGMA_MULT * self.sd;
        let stat = spread_bound.max(self.p99) + MARGIN_MS as f64;
        (stat.max(FLOOR_MS as f64)) as u64
    }
}

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

struct SuccessRow {
    ttft_ms: u64,
    prompt_tokens: u64,
    estimated: bool,
    age_hours: f64,
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
    pub(crate) fn with_pool_if_present(db_pool: &Option<openproxy_db::DbPool>) -> Arc<Self> {
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
            if let Some(stats) = self.snapshot(&key, sync_conn) {
                if let Some(budget) = budget_from_stats(&stats, est_prompt_tokens, ceiling_ms) {
                    return Some(budget);
                }
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
        if let Some(hash) = session_hash {
            if let Some(stats) = cache.get(&BudgetKey::session(provider, model_row_id.0, hash)) {
                stats.censored_max_ms.fetch_max(waited_ms, Ordering::Relaxed);
            }
        }
        if let Some(stats) = cache.get(&BudgetKey::global(provider, model_row_id.0)) {
            stats.censored_max_ms.fetch_max(waited_ms, Ordering::Relaxed);
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
        if let Some(stats) = &existing {
            if stats.loaded_at.elapsed() < ttl {
                return Some(Arc::clone(stats));
            }
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
        let stats = Arc::new(TargetStats {
            loaded_at: Instant::now(),
            buckets,
            censored_max_ms: AtomicU64::new(censored_max_ms),
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
        self.load_sync(conn, BudgetKey::session(provider, model_row_id, session_hash));
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

/// Pull recent successful TTFT samples for one key from `usage`. The global
/// level aggregates every row of the target (sessioned or not); a session
/// level reads only its own rows, over the shorter session window.
fn load_success_rows(conn: &Connection, key: &BudgetKey) -> Option<Vec<SuccessRow>> {
    let (window, session_filter) = match key.session_hash {
        Some(_) => (format!("-{SESSION_WINDOW_MINUTES} minutes"), "AND session_hash = ?5"),
        None => (format!("-{HISTORY_WINDOW_DAYS} days"), ""),
    };
    let sql = format!(
        "SELECT ttft_ms, prompt_tokens, prompt_tokens_estimated, \
                (julianday('now') - julianday(created_at)) * 24.0 \
         FROM usage \
         WHERE provider_id = ?1 AND model_row_id = ?2 \
           AND status_code >= 200 AND status_code < 400 \
           AND ttft_ms IS NOT NULL AND ttft_ms > 0 \
           AND created_at >= datetime('now', ?3) \
           {session_filter} \
         ORDER BY id DESC LIMIT ?4"
    );
    let mut stmt = conn.prepare(&sql).ok()?;
    let out: Vec<SuccessRow> = match key.session_hash {
        Some(hash) => {
            let rows = stmt
                .query_map(
                    rusqlite::params![
                        key.provider,
                        key.model_row_id,
                        window,
                        SAMPLE_LIMIT,
                        hash as i64
                    ],
                    map_success_row,
                )
                .ok()?;
            rows.filter_map(|r| r.ok()).collect()
        }
        None => {
            let rows = stmt
                .query_map(
                    rusqlite::params![key.provider, key.model_row_id, window, SAMPLE_LIMIT],
                    map_success_row,
                )
                .ok()?;
            rows.filter_map(|r| r.ok()).collect()
        }
    };
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn map_success_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SuccessRow> {
    Ok(SuccessRow {
        ttft_ms: (row.get::<_, Option<f64>>(0)?.unwrap_or(0.0) as u64).max(1),
        prompt_tokens: row.get::<_, Option<i64>>(1)?.unwrap_or(0).max(0) as u64,
        estimated: row.get::<_, Option<i64>>(2)?.unwrap_or(0) != 0,
        age_hours: row.get::<_, Option<f64>>(3)?.unwrap_or(0.0).max(0.0),
    })
}

/// Largest `headers`-phase timeout for this target within the window. Rows
/// look like `upstream timeout in phase headers after 8274ms` (correction 1).
fn load_censored_max(conn: &Connection, key: &BudgetKey) -> u64 {
    let (window, session_filter) = match key.session_hash {
        Some(_) => (format!("-{SESSION_WINDOW_MINUTES} minutes"), "AND session_hash = ?5"),
        None => (format!("-{HISTORY_WINDOW_DAYS} days"), ""),
    };
    let sql = format!(
        "SELECT error_msg FROM usage \
         WHERE provider_id = ?1 AND model_row_id = ?2 \
           AND error_msg LIKE '%in phase headers after %ms%' \
           AND created_at >= datetime('now', ?3) \
           {session_filter} \
         ORDER BY id DESC LIMIT ?4"
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return 0;
    };
    let mut max_ms: u64 = 0;
    match key.session_hash {
        Some(hash) => {
            let Ok(rows) = stmt.query_map(
                rusqlite::params![
                    key.provider,
                    key.model_row_id,
                    window,
                    CENSORED_LIMIT,
                    hash as i64
                ],
                |r| r.get::<_, Option<String>>(0),
            ) else {
                return 0;
            };
            for msg in rows.filter_map(|r| r.ok()).flatten() {
                if let Some(ms) = parse_headers_timeout_ms(&msg) {
                    max_ms = max_ms.max(ms);
                }
            }
        }
        None => {
            let Ok(rows) = stmt.query_map(
                rusqlite::params![key.provider, key.model_row_id, window, CENSORED_LIMIT],
                |r| r.get::<_, Option<String>>(0),
            ) else {
                return 0;
            };
            for msg in rows.filter_map(|r| r.ok()).flatten() {
                if let Some(ms) = parse_headers_timeout_ms(&msg) {
                    max_ms = max_ms.max(ms);
                }
            }
        }
    }
    max_ms
}

/// Resolves the learned budget of one level for the request's token bucket,
/// or `None` when the level has no trustworthy data or its budget already
/// reaches the caller's ceiling (nothing left to shorten).
fn budget_from_stats(
    stats: &TargetStats,
    est_prompt_tokens: u32,
    ceiling_ms: u64,
) -> Option<u64> {
    // Widen upward: exact bucket first, then the next populated ones.
    let start = tokens_bucket(est_prompt_tokens);
    let mut budget_ms: Option<u64> = None;
    for b in start..BUCKET_COUNT {
        if let Some(bs) = stats.buckets[b] {
            budget_ms = Some(bs.budget_ms());
            break;
        }
    }
    let mut budget_ms = budget_ms?;
    // Anti-starvation relax (correction 1): every cut raises the floor of
    // the next budget. When the relax reaches the resolved timeout the
    // predictor turns itself off for this level.
    let censored = stats.censored_max_ms.load(Ordering::Relaxed);
    if censored > 0 {
        let relaxed = (censored as f64 * CENSORED_RELAX_MULT) as u64;
        budget_ms = budget_ms.max(relaxed);
    }
    if budget_ms >= ceiling_ms {
        return None;
    }
    Some(budget_ms)
}

fn parse_headers_timeout_ms(error_msg: &str) -> Option<u64> {
    let marker = "in phase headers after ";
    let idx = error_msg.find(marker)?;
    let rest = &error_msg[idx + marker.len()..];
    let end = rest.find("ms")?;
    rest[..end].trim().parse::<u64>().ok()
}

/// Builds per-bucket statistics with recency weighting (correction 6) and
/// the estimated-token preference (correction 5).
fn build_buckets(rows: &[SuccessRow], half_life_h: f64) -> [Option<BucketStats>; BUCKET_COUNT] {
    // Prefer rows whose prompt tokens were estimated the same way dispatch
    // estimates them (comparable buckets). Fall back to all rows only when
    // the estimated population is too thin to trust a bucket.
    let population: Vec<&SuccessRow> = {
        let estimated: Vec<&SuccessRow> = rows.iter().filter(|r| r.estimated).collect();
        if estimated.len() >= MIN_BUCKET_SAMPLES {
            estimated
        } else {
            rows.iter().collect()
        }
    };
    let mut buckets: [Option<BucketStats>; BUCKET_COUNT] = Default::default();
    for b in 0..BUCKET_COUNT {
        let members: Vec<&SuccessRow> = population
            .iter()
            .copied()
            .filter(|r| tokens_bucket(r.prompt_tokens.max(1) as u32) == b)
            .collect();
        let n = members.len();
        if n < MIN_BUCKET_SAMPLES {
            continue;
        }
        // Recency weights (correction 6): 48h half-life for the target
        // norm, 30min for a session (its regime must dominate fast).
        let weights: Vec<f64> = members
            .iter()
            .map(|r| 0.5f64.powf(r.age_hours / half_life_h))
            .collect();
        let wsum: f64 = weights.iter().sum();
        if wsum <= 0.0 {
            continue;
        }
        let mean: f64 = members
            .iter()
            .zip(&weights)
            .map(|(r, w)| r.ttft_ms as f64 * w)
            .sum::<f64>()
            / wsum;
        let var: f64 = members
            .iter()
            .zip(&weights)
            .map(|(r, w)| {
                let d = r.ttft_ms as f64 - mean;
                d * d * w
            })
            .sum::<f64>()
            / wsum;
        // Sample correction: the weighted population variance underestimates
        // the spread by roughly n/(n-1) for near-uniform weights.
        let sd = (var * (n as f64) / ((n - 1) as f64).max(1.0)).sqrt();
        // Weighted empirical p99 (correction 2): sort by ttft, walk the
        // cumulative weight to the 99% quantile.
        let mut order: Vec<(u64, f64)> = members
            .iter()
            .zip(&weights)
            .map(|(r, w)| (r.ttft_ms, *w))
            .collect();
        order.sort_by_key(|(ttft, _)| *ttft);
        let target_cum = 0.99 * wsum;
        let mut cum = 0.0;
        let mut p99 = order.last().map(|(t, _)| *t as f64).unwrap_or(0.0);
        for (ttft, w) in &order {
            cum += w;
            if cum >= target_cum {
                p99 = *ttft as f64;
                break;
            }
        }
        buckets[b] = Some(BucketStats { mean, sd, p99 });
    }
    buckets
}

#[cfg(test)]
mod tests {
    use super::*;
    use openproxy_types::ids::ModelRowId;

    /// Minimal usage schema mirroring the production columns the query
    /// touches, plus prompt_tokens_estimated (correction 5) and error_msg.
    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE usage (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                request_id TEXT, trace_id TEXT, attempt INTEGER DEFAULT 1,
                provider_id TEXT NOT NULL, account_id INTEGER,
                combo_id INTEGER, model_row_id INTEGER,
                upstream_model_id TEXT NOT NULL DEFAULT '', combo_target_id INTEGER,
                prompt_tokens INTEGER, prompt_tokens_estimated INTEGER,
                session_hash INTEGER,
                completion_tokens INTEGER, cached_tokens INTEGER,
                cost_usd REAL, status_code INTEGER NOT NULL,
                error_msg TEXT, created_at TEXT NOT NULL DEFAULT (datetime('now')),
                connect_ms INTEGER, ttft_ms INTEGER, total_ms INTEGER
             );",
        )
        .unwrap();
        conn
    }

    /// Shared mutex connection: the sync-mode predictor locks it itself.
    fn shared_conn(conn: Connection) -> Arc<parking_lot::Mutex<Connection>> {
        Arc::new(parking_lot::Mutex::new(conn))
    }

    fn insert(c: &Connection, provider: &str, model_row: i64, ttft: i64, prompt: i64) {
        insert_at(c, provider, model_row, ttft, prompt, "datetime('now')");
    }

    fn insert_est(
        c: &Connection,
        provider: &str,
        model_row: i64,
        ttft: i64,
        prompt: i64,
        estimated: i64,
    ) {
        c.execute(
            "INSERT INTO usage (provider_id, model_row_id, upstream_model_id, status_code, ttft_ms, prompt_tokens, prompt_tokens_estimated, created_at) \
             VALUES (?1, ?2, 'm', 200, ?3, ?4, ?5, datetime('now'))",
            rusqlite::params![provider, model_row, ttft, prompt, estimated],
        )
        .unwrap();
    }

    fn insert_at(
        c: &Connection,
        provider: &str,
        model_row: i64,
        ttft: i64,
        prompt: i64,
        created_at: &str,
    ) {
        c.execute(
            &format!(
                "INSERT INTO usage (provider_id, model_row_id, upstream_model_id, status_code, ttft_ms, prompt_tokens, created_at) \
                 VALUES (?1, ?2, 'm', 200, ?3, ?4, {created_at})"
            ),
            rusqlite::params![provider, model_row, ttft, prompt],
        )
        .unwrap();
    }

    fn insert_sess(
        c: &Connection,
        provider: &str,
        model_row: i64,
        session_hash: i64,
        ttft: i64,
        prompt: i64,
    ) {
        insert_sess_at(
            c,
            provider,
            model_row,
            session_hash,
            ttft,
            prompt,
            "datetime('now')",
        );
    }

    fn insert_sess_at(
        c: &Connection,
        provider: &str,
        model_row: i64,
        session_hash: i64,
        ttft: i64,
        prompt: i64,
        created_at: &str,
    ) {
        c.execute(
            &format!(
                "INSERT INTO usage (provider_id, model_row_id, upstream_model_id, status_code, \
                                    ttft_ms, prompt_tokens, session_hash, created_at) \
                 VALUES (?1, ?2, 'm', 200, ?3, ?4, ?5, {created_at})"
            ),
            rusqlite::params![provider, model_row, ttft, prompt, session_hash],
        )
        .unwrap();
    }

    /// Inserts a headers-phase timeout row exactly as the usage tracker
    /// records it (correction 1): status 0, NULL ttft, parseable error_msg.
    fn insert_cut(c: &Connection, provider: &str, model_row: i64, waited_ms: u64) {
        c.execute(
            "INSERT INTO usage (provider_id, model_row_id, upstream_model_id, status_code, ttft_ms, prompt_tokens, error_msg, created_at) \
             VALUES (?1, ?2, 'm', 0, NULL, 5000, ?3, datetime('now'))",
            rusqlite::params![
                provider,
                model_row,
                format!("upstream timeout in phase headers after {waited_ms}ms")
            ],
        )
        .unwrap();
    }

    fn budget_of(
        b: &Arc<LatencyBudget>,
        c: &parking_lot::Mutex<Connection>,
        provider: &str,
        row: i64,
        est: u32,
        ceiling: u64,
    ) -> Option<u64> {
        budget_of_sess(b, c, provider, row, None, est, ceiling)
    }

    fn budget_of_sess(
        b: &Arc<LatencyBudget>,
        c: &parking_lot::Mutex<Connection>,
        provider: &str,
        row: i64,
        session_hash: Option<u64>,
        est: u32,
        ceiling: u64,
    ) -> Option<u64> {
        b.ttft_budget_ms(provider, ModelRowId(row), session_hash, est, ceiling, Some(c))
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
        let b = budget_of(&budget, &mconn, "fast", 7, 5_000, 60_000)
            .expect("budget from 30 samples");
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
        assert!(b >= 20_000 && b <= 23_000, "cold tail protected: {b}");
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
        let b = budget_of(&budget, &mconn, "bimodal", 6, 100_000, 60_000)
            .expect("budget");
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
        let b = budget_of(&budget, &mconn, "w", 9, 5_000, 60_000)
            .expect("widened bucket has samples");
        // mean 13444, sigma 5081 -> 13444 + 15243 + 1000 = 29687 (p99 22k
        // loses to the spread bound).
        assert!(b >= 29_000 && b <= 30_500, "widened budget: {b}");

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
        let b = budget_of(&budget, &mconn, "old", 1, 5_000, 60_000)
            .expect("in-window samples exist");
        assert_eq!(b, 5_000, "only the recent fast samples feed the budget: {b}");
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
        let b = budget_of(&budget, &mconn, "n", 3, 500, 60_000)
            .expect("valid bucket-0 samples");
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

    #[test]
    fn cache_prevents_requery_within_ttl() {
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
        let b = budget_of(&budget, &mconn, "e", 12, 2_000, 60_000)
            .expect("exactly-min bucket trusted");
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
            assert!(b >= 5_000 && b <= 8_000, "target {target} budget {b}");
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
        let b2 = budget_of(&budget, &mconn, "agy", 6, 5_000, 120_000)
            .expect("agy alias gets predictions");
        assert_eq!(b2, 5_000);
    }

    // ------------------------------------------------------------------
    // Correction 4: pool-mode background refresh + serve-stale.
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn pool_mode_refreshes_in_background() {
        let pool = openproxy_db::DbPool::test_pool_with_prefix("openproxy-lb-pool-refresh")
            .expect("pool");
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
        let sess =
            budget_of_sess(&budget, &mconn, "mix", 1, Some(12345), 5_000, 60_000).unwrap();
        assert_eq!(sess, 21_000, "session budget reflects its cold regime: {sess}");
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
        let sess_now =
            budget_of_sess(&budget, &mconn, "ttl", 3, Some(54321), 5_000, 60_000).unwrap();
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
        let sess_later =
            budget_of_sess(&budget, &mconn, "ttl", 3, Some(54321), 5_000, 60_000).unwrap();
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
        let sess_a =
            budget_of_sess(&budget, &mconn, "iso", 4, Some(111), 5_000, 60_000).unwrap();
        let sess_b =
            budget_of_sess(&budget, &mconn, "iso", 4, Some(222), 5_000, 60_000).unwrap();
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
        assert_eq!(sess, 5_000, "stale regime outside the window is forgotten: {sess}");
    }

}
