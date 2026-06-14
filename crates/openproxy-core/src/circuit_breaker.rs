//! Per-account circuit breaker. In-memory, per-process.

use crate::config::CircuitBreakerConfig;
use crate::ids::AccountId;
use std::sync::Arc;
use std::time::{Duration, Instant};
use parking_lot::Mutex;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Healthy,
    Degraded,
    Unhealthy,
}

#[derive(Debug)]
struct AccountBreaker {
    consecutive_failures: u8,
    state: Health,
    unhealthy_until: Option<Instant>,
}

#[derive(Clone)]
pub struct CircuitBreakerRegistry {
    inner: Arc<Mutex<HashMap<AccountId, AccountBreaker>>>,
    threshold: u8,
    unhealthy_duration: Duration,
}

impl CircuitBreakerRegistry {
    pub fn new(config: &CircuitBreakerConfig) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            threshold: config.failure_threshold,
            unhealthy_duration: Duration::from_millis(config.unhealthy_duration_ms),
        }
    }

    /// Check if account is eligible. If unhealthy and cooldown passed, transition to healthy.
    pub fn is_healthy(&self, account: AccountId) -> Health {
        let mut g = self.inner.lock();
        let entry = g.entry(account).or_insert(AccountBreaker {
            consecutive_failures: 0,
            state: Health::Healthy,
            unhealthy_until: None,
        });
        if entry.state == Health::Unhealthy {
            if let Some(until) = entry.unhealthy_until {
                if Instant::now() >= until {
                    entry.state = Health::Healthy;
                    entry.consecutive_failures = 0;
                    entry.unhealthy_until = None;
                }
            }
        }
        entry.state
    }

    /// Record a successful request: reset failures, healthy.
    pub fn record_success(&self, account: AccountId) {
        let mut g = self.inner.lock();
        if let Some(entry) = g.get_mut(&account) {
            entry.consecutive_failures = 0;
            entry.state = Health::Healthy;
            entry.unhealthy_until = None;
        }
    }

    /// Record a failure: increment counter, mark unhealthy if threshold reached.
    pub fn record_failure(&self, account: AccountId) -> Health {
        let mut g = self.inner.lock();
        let entry = g.entry(account).or_insert(AccountBreaker {
            consecutive_failures: 0,
            state: Health::Healthy,
            unhealthy_until: None,
        });
        entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
        if entry.consecutive_failures >= self.threshold {
            entry.state = Health::Unhealthy;
            entry.unhealthy_until = Some(Instant::now() + self.unhealthy_duration);
        }
        entry.state
    }

    /// Test helper: force an account unhealthy now.
    #[cfg(test)]
    pub fn force_unhealthy(&self, account: AccountId) {
        let mut g = self.inner.lock();
        g.insert(account, AccountBreaker {
            consecutive_failures: self.threshold,
            state: Health::Unhealthy,
            unhealthy_until: Some(Instant::now() + self.unhealthy_duration),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration as StdDuration;

    fn cfg(threshold: u8, unhealthy_ms: u64) -> CircuitBreakerConfig {
        CircuitBreakerConfig {
            failure_threshold: threshold,
            unhealthy_duration_ms: unhealthy_ms,
        }
    }

    #[test]
    fn new_account_is_healthy() {
        let reg = CircuitBreakerRegistry::new(&cfg(5, 60_000));
        assert_eq!(reg.is_healthy(AccountId(1)), Health::Healthy);
    }

    #[test]
    fn failures_below_threshold_stay_healthy() {
        let reg = CircuitBreakerRegistry::new(&cfg(5, 60_000));
        for _ in 0..4 {
            assert_eq!(reg.record_failure(AccountId(1)), Health::Healthy);
        }
        assert_eq!(reg.is_healthy(AccountId(1)), Health::Healthy);
    }

    #[test]
    fn failures_reach_threshold_makes_unhealthy() {
        let reg = CircuitBreakerRegistry::new(&cfg(5, 60_000));
        for _ in 0..4 {
            assert_eq!(reg.record_failure(AccountId(1)), Health::Healthy);
        }
        assert_eq!(reg.record_failure(AccountId(1)), Health::Unhealthy);
        assert_eq!(reg.is_healthy(AccountId(1)), Health::Unhealthy);
    }

    #[test]
    fn unhealthy_cooldown_transitions_to_healthy() {
        let reg = CircuitBreakerRegistry::new(&cfg(5, 1));
        reg.force_unhealthy(AccountId(1));
        assert_eq!(reg.is_healthy(AccountId(1)), Health::Unhealthy);
        thread::sleep(StdDuration::from_millis(10));
        assert_eq!(reg.is_healthy(AccountId(1)), Health::Healthy);
    }

    #[test]
    fn record_success_resets_counter() {
        let reg = CircuitBreakerRegistry::new(&cfg(5, 60_000));
        for _ in 0..4 {
            reg.record_failure(AccountId(1));
        }
        reg.record_success(AccountId(1));
        for _ in 0..4 {
            assert_eq!(reg.record_failure(AccountId(1)), Health::Healthy);
        }
        assert_eq!(reg.is_healthy(AccountId(1)), Health::Healthy);
    }

    #[test]
    fn multiple_accounts_independent() {
        let reg = CircuitBreakerRegistry::new(&cfg(5, 60_000));
        reg.force_unhealthy(AccountId(1));
        assert_eq!(reg.is_healthy(AccountId(1)), Health::Unhealthy);
        assert_eq!(reg.is_healthy(AccountId(2)), Health::Healthy);
    }
}
