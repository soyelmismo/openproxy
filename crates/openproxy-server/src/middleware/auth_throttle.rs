//! Shared per-IP failed-authentication throttle.
//!
//! Extracted from [`crate::handlers::admin::auth::AdminAuthLimiter`] (OP-27)
//! so the public `/v1` API can reuse the exact same algorithm and eviction
//! hardening, closing the gap where failed attempts against `/v1/*` ran
//! through `authenticate` (SHA-256 + SQLite lookup per attempt) without any
//! throttle: the per-IP branch of `rate_limit_middleware` is unreachable
//! pre-auth because the auth layer is the OUTERMOST route layer, and
//! `GET /v1/models` performs auth in-handler with no middleware at all.
//!
//! The limiter counts failed authentications per client IP inside a sliding
//! window. A blocked IP receives `429` until its window expires or a
//! successful authentication from that IP clears the budget.

use crate::error::ApiError;
use crate::state::AppState;
use axum::extract::State;
use openproxy_types::CoreError;

/// Max failed public API authentications per IP per window.
const PUBLIC_AUTH_MAX_FAILURES: u32 = 20;
/// Window for the public API failure counter (matches the admin throttle).
const PUBLIC_AUTH_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);
/// Upper bound on tracked IPs before eviction kicks in (spoofed-IP floods).
const MAX_TRACKED_IPS: usize = 100_000;

/// Per-IP failure budget with a sliding window.
///
/// Shared by the admin throttle (OP-27) and the public `/v1` throttle; the
/// eviction strategy is part of its security contract:
/// * expired-window entries are reaped first — dropping them cannot unblock
///   anyone because their counters had already expired;
/// * if the map is still full, the entries with the LOWEST failure counts are
///   dropped — those have not tripped the throttle, so dropping them does not
///   unblock anyone, while already-blocked IPs keep their counters.
pub struct AuthFailureLimiter {
    max_failures: u32,
    window: std::time::Duration,
    failures: dashmap::DashMap<std::net::IpAddr, (u32, std::time::Instant)>,
}

impl AuthFailureLimiter {
    pub fn new(max_failures: u32, window: std::time::Duration) -> Self {
        Self {
            max_failures,
            window,
            failures: dashmap::DashMap::new(),
        }
    }

    /// `true` when `ip` exhausted its failure budget inside the window.
    ///
    /// The budget only counts while its window is still live (root fix,
    /// audit A4-03): without the window check, a full budget blocked an IP
    /// forever, because the early 429 made `record_success` unreachable and
    /// nothing could ever reset the counter.
    pub fn is_blocked(&self, ip: std::net::IpAddr) -> bool {
        self.failures.get(&ip).is_some_and(|e| {
            let (count, start) = e.value();
            *count >= self.max_failures && start.elapsed() < self.window
        })
    }

    /// Count one failed attempt; resets the window when it expired.
    pub fn record_failure(&self, ip: std::net::IpAddr) {
        use dashmap::mapref::entry::Entry;
        if self.failures.len() >= MAX_TRACKED_IPS {
            self.evict_expired_failures();
            if self.failures.len() >= MAX_TRACKED_IPS {
                self.evict_lowest_failures(10_000);
            }
        }
        let now = std::time::Instant::now();
        match self.failures.entry(ip) {
            Entry::Occupied(mut o) => {
                let (count, start) = o.get_mut();
                if start.elapsed() >= self.window {
                    *count = 1;
                    *start = now;
                } else {
                    *count += 1;
                }
            }
            Entry::Vacant(v) => {
                v.insert((1, now));
            }
        }
    }

    /// A successful authentication clears the IP's failure budget.
    pub fn record_success(&self, ip: std::net::IpAddr) {
        self.failures.remove(&ip);
    }

    /// Drop every failure-counter entry whose window has expired.
    fn evict_expired_failures(&self) {
        let now = std::time::Instant::now();
        self.failures
            .retain(|_, (_, start)| now.duration_since(*start) < self.window);
    }

    /// Drop the `n` failure-counter entries with the LOWEST failure count.
    /// Only invoked when [`Self::evict_expired_failures`] could not free
    /// enough slots — i.e. every surviving entry is still inside its window,
    /// which only happens under a sustained spoofed-IP flood.
    fn evict_lowest_failures(&self, n: usize) {
        let mut lowest: Vec<(u32, std::net::IpAddr)> = self
            .failures
            .iter()
            .map(|e| (e.value().0, *e.key()))
            .collect();
        lowest.sort_unstable_by_key(|a| a.0);
        for (_, ip) in lowest.into_iter().take(n) {
            self.failures.remove(&ip);
        }
    }

