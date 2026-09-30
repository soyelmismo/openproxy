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
            if self.windows.len() >= self.config.max_capacity {
                self.windows.clear();
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
        self.in_flight.get(&key).map(|e| *e.value()).unwrap_or(0)
    }

    /// Acquire one in-flight slot for `key`, or `None` when the key already
    /// holds `max_concurrent_per_key` requests.
    pub fn try_acquire(&self, key: RateLimitKey) -> Option<InFlightGuard> {
        use dashmap::mapref::entry::Entry;
        // Hard cap on distinct keys to bound memory under a spoofed-IP flood.
        if self.in_flight.len() >= 100_000 {
            self.in_flight.clear();
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

        // The third key triggers cleanup; none expired, so the map is cleared
        assert!(rl.check(key3));
        assert_eq!(rl.windows.len(), 1); // Only key3 remains
    }
}
