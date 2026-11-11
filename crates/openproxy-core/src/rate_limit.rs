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
///
/// The per-key cap is a fairness bound, not a memory guard: it stops one
/// client from monopolising the global pool. It is deliberately small so a
/// single key cannot crowd out everyone else.
pub const DEFAULT_MAX_CONCURRENT_PER_KEY: usize = 10;

/// Default cap on concurrently in-flight requests across the whole proxy.
///
/// This is the memory/socket guard: every in-flight request retains a parsed
/// body, channels and (for streaming) an upstream connection for up to the
/// 300 s SSE lifetime. Sized so a burst of a few hundred concurrent clients
/// is served rather than dropped.
pub const DEFAULT_MAX_CONCURRENT_GLOBAL: usize = 512;

/// Tracks the number of concurrently in-flight requests per
/// [`RateLimitKey`].
///
/// A request acquires a slot before the handler runs and releases it only
/// when its response body has fully finished streaming (see the
/// `InFlightBody` wrapper in the server crate), which is what bounds
/// long-lived SSE streams.
pub struct InFlightLimiter {
    /// Live per-key cap. Atomic so the admin API can retune it without a
    /// restart and without tearing down the in-flight counters (a rebuild
    /// would silently release every held slot).
    max_per_key: std::sync::atomic::AtomicUsize,
    /// Live proxy-wide cap across every key.
    max_global: std::sync::atomic::AtomicUsize,
    in_flight: Arc<DashMap<RateLimitKey, usize>>,
    /// Total in-flight requests across all keys.
    global: Arc<std::sync::atomic::AtomicUsize>,
}

/// Releases one in-flight slot when dropped.
pub struct InFlightGuard {
    key: RateLimitKey,
    in_flight: Arc<DashMap<RateLimitKey, usize>>,
    global: Arc<std::sync::atomic::AtomicUsize>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        use dashmap::mapref::entry::Entry;
        self.global
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
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
        Self::with_global_limit(max_concurrent_per_key, DEFAULT_MAX_CONCURRENT_GLOBAL)
    }

    /// Construct with explicit per-key and proxy-wide caps.
    pub fn with_global_limit(max_concurrent_per_key: usize, max_concurrent_global: usize) -> Self {
        Self {
            max_per_key: std::sync::atomic::AtomicUsize::new(max_concurrent_per_key.max(1)),
            max_global: std::sync::atomic::AtomicUsize::new(max_concurrent_global.max(1)),
            in_flight: Arc::new(DashMap::new()),
            global: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// Current per-key concurrency cap.
    pub fn max_concurrent_per_key(&self) -> usize {
        self.max_per_key.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Current proxy-wide concurrency cap.
    pub fn max_concurrent_global(&self) -> usize {
        self.max_global.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Total requests currently in flight across every key.
    pub fn global_active(&self) -> usize {
        self.global.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Retune the per-key cap live. In-flight counters are preserved, so
    /// requests already holding a slot keep it; the new cap applies to the
    /// next `try_acquire`. Lowering it below the current in-flight count does
    /// not evict anything — it just refuses new slots until the count drains.
    pub fn set_max_concurrent_per_key(&self, max_concurrent_per_key: usize) {
        self.max_per_key.store(
            max_concurrent_per_key.max(1),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// Retune the proxy-wide cap live (same semantics as the per-key setter).
    pub fn set_max_concurrent_global(&self, max_concurrent_global: usize) {
        self.max_global.store(
            max_concurrent_global.max(1),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// Number of requests currently in flight for `key`.
    pub fn active(&self, key: RateLimitKey) -> usize {
        self.in_flight.get(&key).map_or(0, |e| *e.value())
    }

    /// Acquire one in-flight slot for `key`, or `None` when the key already
    /// holds `max_concurrent_per_key` requests.
    pub fn try_acquire(&self, key: RateLimitKey) -> Option<InFlightGuard> {
        use dashmap::mapref::entry::Entry;
        use std::sync::atomic::Ordering;
        let max = self.max_per_key.load(Ordering::Relaxed);
        let global_max = self.max_global.load(Ordering::Relaxed);

        // Proxy-wide guard first: no key may push the process past this
        // ceiling, however many keys are active.
        if self.global.load(Ordering::Relaxed) >= global_max {
            return None;
        }
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
                if *o.get() >= max {
                    None
                } else {
                    *o.get_mut() += 1;
                    self.global.fetch_add(1, Ordering::Relaxed);
                    Some(InFlightGuard {
                        key,
                        in_flight: Arc::clone(&self.in_flight),
                        global: Arc::clone(&self.global),
                    })
                }
            }
            Entry::Vacant(v) => {
                v.insert(1);
                self.global.fetch_add(1, Ordering::Relaxed);
                Some(InFlightGuard {
                    key,
                    in_flight: Arc::clone(&self.in_flight),
                    global: Arc::clone(&self.global),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_cap_refuses_once_the_proxy_is_full() {
        // 5 slots per key but only 3 proxy-wide: the global ceiling must win.
        let limiter = InFlightLimiter::with_global_limit(5, 3);
        let a = RateLimitKey::Key(ApiKeyId(1));
        let b = RateLimitKey::Key(ApiKeyId(2));

        let g1 = limiter.try_acquire(a).expect("1st");
        let _g2 = limiter.try_acquire(b).expect("2nd (other key)");
        let _g3 = limiter.try_acquire(a).expect("3rd");
        assert_eq!(limiter.global_active(), 3);
        // Global is full even though neither key is at its per-key cap.
        assert!(limiter.try_acquire(b).is_none(), "global ceiling blocks");
        assert!(limiter.try_acquire(a).is_none(), "global ceiling blocks");

        drop(g1);
        assert_eq!(limiter.global_active(), 2);
        let _g4 = limiter.try_acquire(b).expect("slot freed proxy-wide");
        assert_eq!(limiter.global_active(), 3);
    }

    #[test]
    fn per_key_cap_is_fairness_not_a_global_lock() {
        // 2 per key, 10 global: one busy key cannot starve another.
        let limiter = InFlightLimiter::with_global_limit(2, 10);
        let a = RateLimitKey::Key(ApiKeyId(1));
        let b = RateLimitKey::Key(ApiKeyId(2));

        let _a1 = limiter.try_acquire(a).expect("a1");
        let _a2 = limiter.try_acquire(a).expect("a2");
        assert!(limiter.try_acquire(a).is_none(), "a is at its own cap");
        // b is unaffected.
        let _b1 = limiter.try_acquire(b).expect("b1 while a is saturated");
        assert_eq!(limiter.global_active(), 3);
    }

    #[test]
    fn caps_are_retunable_live_without_dropping_in_flight() {
        let limiter = InFlightLimiter::with_global_limit(2, 4);
        let key = RateLimitKey::Key(ApiKeyId(9));
        let held = limiter.try_acquire(key).expect("held slot");

        limiter.set_max_concurrent_per_key(1);
        limiter.set_max_concurrent_global(1);
        assert_eq!(limiter.max_concurrent_per_key(), 1);
        assert_eq!(limiter.max_concurrent_global(), 1);
        // The already-held slot survives; new ones are refused.
        assert_eq!(limiter.global_active(), 1);
        assert!(limiter.try_acquire(key).is_none());

        drop(held);
        assert_eq!(limiter.global_active(), 0);
        limiter.set_max_concurrent_per_key(8);
        limiter.set_max_concurrent_global(16);
        let _again = limiter.try_acquire(key).expect("after raising the caps");
    }

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