    /// Test-only: age every failure entry past the window, as time would.
    #[cfg(test)]
    pub(crate) fn age_entries_past_window_for_test(&self) {
        let now = std::time::Instant::now();
        self.failures.iter_mut().for_each(|mut e| {
            let (_, start) = e.value_mut();
            *start = now
                .checked_sub(self.window + std::time::Duration::from_secs(1))
                .unwrap_or(now);
        });
    }
}

/// Build the `/v1` failure limiter with the public-surface policy.
pub fn public_auth_limiter() -> AuthFailureLimiter {
    AuthFailureLimiter::new(PUBLIC_AUTH_MAX_FAILURES, PUBLIC_AUTH_WINDOW)
}

/// Outermost `/v1` middleware: throttle unauthenticated brute force.
///
/// Wraps the whole public API router (including `GET /v1/models`, which
/// authenticates in-handler and has no other middleware). Requests from a
/// blocked IP receive `429` before `authenticate` runs; every `401` response
/// counts as one failure, and any `2xx` clears the budget — `/v1` has no
/// unauthenticated success path, so only a valid key can reset the counter.
///
/// Failure recording is response-driven (`401`) rather than embedded in
/// `authenticate` because the endpoint has three distinct auth entry points
/// (chat middleware, key middleware for media, in-handler auth for models)
/// and all of them answer `401` exclusively for credential rejection.
pub async fn public_auth_throttle_middleware(
    State(state): State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, ApiError> {
    // The peer address is read from the request extensions when present so
    // the middleware also works in `oneshot` tests that do not attach
    // `ConnectInfo`.
    let peer = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0);
    let client_ip = crate::client_ip::resolve_client_ip(
        req.headers(),
        peer.as_ref(),
        &state.config().server.trusted_proxies,
    )
    .or_else(|| peer.map(|p| p.ip()))
    .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));

    if state.public_auth_limiter().is_blocked(client_ip) {
        // Escape hatch: a VALID credential must not be locked out by the
        // failures of another client behind the same IP (shared NAT — the
        // failure counter is per-IP, not per-key). Verify against the
        // in-memory key cache ONLY: no SQLite, no hash lookup cost beyond
        // one SHA-256, so an attacker rotating candidate keys stays blocked
        // without touching the database, while a key that authenticated
        // successfully in the last 60 s is cached and passes. Downstream
        // middleware still performs full validation on the passed request.
        let known_good_key = crate::middleware::auth::extract_bearer_or_api_key_token(
            req.headers(),
        )
        .is_some_and(|token| {
            let key_hash = openproxy_core::api_keys::hash_key(token);
            state
                .get_cached_api_key(&key_hash)
                .is_some_and(|key| key.is_active)
        });
        if !known_good_key {
            return Err(ApiError(CoreError::RateLimited {
                provider: "public_auth_throttle".into(),
                retry_after_ms: 60_000,
                is_proxy_rotated: false,
            }));
        }
    }

    let response = next.run(req).await;
    if response.status() == axum::http::StatusCode::UNAUTHORIZED {
        state.public_auth_limiter().record_failure(client_ip);
    } else if response.status().is_success() {
        state.public_auth_limiter().record_success(client_ip);
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_blocks_at_threshold_and_success_clears() {
        let limiter = AuthFailureLimiter::new(3, std::time::Duration::from_secs(60));
        let ip: std::net::IpAddr = "203.0.113.9".parse().unwrap();
        for _ in 0..2 {
            limiter.record_failure(ip);
        }
        assert!(!limiter.is_blocked(ip));
        limiter.record_failure(ip);
        assert!(limiter.is_blocked(ip), "blocked at budget");
        limiter.record_success(ip);
        assert!(!limiter.is_blocked(ip), "success clears the budget");
    }

    #[test]
    fn expired_window_no_longer_blocks() {
        let limiter = AuthFailureLimiter::new(1, std::time::Duration::from_millis(20));
        let ip: std::net::IpAddr = "203.0.113.10".parse().unwrap();
        limiter.record_failure(ip);
        assert!(limiter.is_blocked(ip));
        std::thread::sleep(std::time::Duration::from_millis(40));
        assert!(!limiter.is_blocked(ip), "window expiry unblocks");
    }
}
