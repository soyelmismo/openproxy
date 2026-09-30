//! Simple per-key rate limiter using a sliding window.
//!
//! Not a token bucket: a "max N requests per minute per API key" guard backed by
//! a DashMap for O(1) lookups, with entries cleaned lazily on insert.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use openproxy_types::ids::ApiKeyId;

/// Key identifying a rate limit bucket without string allocations on the hot path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RateLimitKey {
    Key(ApiKeyId),
    Ip(std::net::IpAddr),
}

/// Configuration for the rate limiter.
#[derive(Clone)]
pub struct RateLimitConfig {
    /// Maximum requests per window per key.
    pub max_requests: u32,
    /// Window duration.
    pub window: Duration,
    /// Maximum capacity of the rate limiter's internal storage map.
    pub max_capacity: usize,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            max_requests: 60, // 60 requests per minute per key
            window: Duration::from_mins(1),
            max_capacity: 100_000,
        }
    }
}

pub trait RateLimiter: Send + Sync {
    /// Check if a request from `key` is allowed. Returns `true` if
    /// allowed, `false` if rate-limited.
    fn check(&self, key: RateLimitKey) -> bool;

    /// Remove expired entries. Call periodically to prevent unbounded
    /// growth (e.g. every 5 minutes).
    fn cleanup(&self);
}

/// A per-key rate limiter. Keyed on [`RateLimitKey`] (typically the API key id
/// or the client IP).
pub struct SlidingWindowRateLimiter {
    config: RateLimitConfig,
    /// Map of key -> (count, window_start).
    windows: Arc<DashMap<RateLimitKey, (u32, Instant)>>,
}

impl SlidingWindowRateLimiter {
    pub fn new(config: RateLimitConfig) -> Self {
        Self {
            config,
            windows: Arc::new(DashMap::new()),
        }
    }

    /// Drop the `n` window entries with the LOWEST request count, but ONLY
    /// those that have not yet tripped the rate limit (`count < max`). Only
    /// invoked when [`cleanup`] could not free enough slots — i.e. every
    /// surviving entry is still inside its window. Entries that have already
    /// tripped the rate limit (`count >= max`) are preserved so the throttle
    /// they back survives the spoofed-key flood.
    fn evict_lowest_count_windows(&self, n: usize) {
        let max = self.config.max_requests;
        let mut lowest: Vec<(u32, RateLimitKey)> = self
            .windows
            .iter()
            .filter(|e| e.value().0 < max) // never evict a throttled entry
            .map(|e| (e.value().0, *e.key()))
            .collect();
        lowest.sort_unstable_by_key(|a| a.0);
        for (_, key) in lowest.into_iter().take(n) {
            self.windows.remove(&key);
        }
    }

    /// Last-resort eviction when the map is still full after `cleanup` and
    /// `evict_lowest_count_windows` (i.e. every entry is currently
    /// throttled). We drop the OLDEST entries because they are closest to
    /// their window expiring naturally, so the unblock window is shortest.
    /// Without this the map could grow unboundedly under a sustained flood
    /// of unique keys, which is a memory DoS vector that outweighs the
    /// short-lived unblock.
    fn evict_oldest_throttled_windows(&self, n: usize) {
        let now = Instant::now();
        let mut oldest: Vec<(std::time::Duration, RateLimitKey)> = self
            .windows
            .iter()
            .map(|e| (now.duration_since(e.value().1), *e.key()))
            .collect();
        oldest.sort_unstable_by_key(|a| a.0);
        for (_, key) in oldest.into_iter().take(n) {
            self.windows.remove(&key);
        }
    }
}

impl RateLimiter for SlidingWindowRateLimiter {
    fn check(&self, key: RateLimitKey) -> bool {
        let now = Instant::now();
        let max = self.config.max_requests;
        let window = self.config.window;

        if let Some(mut entry) = self.windows.get_mut(&key) {
            let (count, start) = entry.value_mut();
            if now.duration_since(*start) >= window {
                // Window expired — reset.
                *count = 1;
                *start = now;
                return true;
            } else if *count < max {
                *count += 1;
                return true;
            }
            return false;
        }

        if self.windows.len() >= self.config.max_capacity {
            self.cleanup();
            // Security: previously this called `clear()`, which wiped the
            // counters for ACTIVE windows too — a flood of unique spoofed
            // keys would unblock keys that had already been throttled. We
            // now drop only the entries with the LOWEST count (those have
            // not tripped the rate limit and dropping them does not unblock
            // anyone). Active throttles (count >= max) are preserved.
            if self.windows.len() >= self.config.max_capacity {
                self.evict_lowest_count_windows(10_000);
                // Last resort: if every entry is currently throttled, evict
                // the oldest ones to keep the memory bound. The unblock
                // window is short (closest to natural expiry).
                if self.windows.len() >= self.config.max_capacity {
                    self.evict_oldest_throttled_windows(1_000);
                }
            }
        }

        self.windows.insert(key, (1, now));
        true
    }

