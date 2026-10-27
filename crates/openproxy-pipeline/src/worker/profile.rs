//! Opt-in group commit and journal worker telemetry.
//!
//! Enabled strictly under `feature = "benchmark-profile"`. All timers, locks,
//! and module structures disappear in default builds.
//! Measures bounded log2 histograms (sizes, microsecond wait and transaction times)
//! without recording sensitive payloads, job contents, or IDs.

use serde::Serialize;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static ACTIVE: AtomicBool = AtomicBool::new(false);
static ADMISSION_METRICS: Mutex<StageMetrics> = Mutex::new(StageMetrics::new());
static REPLAY_METRICS: Mutex<StageMetrics> = Mutex::new(StageMetrics::new());

const NUM_LOG2_BUCKETS: usize = 32;

/// Bounded log2 histogram for non-negative values (counts or microseconds).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Histogram {
    pub count: u64,
    pub sum: u64,
    pub min: Option<u64>,
    pub max: Option<u64>,
    /// Buckets where index `i` (0..=31) covers `[2^i, 2^(i+1) - 1]`, with bucket 0 covering `[0, 1]`.
    pub buckets: [u64; NUM_LOG2_BUCKETS],
}

impl Default for Histogram {
    fn default() -> Self {
        Self {
            count: 0,
            sum: 0,
            min: None,
            max: None,
            buckets: [0; NUM_LOG2_BUCKETS],
        }
    }
}

impl Histogram {
    pub const fn new() -> Self {
        Self {
            count: 0,
            sum: 0,
            min: None,
            max: None,
            buckets: [0; NUM_LOG2_BUCKETS],
        }
    }

    #[inline]
    fn bucket_index(val: u64) -> usize {
        if val == 0 {
            0
        } else {
            let log = 63 - val.leading_zeros() as usize;
            log.min(NUM_LOG2_BUCKETS - 1)
        }
    }

    pub fn record(&mut self, val: u64) {
        self.count = self.count.saturating_add(1);
        self.sum = self.sum.saturating_add(val);
        self.min = Some(self.min.map_or(val, |m| m.min(val)));
        self.max = Some(self.max.map_or(val, |m| m.max(val)));
        let idx = Self::bucket_index(val);
        self.buckets[idx] = self.buckets[idx].saturating_add(1);
    }

    /// Estimated upper bound for percentile `p` (e.g. 0.95 for p95).
    pub fn quantile_upper_bound(&self, p: f64) -> u64 {
        if self.count == 0 {
            return 0;
        }
        let target_rank = ((self.count as f64) * p).ceil() as u64;
        let mut accumulated = 0u64;
        for (i, &bucket_count) in self.buckets.iter().enumerate() {
            accumulated = accumulated.saturating_add(bucket_count);
            if accumulated >= target_rank {
                if i == NUM_LOG2_BUCKETS - 1 {
                    return self.max.unwrap_or(0);
                }
                return (1u64 << (i + 1)).saturating_sub(1);
            }
        }
        self.max.unwrap_or(0)
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct StageMetrics {
    pub batches: u64,
    pub entries: u64,
    pub batch_size: Histogram,
    /// Oldest admission's enqueue-to-dequeue wait per batch, excluding the
    /// separately reported SQLite lock wait and transaction duration.
    /// Replay does not enqueue here and leaves this histogram empty.
    pub queue_wait_us: Histogram,
    pub lock_wait_us: Histogram,
    pub transaction_us: Histogram,
}

impl StageMetrics {
    pub const fn new() -> Self {
        Self {
            batches: 0,
            entries: 0,
            batch_size: Histogram::new(),
            queue_wait_us: Histogram::new(),
            lock_wait_us: Histogram::new(),
            transaction_us: Histogram::new(),
        }
    }

    pub fn clear(&mut self) {
        *self = Self::new();
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub admission: StageMetrics,
    pub replay: StageMetrics,
}

/// Starts collecting group-commit and drain metrics.
pub fn start() {
    ADMISSION_METRICS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
    REPLAY_METRICS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
    ACTIVE.store(true, Ordering::Relaxed);
}

/// Stops collecting metrics and returns a serialized snapshot.
pub fn stop() -> Snapshot {
    ACTIVE.store(false, Ordering::Relaxed);
    let admission = ADMISSION_METRICS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let replay = REPLAY_METRICS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    Snapshot { admission, replay }
}

#[inline]
pub fn is_active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

#[inline]
pub(crate) fn record_admission_commit(
    batch_size: usize,
    queue_wait: Duration,
    lock_wait: Duration,
    transaction: Duration,
) {
    if !is_active() {
        return;
    }
    let mut g = ADMISSION_METRICS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !is_active() {
        return;
    }
    g.batches = g.batches.saturating_add(1);
    g.entries = g.entries.saturating_add(batch_size as u64);
    g.batch_size.record(batch_size as u64);
    g.queue_wait_us
        .record(queue_wait.as_micros().min(u64::MAX as u128) as u64);
    g.lock_wait_us
        .record(lock_wait.as_micros().min(u64::MAX as u128) as u64);
    g.transaction_us
        .record(transaction.as_micros().min(u64::MAX as u128) as u64);
}

#[inline]
pub(crate) fn record_replay_commit(batch_size: usize, lock_wait: Duration, transaction: Duration) {
    if !is_active() {
        return;
    }
    let mut g = REPLAY_METRICS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !is_active() {
        return;
    }
    g.batches = g.batches.saturating_add(1);
    g.entries = g.entries.saturating_add(batch_size as u64);
    g.batch_size.record(batch_size as u64);
    g.lock_wait_us
        .record(lock_wait.as_micros().min(u64::MAX as u128) as u64);
    g.transaction_us
        .record(transaction.as_micros().min(u64::MAX as u128) as u64);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_histogram_recording_and_quantile() {
        let mut h = Histogram::new();
        assert_eq!(h.quantile_upper_bound(0.95), 0);
        h.record(10);
        h.record(20);
        h.record(100);
        assert_eq!(h.count, 3);
        assert_eq!(h.sum, 130);
        assert_eq!(h.min, Some(10));
        assert_eq!(h.max, Some(100));
        assert!(h.quantile_upper_bound(0.95) >= 100);
        h.record(u64::MAX);
        assert_eq!(h.quantile_upper_bound(1.0), u64::MAX);
    }

    #[test]
    fn test_profile_lifecycle() {
        start();
        assert!(is_active());
        record_admission_commit(
            5,
            Duration::from_micros(10),
            Duration::from_micros(20),
            Duration::from_micros(30),
        );
        record_replay_commit(12, Duration::from_micros(15), Duration::from_micros(25));
        let snap = stop();
        assert!(!is_active());
        assert_eq!(snap.admission.batches, 1);
        assert_eq!(snap.admission.entries, 5);
        assert_eq!(snap.admission.batch_size.count, 1);
        assert_eq!(snap.replay.batches, 1);
        assert_eq!(snap.replay.entries, 12);
        assert_eq!(snap.replay.batch_size.count, 1);
    }
}
