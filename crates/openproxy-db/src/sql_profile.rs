//! Opt-in process-wide SQL statement counters and lock duration profiler.
//! Never enabled in normal builds.
//! Counts executed statements, execution durations and lock acquisition wait times,
//! not guessed query counts.

#[cfg(feature = "sql-profile")]
use rusqlite::trace::{TraceEvent, TraceEventCodes};
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static ACTIVE: AtomicBool = AtomicBool::new(false);
static STATEMENT_COUNTS: Mutex<BTreeMap<String, u64>> = Mutex::new(BTreeMap::new());
static SQL_PROFILES: Mutex<BTreeMap<String, DurationHistogram>> = Mutex::new(BTreeMap::new());
static LOCK_PROFILES: Mutex<BTreeMap<String, DurationHistogram>> = Mutex::new(BTreeMap::new());

const MAX_SQL_ENTRIES: usize = 512;
const NUM_LOG2_BUCKETS: usize = 32;

/// Lock kinds measured during DbPool acquisition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockKind {
    Reader,
    Writer,
}

impl LockKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Reader => "reader",
            Self::Writer => "writer",
        }
    }
}

/// Duration histogram using power-of-two (log2) buckets for nanoseconds.
/// Also stores total duration and count, enabling exact totals and bounded percentile estimates.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct DurationHistogram {
    pub count: u64,
    pub total_ns: u64,
    pub min_ns: Option<u64>,
    pub max_ns: Option<u64>,
    /// Buckets where index `i` (0..=31) covers `[2^i, 2^(i+1) - 1]` ns, with bucket 0 covering `[0, 1]` ns.
    pub buckets: [u64; NUM_LOG2_BUCKETS],
}

impl DurationHistogram {
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    fn bucket_index(ns: u64) -> usize {
        if ns == 0 {
            0
        } else {
            let log = 63 - ns.leading_zeros() as usize;
            log.min(NUM_LOG2_BUCKETS - 1)
        }
    }

    pub fn record(&mut self, ns: u64) {
        self.count = self.count.saturating_add(1);
        self.total_ns = self.total_ns.saturating_add(ns);
        self.min_ns = Some(self.min_ns.map_or(ns, |m| m.min(ns)));
        self.max_ns = Some(self.max_ns.map_or(ns, |m| m.max(ns)));
        let idx = Self::bucket_index(ns);
        self.buckets[idx] = self.buckets[idx].saturating_add(1);
    }

    /// Upper bound estimate for a given percentile (e.g. 0.95 for p95, 0.99 for p99) in nanoseconds.
    /// Returns 0 if count == 0.
    pub fn percentile_upper_bound_ns(&self, p: f64) -> u64 {
        if self.count == 0 {
            return 0;
        }
        let target_rank = ((self.count as f64) * p).ceil() as u64;
        let mut accumulated = 0u64;
        for (i, &bucket_count) in self.buckets.iter().enumerate() {
            accumulated = accumulated.saturating_add(bucket_count);
            if accumulated >= target_rank {
                if i == NUM_LOG2_BUCKETS - 1 {
                    return self.max_ns.unwrap_or(0);
                }
                // Upper bound of bucket i is 2^(i+1) - 1, except bucket 0 which is 1
                return (1u64 << (i + 1)).saturating_sub(1);
            }
        }
        self.max_ns.unwrap_or(0)
    }