    fn cleanup(&self) {
        let now = Instant::now();
        let window = self.config.window;
        self.windows
            .retain(|_, (_, start)| now.duration_since(*start) < window);
    }
}

/// Default cap on concurrently in-flight requests per rate-limit key
/// (API key or client IP).
///
/// Security (OP-03): the request-per-minute limiter only counts request
/// *starts*, so a key could otherwise hold an unbounded number of concurrent
/// requests — each retaining a parsed body, channels and an upstream
/// connection for up to the 300 s SSE stream lifetime.
pub const DEFAULT_MAX_CONCURRENT_PER_KEY: usize = 64;

/// Tracks the number of concurrently in-flight requests per
/// [`RateLimitKey`].
///
/// A request acquires a slot before the handler runs and releases it only
/// when its response body has fully finished streaming (see the
/// `InFlightBody` wrapper in the server crate), which is what bounds
/// long-lived SSE streams.
pub struct InFlightLimiter {
    max: usize,
    in_flight: Arc<DashMap<RateLimitKey, usize>>,
}

/// Releases one in-flight slot when dropped.
pub struct InFlightGuard {
    key: RateLimitKey,
    in_flight: Arc<DashMap<RateLimitKey, usize>>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        use dashmap::mapref::entry::Entry;
        match self.in_flight.entry(self.key) {
            Entry::Occupied(mut o) => {
                if *o.get() <= 1 {
                    o.remove();
                } else {
                    *o.get_mut() -= 1;
                }
            }
            Entry::Vacant(_) => {}
        }
    }
}

impl InFlightLimiter {
    pub fn new(max_concurrent_per_key: usize) -> Self {
        Self {
            max: max_concurrent_per_key.max(1),
            in_flight: Arc::new(DashMap::new()),
        }
    }

    /// Number of requests currently in flight for `key`.
    pub fn active(&self, key: RateLimitKey) -> usize {
        self.in_flight.get(&key).map_or(0, |e| *e.value())
    }

