use crate::ids::ComboTargetId;
use crate::time::now_ms;
use std::collections::HashMap;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
pub struct SelectionRegistry {
    inner: RwLock<HashMap<i64, SelectionRegistryEntry>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TargetReputationMetrics {
    pub success_count: u64,
    pub failure_count: u64,
    pub timeout_count: u64,
    pub latency_samples: u64,
    pub total_latency_ms: u64,
    pub last_success_ms: u64,
    pub last_activity_ms: u64,
}

#[derive(Debug, Default)]
struct SelectionRegistryEntry {
    last_success_ms: AtomicU64,
    last_activity_ms: AtomicU64,
    request_count: AtomicU64,
    success_count: AtomicU64,
    failure_count: AtomicU64,
    timeout_count: AtomicU64,
    total_latency_ms: AtomicU64,
    latency_samples: AtomicU64,
}

impl SelectionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn update_entry(&self, target_id: ComboTargetId, update: impl FnOnce(&SelectionRegistryEntry)) {
        {
            let g = self.inner.read().unwrap_or_else(|e| e.into_inner());
            if let Some(e) = g.get(&target_id.0) {
                update(e);
                return;
            }
        }

        let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let e = g.entry(target_id.0).or_default();
        update(e);
    }

    pub fn record_success(&self, target_id: ComboTargetId) {
        self.record_success_with_latency(target_id, 0);
    }

    pub fn record_success_with_latency(&self, target_id: ComboTargetId, latency_ms: u64) {
        let now = now_ms();
        self.update_entry(target_id, |e| {
            e.last_success_ms.store(now, Ordering::Relaxed);
            e.last_activity_ms.store(now, Ordering::Relaxed);
            e.request_count.fetch_add(1, Ordering::Relaxed);
            e.success_count.fetch_add(1, Ordering::Relaxed);
            if latency_ms > 0 {
                e.total_latency_ms.fetch_add(latency_ms, Ordering::Relaxed);
                e.latency_samples.fetch_add(1, Ordering::Relaxed);
            }
        });
    }

    pub fn record_failure(&self, target_id: ComboTargetId) {
        self.record_failure_with_kind(target_id, false);
    }

    pub fn record_failure_with_kind(&self, target_id: ComboTargetId, is_timeout: bool) {
        let now = now_ms();
        self.update_entry(target_id, |e| {
            e.last_activity_ms.store(now, Ordering::Relaxed);
            e.last_success_ms.store(0, Ordering::Relaxed);
            e.request_count.fetch_add(1, Ordering::Relaxed);
            e.failure_count.fetch_add(1, Ordering::Relaxed);
            if is_timeout {
                e.timeout_count.fetch_add(1, Ordering::Relaxed);
            }
        });
    }

    /// Composite reputation score in [0.05, 1.0]: successes / (successes +
    /// failures), damped by a timeout penalty, a 0.6x penalty for a failure in the
    /// last 60s, and latency above 10s. A target with no traffic in the window
    /// scores 1.0.
    pub fn reputation_score(&self, target_id: ComboTargetId, window_secs: u64) -> f64 {
        let g = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let Some(e) = g.get(&target_id.0) else {
            return 1.0;
        };

        let last_activity = e.last_activity_ms.load(Ordering::Relaxed);
        if last_activity == 0 {
            return 1.0;
        }

        let now = now_ms();
        let window_ms = window_secs.saturating_mul(1000);
        if now.saturating_sub(last_activity) > window_ms {
            return 1.0;
        }

        let successes = e.success_count.load(Ordering::Relaxed);
        let failures = e.failure_count.load(Ordering::Relaxed);
        let timeouts = e.timeout_count.load(Ordering::Relaxed);
        let total = successes.saturating_add(failures);
        if total == 0 {
            return 1.0;
        }

        let success_rate = (successes as f64) / (total as f64);

        let timeout_factor = if timeouts > 0 {
            let timeout_ratio = (timeouts as f64) / (total as f64);
            (1.0 - (0.5 * timeout_ratio)).clamp(0.2, 1.0)
        } else {
            1.0
        };

        let last_success = e.last_success_ms.load(Ordering::Relaxed);
        let recency_penalty =
            if last_success < last_activity && now.saturating_sub(last_activity) < 60_000 {
                0.6
            } else {
                1.0
            };

        let latency_samples = e.latency_samples.load(Ordering::Relaxed);
        let latency_factor = if let Some(avg_ms) = e
            .total_latency_ms
            .load(Ordering::Relaxed)
            .checked_div(latency_samples)
        {
            if avg_ms > 10_000 {
                let excess = (avg_ms - 10_000) as f64;
                (1.0 - (excess / 50_000.0)).clamp(0.2, 1.0)
            } else {
                1.0
            }
        } else {
            1.0
        };

        (success_rate * timeout_factor * recency_penalty * latency_factor).clamp(0.05, 1.0)
    }

