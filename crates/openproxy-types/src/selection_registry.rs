use crate::ids::ComboTargetId;
use parking_lot::Mutex;
use std::collections::HashMap;

#[derive(Default)]
pub struct SelectionRegistry {
    inner: Mutex<HashMap<i64, SelectionRegistryEntry>>,
}

#[derive(Debug, Clone, Copy, Default)]
struct SelectionRegistryEntry {
    last_success_ms: u64,
    request_count: u64,
}

impl SelectionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_success(&self, target_id: ComboTargetId) {
        let now = now_ms();
        let mut g = self.inner.lock();
        let e = g.entry(target_id.0).or_default();
        e.last_success_ms = now;
        e.request_count = e.request_count.saturating_add(1);
    }

    pub fn record_request(&self, target_id: ComboTargetId) {
        let mut g = self.inner.lock();
        let e = g.entry(target_id.0).or_default();
        e.request_count = e.request_count.saturating_add(1);
    }

    pub fn last_success_within(&self, target_id: ComboTargetId, window_secs: u64) -> u64 {
        let g = self.inner.lock();
        match g.get(&target_id.0) {
            Some(e) if e.last_success_ms > 0 => {
                let now = now_ms();
                let window_ms = window_secs.saturating_mul(1000);
                if now.saturating_sub(e.last_success_ms) <= window_ms {
                    e.last_success_ms
                } else {
                    0
                }
            }
            _ => 0,
        }
    }

    pub fn request_count_within(&self, target_id: ComboTargetId, window_secs: u64) -> u64 {
        let g = self.inner.lock();
        match g.get(&target_id.0) {
            Some(e) if e.request_count > 0 => {
                if e.last_success_ms == 0 {
                    return e.request_count;
                }
                let now = now_ms();
                let window_ms = window_secs.saturating_mul(1000);
                if now.saturating_sub(e.last_success_ms) <= window_ms {
                    e.request_count
                } else {
                    0
                }
            }
            _ => 0,
        }
    }

    pub fn prune_stale(&self, max_age: std::time::Duration) -> usize {
        let mut g = self.inner.lock();
        let now = now_ms();
        let cutoff = now.saturating_sub(max_age.as_millis() as u64);
        let before = g.len();
        g.retain(|_, e| {
            if e.last_success_ms > 0 && e.last_success_ms >= cutoff {
                return true;
            }
            if e.last_success_ms == 0 && e.request_count > 0 {
                return true;
            }
            false
        });
        before - g.len()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().is_empty()
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_new_and_basic_metrics() {
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

        // Pruning with 0 max_age should remove target_1 (since its success is now > 0ms old)
        let removed = registry.prune_stale(Duration::from_millis(0));
        assert_eq!(removed, 1);
        assert_eq!(registry.len(), 1);
    }
}
