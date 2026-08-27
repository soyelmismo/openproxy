//! Simple per-key rate limiter using a sliding window.
//!
//! Not a production-grade token bucket — just a "max N requests per
//! minute per API key" guard. Uses a DashMap for O(1) lookups.
//! Entries are lazily cleaned up on insert.

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

#[cfg(test)]
mod tests {
    use super::*;

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

        // Insert first two keys, staying within capacity
        assert!(rl.check(key1));
        assert!(rl.check(key2));
        assert_eq!(rl.windows.len(), 2);

        // Third key should trigger cleanup, and since none are expired, clear the map
        assert!(rl.check(key3));
        assert_eq!(rl.windows.len(), 1); // Only key3 remains
    }
}
