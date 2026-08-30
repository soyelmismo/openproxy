use crate::ids::ComboTargetId;
use crate::time::now_ms;
use std::collections::HashMap;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
pub struct SelectionRegistry {
    inner: RwLock<HashMap<i64, SelectionRegistryEntry>>,
}

#[derive(Debug, Default)]
struct SelectionRegistryEntry {
    last_success_ms: AtomicU64,
    last_activity_ms: AtomicU64,
    request_count: AtomicU64,
}

impl SelectionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_success(&self, target_id: ComboTargetId) {
        let now = now_ms();
        if let Some(e) = self
            .inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&target_id.0)
        {
            e.last_success_ms.store(now, Ordering::Relaxed);
            e.last_activity_ms.store(now, Ordering::Relaxed);
            let _ = e
                .request_count
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                    Some(v.saturating_add(1))
                });
            return;
        }

        let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let e = g.entry(target_id.0).or_default();
        e.last_success_ms.store(now, Ordering::Relaxed);
        e.last_activity_ms.store(now, Ordering::Relaxed);
        let _ = e
            .request_count
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_add(1))
            });
    }

    pub fn record_failure(&self, target_id: ComboTargetId) {
        let now = now_ms();
        if let Some(e) = self
            .inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&target_id.0)
        {
            e.last_activity_ms.store(now, Ordering::Relaxed);
            e.last_success_ms.store(0, Ordering::Relaxed);
            let _ = e
                .request_count
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                    Some(v.saturating_add(1))
                });
            return;
        }

        let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let e = g.entry(target_id.0).or_default();
        e.last_activity_ms.store(now, Ordering::Relaxed);
        e.last_success_ms.store(0, Ordering::Relaxed);
        let _ = e
            .request_count
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_add(1))
            });
    }

    pub fn record_request(&self, target_id: ComboTargetId) {
        let now = now_ms();
        if let Some(e) = self
            .inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&target_id.0)
        {
            e.last_activity_ms.store(now, Ordering::Relaxed);
            let _ = e
                .request_count
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                    Some(v.saturating_add(1))
                });
            return;
        }

        let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let e = g.entry(target_id.0).or_default();
        e.last_activity_ms.store(now, Ordering::Relaxed);
        let _ = e
            .request_count
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_add(1))
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

        // record_request doesn't update last_success_ms
        assert_eq!(registry.last_success_within(target_1, 10), 0);
        // But request count should be 1
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

        // Wait a small amount to ensure time passes
        std::thread::sleep(Duration::from_millis(10));

        // Within large window, should return values
        assert!(registry.last_success_within(target_1, 10) > 0);
        assert_eq!(registry.request_count_within(target_1, 10), 1);

        // Outside window (0 seconds), should return 0
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

        // Wait a little bit
        std::thread::sleep(Duration::from_millis(10));

        // Pruning with large max_age should not remove anything
        assert_eq!(registry.prune_stale(Duration::from_secs(10)), 0);
        assert_eq!(registry.len(), 2);

        // Pruning with 0 max_age should remove both targets (since activity is now > 0ms old)
        let removed = registry.prune_stale(Duration::from_millis(0));
        assert_eq!(removed, 2);
        assert_eq!(registry.len(), 0);
    }
}
