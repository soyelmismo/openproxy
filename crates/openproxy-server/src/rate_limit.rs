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
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            max_requests: 60, // 60 requests per minute per key
            window: Duration::from_mins(1),
        }
    }
}

/// A per-key rate limiter. Keyed on [`RateLimitKey`] (typically the API key id
/// or the client IP).
pub struct RateLimiter {
    config: RateLimitConfig,
    /// Map of key -> (count, window_start).
    windows: Arc<DashMap<RateLimitKey, (u32, Instant)>>,
}

impl RateLimiter {
    pub fn new(config: RateLimitConfig) -> Self {
        Self {
            config,
            windows: Arc::new(DashMap::new()),
        }
    }

    /// Check if a request from `key` is allowed. Returns `true` if
    /// allowed, `false` if rate-limited.
    pub fn check(&self, key: RateLimitKey) -> bool {
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

        self.windows.insert(key, (1, now));
        true
    }

    /// Remove expired entries. Call periodically to prevent unbounded
    /// growth (e.g. every 5 minutes).
    pub fn cleanup(&self) {
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
        let rl = RateLimiter::new(RateLimitConfig {
            max_requests: 3,
            window: Duration::from_mins(1),
        });
        let key = RateLimitKey::Key(ApiKeyId(1));
        assert!(rl.check(key));
        assert!(rl.check(key));
        assert!(rl.check(key));
        assert!(!rl.check(key)); // 4th request blocked
    }

    #[test]
    fn different_keys_independent() {
        let rl = RateLimiter::new(RateLimitConfig {
            max_requests: 2,
            window: Duration::from_mins(1),
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
        let rl = RateLimiter::new(RateLimitConfig {
            max_requests: 1,
            window: Duration::from_millis(50),
        });
        let key = RateLimitKey::Key(ApiKeyId(1));
        assert!(rl.check(key));
        assert!(!rl.check(key)); // blocked
        std::thread::sleep(Duration::from_millis(60));
        assert!(rl.check(key)); // window reset
    }
}