    /// Acquire one in-flight slot for `key`, or `None` when the key already
    /// holds `max_concurrent_per_key` requests.
    pub fn try_acquire(&self, key: RateLimitKey) -> Option<InFlightGuard> {
        use dashmap::mapref::entry::Entry;
        // Hard cap on distinct keys to bound memory under a spoofed-IP flood.
        // Security: previously this called `clear()` on overflow, which would
        // drop the in-flight counters for ALL keys, letting a key that had
        // already saturated its cap grab another batch. We now drop only
        // keys with zero in-flight requests (leaked entries from a panic
        // before the guard ran its Drop) — active keys are preserved.
        if self.in_flight.len() >= 100_000 {
            self.in_flight.retain(|_, count| *count > 0);
        }
        match self.in_flight.entry(key) {
            Entry::Occupied(mut o) => {
                if *o.get() >= self.max {
                    None
                } else {
                    *o.get_mut() += 1;
                    Some(InFlightGuard {
                        key,
                        in_flight: Arc::clone(&self.in_flight),
                    })
                }
            }
            Entry::Vacant(v) => {
                v.insert(1);
                Some(InFlightGuard {
                    key,
                    in_flight: Arc::clone(&self.in_flight),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inflight_limiter_caps_concurrency_and_releases() {
        let limiter = InFlightLimiter::new(3);
        let key = RateLimitKey::Key(ApiKeyId(7));

        let g1 = limiter.try_acquire(key).expect("first slot");
        let _g2 = limiter.try_acquire(key).expect("second slot");
        let g3 = limiter.try_acquire(key).expect("third slot");
        assert!(limiter.try_acquire(key).is_none(), "4th concurrent blocked");
        assert_eq!(limiter.active(key), 3);

        drop(g1);
        drop(g3);
        assert_eq!(limiter.active(key), 1);

        let _g4 = limiter.try_acquire(key).expect("slot freed after drop");
        drop(_g2);
        drop(_g4);
        assert_eq!(limiter.active(key), 0, "map entry removed at zero");
    }

    #[test]
    fn inflight_limiter_keys_are_independent() {
        let limiter = InFlightLimiter::new(1);
        let k1 = RateLimitKey::Key(ApiKeyId(1));
        let k2 = RateLimitKey::Key(ApiKeyId(2));
        let _g1 = limiter.try_acquire(k1).expect("k1 slot");
        assert!(limiter.try_acquire(k1).is_none());
        assert!(limiter.try_acquire(k2).is_some(), "k2 unaffected");
    }

    #[test]
    fn allows_up_to_limit() {
        let rl = SlidingWindowRateLimiter::new(RateLimitConfig {
            max_requests: 3,
            window: Duration::from_mins(1),
            ..Default::default()
        });
        let key = RateLimitKey::Key(ApiKeyId(1));
        assert!(rl.check(key));
        assert!(rl.check(key));
        assert!(rl.check(key));
        assert!(!rl.check(key)); // 4th request blocked
    }

    #[test]
    fn different_keys_independent() {
        let rl = SlidingWindowRateLimiter::new(RateLimitConfig {
            max_requests: 2,
            window: Duration::from_mins(1),
            ..Default::default()
        });
        let key1 = RateLimitKey::Key(ApiKeyId(1));
        let key2 = RateLimitKey::Key(ApiKeyId(2));
        assert!(rl.check(key1));
        assert!(rl.check(key1));
        assert!(!rl.check(key1)); // key1 blocked
        assert!(rl.check(key2)); // key2 still ok
        assert!(rl.check(key2));
        assert!(!rl.check(key2)); // key2 blocked
    }

    #[test]
    fn window_resets_after_expiry() {
        let rl = SlidingWindowRateLimiter::new(RateLimitConfig {
            max_requests: 1,
            window: Duration::from_millis(50),
            ..Default::default()
        });
        let key = RateLimitKey::Key(ApiKeyId(1));
        assert!(rl.check(key));
        assert!(!rl.check(key)); // blocked
        std::thread::sleep(Duration::from_millis(60));
        assert!(rl.check(key)); // window reset
    }

    #[test]
    fn bounds_memory_growth() {
        let rl = SlidingWindowRateLimiter::new(RateLimitConfig {
            max_requests: 1,
            window: Duration::from_mins(1),
            max_capacity: 2,
        });

        let key1 = RateLimitKey::Key(ApiKeyId(1));
        let key2 = RateLimitKey::Key(ApiKeyId(2));
        let key3 = RateLimitKey::Key(ApiKeyId(3));

        // Two keys within capacity
        assert!(rl.check(key1));
        assert!(rl.check(key2));
        assert_eq!(rl.windows.len(), 2);

        // The third key triggers cleanup; none expired, so the lowest-count
        // entries (key1, key2 with count=1) are evicted to make room for key3.
        assert!(rl.check(key3));
        assert_eq!(rl.windows.len(), 1); // Only key3 remains
    }

    /// Regression: a flood of unique keys MUST NOT wipe the counter of a key
    /// that was already throttled. Previously the overflow branch called
    /// `self.windows.clear()`, which let a throttled key back in after the
    /// flood crested `max_capacity`.
    #[test]
    fn throttle_survives_spoofed_key_flood() {
        let rl = SlidingWindowRateLimiter::new(RateLimitConfig {
            max_requests: 10,
            window: Duration::from_secs(60),
            max_capacity: 100,
        });

        // Trip the throttle for `blocked_key` (count reaches max=10).
        let blocked_key = RateLimitKey::Key(ApiKeyId(1));
        for _ in 0..10 {
            assert!(rl.check(blocked_key));
        }
        assert!(!rl.check(blocked_key), "blocked at budget");

        // Flood with unique keys. Each flood key has count=1 (below max=10),
        // so eviction drops THEM, not `blocked_key` (count=10 = max).
        for i in 2..200i64 {
            let k = RateLimitKey::Key(ApiKeyId(i));
            let _ = rl.check(k);
        }

        // The throttled key MUST still be throttled after the flood.
        assert!(
            !rl.check(blocked_key),
            "BLOCKED key must remain blocked after spoofed-key flood — clear() regression"
        );
    }
}
