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