    pub fn target_metrics(&self, target_id: ComboTargetId) -> TargetReputationMetrics {
        let g = self.inner.read().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = g.get(&target_id.0) {
            TargetReputationMetrics {
                success_count: e.success_count.load(Ordering::Relaxed),
                failure_count: e.failure_count.load(Ordering::Relaxed),
                timeout_count: e.timeout_count.load(Ordering::Relaxed),
                latency_samples: e.latency_samples.load(Ordering::Relaxed),
                total_latency_ms: e.total_latency_ms.load(Ordering::Relaxed),
                last_success_ms: e.last_success_ms.load(Ordering::Relaxed),
                last_activity_ms: e.last_activity_ms.load(Ordering::Relaxed),
            }
        } else {
            TargetReputationMetrics::default()
        }
    }

    pub fn record_request(&self, target_id: ComboTargetId) {
        let now = now_ms();
        self.update_entry(target_id, |e| {
            e.last_activity_ms.store(now, Ordering::Relaxed);
            let _ = e
                .request_count
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                    Some(v.saturating_add(1))
                });
        });
    }

    pub fn last_success_within(&self, target_id: ComboTargetId, window_secs: u64) -> u64 {
        let g = self.inner.read().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = g.get(&target_id.0) {
            let success_ms = e.last_success_ms.load(Ordering::Relaxed);
            if success_ms > 0 {
                let now = now_ms();
                let window_ms = window_secs.saturating_mul(1000);
                if now.saturating_sub(success_ms) <= window_ms {
                    return success_ms;
                }
            }
        }
        0
    }

    pub fn last_activity_within(&self, target_id: ComboTargetId, window_secs: u64) -> u64 {
        let g = self.inner.read().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = g.get(&target_id.0) {
            let activity_ms = e.last_activity_ms.load(Ordering::Relaxed);
            if activity_ms > 0 {
                let now = now_ms();
                let window_ms = window_secs.saturating_mul(1000);
                if now.saturating_sub(activity_ms) <= window_ms {
                    return activity_ms;
                }
            }
        }
        0
    }

    fn resolve_entry_reference_ms(e: &SelectionRegistryEntry) -> u64 {
        let success_ms = e.last_success_ms.load(Ordering::Relaxed);
        if success_ms > 0 {
            success_ms
        } else {
            e.last_activity_ms.load(Ordering::Relaxed)
        }
    }

    fn get_entry_request_count(e: &SelectionRegistryEntry, window_secs: u64) -> u64 {
        let request_count = e.request_count.load(Ordering::Relaxed);
        if request_count == 0 {
            return 0;
        }
        let reference_ms = Self::resolve_entry_reference_ms(e);
        if reference_ms == 0 {
            return request_count;
        }
        let window_ms = window_secs.saturating_mul(1000);
        if now_ms().saturating_sub(reference_ms) <= window_ms {
            request_count
        } else {
            0
        }
    }

    pub fn request_count_within(&self, target_id: ComboTargetId, window_secs: u64) -> u64 {
        let g = self.inner.read().unwrap_or_else(|e| e.into_inner());
        g.get(&target_id.0)
            .map_or(0, |e| Self::get_entry_request_count(e, window_secs))
    }

    pub fn prune_stale(&self, max_age: std::time::Duration) -> usize {
        let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let now = now_ms();
        let cutoff = now.saturating_sub(max_age.as_millis() as u64);
        let before = g.len();
        g.retain(|_, e| {
            let last_active = e
                .last_success_ms
                .load(Ordering::Relaxed)
                .max(e.last_activity_ms.load(Ordering::Relaxed));
            last_active > 0 && last_active >= cutoff
        });
        before - g.len()
    }

    pub fn len(&self) -> usize {
        self.inner.read().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_new_and_basic_metrics() {
        let registry = SelectionRegistry::new();
        let t = ComboTargetId(999);
        assert_eq!(registry.last_success_within(t, 10), 0);
        assert_eq!(registry.request_count_within(t, 10), 0);
        registry.record_request(t);
        assert_eq!(registry.last_success_within(t, 10), 0);
        assert_eq!(registry.request_count_within(t, 10), 1);

        let registry = SelectionRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);

        let target_1 = ComboTargetId(1);

        registry.record_request(target_1);
        assert!(!registry.is_empty());
        assert_eq!(registry.len(), 1);

        assert_eq!(registry.last_success_within(target_1, 10), 0);
        assert_eq!(registry.request_count_within(target_1, 10), 1);

        registry.record_success(target_1);
        assert_eq!(registry.len(), 1);

        let last_success = registry.last_success_within(target_1, 10);
        assert!(last_success > 0);
        assert_eq!(registry.request_count_within(target_1, 10), 2);
    }

    #[test]
    fn test_record_success_new_target() {
        let registry = SelectionRegistry::new();
        let target = ComboTargetId(42);

        registry.record_success(target);
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.request_count_within(target, 10), 1);
        assert!(registry.last_success_within(target, 10) > 0);
    }

    #[test]
    fn test_record_failure_clears_last_success() {
        let registry = SelectionRegistry::new();
        let target = ComboTargetId(42);

        registry.record_success(target);
        assert!(registry.last_success_within(target, 10) > 0);

        registry.record_failure(target);
        assert_eq!(registry.last_success_within(target, 10), 0);
        assert_eq!(registry.request_count_within(target, 10), 2);
    }

    #[test]
    fn test_time_windows() {
        let registry = SelectionRegistry::new();
        let target_1 = ComboTargetId(1);

        registry.record_success(target_1);

        std::thread::sleep(Duration::from_millis(10));

        assert!(registry.last_success_within(target_1, 10) > 0);
        assert_eq!(registry.request_count_within(target_1, 10), 1);

        assert_eq!(registry.last_success_within(target_1, 0), 0);
        assert_eq!(registry.request_count_within(target_1, 0), 0);
    }

    #[test]
    fn test_prune_stale() {
        let registry = SelectionRegistry::new();
        let target_1 = ComboTargetId(1);
        let target_2 = ComboTargetId(2);

        registry.record_success(target_1);
        registry.record_request(target_2);

        std::thread::sleep(Duration::from_millis(10));

        assert_eq!(registry.prune_stale(Duration::from_secs(10)), 0);
        assert_eq!(registry.len(), 2);

        let removed = registry.prune_stale(Duration::from_millis(0));
        assert_eq!(removed, 2);
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn test_last_activity_within() {
        let registry = SelectionRegistry::new();
        let target_1 = ComboTargetId(1);
        let target_2 = ComboTargetId(2);

        assert_eq!(registry.last_activity_within(target_1, 10), 0);

        registry.record_request(target_1);
        let act1 = registry.last_activity_within(target_1, 10);
        assert!(act1 > 0);

        registry.record_failure(target_2);
        let act2 = registry.last_activity_within(target_2, 10);
        assert!(act2 > 0);

        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(registry.last_activity_within(target_1, 0), 0);
    }

    #[test]
    fn test_reputation_score_and_penalties() {
        let registry = SelectionRegistry::new();
        let healthy = ComboTargetId(10);
        let failing = ComboTargetId(20);
        let timeout = ComboTargetId(30);
        let untried = ComboTargetId(40);

        assert_eq!(registry.reputation_score(untried, 60), 1.0);

        for _ in 0..10 {
            registry.record_success_with_latency(healthy, 200);
        }
        let score_healthy = registry.reputation_score(healthy, 60);
        assert!((score_healthy - 1.0).abs() < 1e-4);

        for _ in 0..2 {
            registry.record_success_with_latency(failing, 300);
        }
        for _ in 0..8 {
            registry.record_failure_with_kind(failing, false);
        }
        let score_failing = registry.reputation_score(failing, 60);
        assert!(score_failing < 0.25);
        assert!(score_failing >= 0.05);

        for _ in 0..5 {
            registry.record_success_with_latency(timeout, 500);
        }
        for _ in 0..5 {
            registry.record_failure_with_kind(timeout, true);
        }
        let score_timeout = registry.reputation_score(timeout, 60);
        assert!(score_timeout < score_healthy);
        assert!(score_timeout < 0.35);

        let metrics = registry.target_metrics(timeout);
        assert_eq!(metrics.success_count, 5);
        assert_eq!(metrics.failure_count, 5);
        assert_eq!(metrics.timeout_count, 5);
    }

    #[test]
    fn test_update_entry_invocation_and_len() {
        let registry = SelectionRegistry::new();
        let target = ComboTargetId(101);
        let mut calls = 0;

        registry.update_entry(target, |_| {
            calls += 1;
        });
        assert_eq!(calls, 1);
        assert_eq!(registry.len(), 1);

        registry.update_entry(target, |_| {
            calls += 1;
        });
        assert_eq!(calls, 2);
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn test_reader_concurrency_fastpath() {
        let registry = std::sync::Arc::new(SelectionRegistry::new());
        let target = ComboTargetId(202);
        registry.record_request(target);

        let (tx, rx) = std::sync::mpsc::channel();
        let reg_clone = std::sync::Arc::clone(&registry);

        let read_guard = registry.inner.read().unwrap_or_else(|e| e.into_inner());

        let handle = std::thread::spawn(move || {
            reg_clone.record_success(target);
            let _ = tx.send(());
        });

        let recv_res = rx.recv_timeout(Duration::from_secs(2));
        drop(read_guard);
        let join_res = handle.join();

        assert!(
            recv_res.is_ok(),
            "Fastpath update should not block on existing read guard"
        );
        assert!(join_res.is_ok());
        assert_eq!(registry.target_metrics(target).success_count, 1);
    }

    #[test]
    fn test_concurrent_lazy_init_same_id() {
        let registry = std::sync::Arc::new(SelectionRegistry::new());
        let target = ComboTargetId(303);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
        let mut handles = Vec::new();

        for _ in 0..4 {
            let reg = std::sync::Arc::clone(&registry);
            let bar = std::sync::Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                bar.wait();
                for _ in 0..100 {
                    reg.record_success(target);
                }
            }));
        }

        for h in handles {
            h.join().expect("worker thread should not panic");
        }

        let metrics = registry.target_metrics(target);
        assert_eq!(metrics.success_count, 400);
        assert_eq!(metrics.failure_count, 0);
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn test_poison_recovery_write_panic() {
        let registry = std::sync::Arc::new(SelectionRegistry::new());
        let reg_clone = std::sync::Arc::clone(&registry);

        let _ = std::thread::spawn(move || {
            let _guard = reg_clone.inner.write().unwrap();
            panic!("intentional panic while holding write lock");
        })
        .join();

        assert!(registry.inner.is_poisoned());

        let target = ComboTargetId(404);
        registry.record_request(target);
        registry.record_success(target);
        registry.record_failure(target);

        assert_eq!(registry.len(), 1);
        let metrics = registry.target_metrics(target);
        assert_eq!(metrics.success_count, 1);
        assert_eq!(metrics.failure_count, 1);
    }

    #[test]
    fn test_saturated_request_count_vs_success_wrap() {
        let registry = SelectionRegistry::new();
        let target = ComboTargetId(505);

        registry.record_request(target);
        {
            let g = registry.inner.write().unwrap_or_else(|e| e.into_inner());
            let entry = g.get(&target.0).expect("entry must exist");
            entry.request_count.store(u64::MAX, Ordering::Relaxed);
        }

        registry.record_request(target);
        {
            let g = registry.inner.read().unwrap_or_else(|e| e.into_inner());
            let entry = g.get(&target.0).expect("entry must exist");
            assert_eq!(entry.request_count.load(Ordering::Relaxed), u64::MAX);
        }

        registry.record_success(target);
        {
            let g = registry.inner.read().unwrap_or_else(|e| e.into_inner());
            let entry = g.get(&target.0).expect("entry must exist");
            assert_eq!(entry.request_count.load(Ordering::Relaxed), 0);
        }
    }
}