    pub fn summarize(&self) -> DurationStats {
        DurationStats {
            count: self.count,
            total_ns: self.total_ns,
            min_ns: self.min_ns.unwrap_or(0),
            max_ns: self.max_ns.unwrap_or(0),
            p95_upper_bound_ns: self.percentile_upper_bound_ns(0.95),
            p99_upper_bound_ns: self.percentile_upper_bound_ns(0.99),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DurationStats {
    pub count: u64,
    pub total_ns: u64,
    pub min_ns: u64,
    pub max_ns: u64,
    pub p95_upper_bound_ns: u64,
    pub p99_upper_bound_ns: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SqlProfileSummary {
    pub count: u64,
    pub total_ns: u64,
    pub min_ns: u64,
    pub max_ns: u64,
    pub p95_upper_bound_ns: u64,
    pub p99_upper_bound_ns: u64,
    pub histogram: DurationHistogram,
}

#[derive(Debug, Clone, Serialize)]
pub struct LockProfileSummary {
    pub count: u64,
    pub total_ns: u64,
    pub min_ns: u64,
    pub max_ns: u64,
    pub p95_upper_bound_ns: u64,
    pub p99_upper_bound_ns: u64,
    pub histogram: DurationHistogram,
}

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    /// Total statements counted.
    pub statements: u64,
    /// Statement counts indexed by prepared SQL template (max 512 entries).
    pub by_prepared_sql: BTreeMap<String, u64>,
    /// Execution duration metrics per prepared SQL statement template.
    pub profile_by_sql: BTreeMap<String, SqlProfileSummary>,
    /// Pool lock acquisition wait durations (reader / writer).
    pub locks: BTreeMap<String, LockProfileSummary>,
}

#[cfg(feature = "sql-profile")]
pub(crate) fn install(conn: &rusqlite::Connection) {
    let mask = TraceEventCodes::SQLITE_TRACE_STMT | TraceEventCodes::SQLITE_TRACE_PROFILE;
    conn.trace_v2(mask, Some(on_trace_event));
}

#[cfg(not(feature = "sql-profile"))]
pub(crate) fn install(_conn: &rusqlite::Connection) {}

#[cfg(feature = "sql-profile")]
fn on_trace_event(event: TraceEvent<'_>) {
    if !ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    match event {
        TraceEvent::Stmt(stmt, _) => {
            // sqlite3_sql, NOT expanded_sql: never collect bound credentials/payloads.
            let sql = stmt.sql();
            let mut counts = STATEMENT_COUNTS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !ACTIVE.load(Ordering::Relaxed) {
                return;
            }
            if counts.contains_key(sql.as_ref()) || counts.len() < MAX_SQL_ENTRIES {
                *counts.entry(sql.into_owned()).or_default() += 1;
            } else {
                *counts.entry("[other statements]".into()).or_default() += 1;
            }
        }
        TraceEvent::Profile(stmt, duration) => {
            let sql = stmt.sql();
            let ns = duration.as_nanos().min(u64::MAX as u128) as u64;
            let mut profiles = SQL_PROFILES
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !ACTIVE.load(Ordering::Relaxed) {
                return;
            }
            if profiles.contains_key(sql.as_ref()) || profiles.len() < MAX_SQL_ENTRIES {
                profiles.entry(sql.into_owned()).or_default().record(ns);
            } else {
                profiles
                    .entry("[other statements]".into())
                    .or_default()
                    .record(ns);
            }
        }
        _ => {}
    }
}

/// Internal API to record pool lock acquisition wait time.
/// Low-complexity inline fast exit when profile inactive.
#[inline]
pub fn record_lock_wait(kind: LockKind, elapsed: Duration) {
    if !ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    let ns = elapsed.as_nanos().min(u64::MAX as u128) as u64;
    let mut locks = LOCK_PROFILES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    locks
        .entry(kind.as_str().to_string())
        .or_default()
        .record(ns);
}

/// Start profiling. Clears counters, profiles and lock waits.
pub fn start() {
    STATEMENT_COUNTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
    SQL_PROFILES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
    LOCK_PROFILES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
    ACTIVE.store(true, Ordering::Relaxed);
}

/// Stop profiling and return a frozen snapshot.
pub fn stop() -> Snapshot {
    ACTIVE.store(false, Ordering::Relaxed);
    let counts = STATEMENT_COUNTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let sql_profs = SQL_PROFILES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let lock_profs = LOCK_PROFILES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();

    let profile_by_sql = sql_profs
        .into_iter()
        .map(|(sql, hist)| {
            let stats = hist.summarize();
            (
                sql,
                SqlProfileSummary {
                    count: stats.count,
                    total_ns: stats.total_ns,
                    min_ns: stats.min_ns,
                    max_ns: stats.max_ns,
                    p95_upper_bound_ns: stats.p95_upper_bound_ns,
                    p99_upper_bound_ns: stats.p99_upper_bound_ns,
                    histogram: hist,
                },
            )
        })
        .collect();

    let locks = lock_profs
        .into_iter()
        .map(|(kind, hist)| {
            let stats = hist.summarize();
            (
                kind,
                LockProfileSummary {
                    count: stats.count,
                    total_ns: stats.total_ns,
                    min_ns: stats.min_ns,
                    max_ns: stats.max_ns,
                    p95_upper_bound_ns: stats.p95_upper_bound_ns,
                    p99_upper_bound_ns: stats.p99_upper_bound_ns,
                    histogram: hist,
                },
            )
        })
        .collect();

    Snapshot {
        statements: counts.values().sum(),
        by_prepared_sql: counts,
        profile_by_sql,
        locks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Lifecycle tests share the process-wide profiler. Keep their reset/stop
    // windows disjoint even when the test runner uses multiple threads.
    static PROFILE_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn counts_execution_on_every_pool_connection_without_expanding_bindings() {
        let _guard = PROFILE_TEST_LOCK.lock().unwrap();
        let pool = crate::DbPool::test_pool().unwrap();
        start();
        pool.writer()
            .query_row("SELECT ?1", ["secret-credential"], |r| {
                r.get::<_, String>(0)
            })
            .unwrap();
        pool.reader()
            .query_row("SELECT 2", [], |r| r.get::<_, i64>(0))
            .unwrap();
        let snapshot = stop();
        assert!(snapshot.statements >= 2);
        assert!(snapshot.by_prepared_sql.contains_key("SELECT ?1"));
        assert!(!format!("{:?}", snapshot.by_prepared_sql).contains("secret-credential"));
        pool.reader()
            .query_row("SELECT 3", [], |r| r.get::<_, i64>(0))
            .unwrap();
        assert_eq!(stop().statements, snapshot.statements);
    }

    #[test]
    fn duration_histogram_bounds_and_percentiles() {
        let mut hist = DurationHistogram::new();
        assert_eq!(hist.percentile_upper_bound_ns(0.95), 0);
        hist.record(100);
        hist.record(200);
        hist.record(500);
        assert_eq!(hist.count, 3);
        assert_eq!(hist.total_ns, 800);
        assert_eq!(hist.min_ns, Some(100));
        assert_eq!(hist.max_ns, Some(500));
        let stats = hist.summarize();
        assert_eq!(stats.count, 3);
        assert_eq!(stats.total_ns, 800);
        assert!(stats.p95_upper_bound_ns >= 500);
        hist.record(u64::MAX);
        assert_eq!(hist.percentile_upper_bound_ns(1.0), u64::MAX);
    }

    #[test]
    fn bounded_map_entries_cannot_exceed_capacity() {
        let _guard = PROFILE_TEST_LOCK.lock().unwrap();
        start();
        // Emulate 600 distinct SQL statement profiles
        for i in 0..600 {
            let sql = format!("SELECT {i}");
            let mut profiles = SQL_PROFILES
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if profiles.contains_key(&sql) || profiles.len() < MAX_SQL_ENTRIES {
                profiles.entry(sql).or_default().record(10);
            } else {
                profiles
                    .entry("[other statements]".into())
                    .or_default()
                    .record(10);
            }
        }
        let snapshot = stop();
        // 512 distinct + 1 for "[other statements]"
        assert!(snapshot.profile_by_sql.len() <= MAX_SQL_ENTRIES + 1);
        assert!(snapshot.profile_by_sql.contains_key("[other statements]"));
    }

    #[test]
    fn lock_wait_recording_and_snapshot() {
        let _guard = PROFILE_TEST_LOCK.lock().unwrap();
        start();
        record_lock_wait(LockKind::Reader, Duration::from_micros(50));
        record_lock_wait(LockKind::Writer, Duration::from_micros(120));
        let snapshot = stop();
        assert!(snapshot.locks.contains_key("reader"));
        assert!(snapshot.locks.contains_key("writer"));
        let reader_lock = &snapshot.locks["reader"];
        assert_eq!(reader_lock.count, 1);
        assert_eq!(reader_lock.total_ns, 50_000);
    }

    #[test]
    fn no_leakage_of_bindings_in_profile_keys() {
        let _guard = PROFILE_TEST_LOCK.lock().unwrap();
        let pool = crate::DbPool::test_pool().unwrap();
        start();
        let secret = "sensitive_bearer_token_xyz";
        pool.writer()
            .query_row("SELECT ?1 AS token", [secret], |r| r.get::<_, String>(0))
            .unwrap();
        let snapshot = stop();
        let serialized = serde_json::to_string(&snapshot).unwrap();
        assert!(!serialized.contains(secret));
    }
}
