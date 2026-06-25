//! Per-account circuit breaker. In-memory, per-process.

use crate::config::CircuitBreakerConfig;
use crate::ids::AccountId;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

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

/// Outcome of a single [`CircuitBreakerRegistry::record_failure_outcome`]
/// call. Exposes the post-call health plus the transition flag so the
/// caller can fire a `circuit_open` notification exactly once per
/// closed→open transition (without re-surfacing it on every subsequent
/// failure that re-affirms the open state).
///
/// `consecutive_failures` and `threshold` are surfaced so the
/// notification body can render a "{{failures}}/{{threshold}} failures"
/// string without the caller having to thread the config through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailureOutcome {
    /// Post-call health (the same value returned by
    /// [`CircuitBreakerRegistry::record_failure`]).
    pub health: Health,
    /// `true` iff THIS call transitioned the breaker from
    /// non-`Unhealthy` to `Unhealthy`. Subsequent failures that
    /// re-affirm the open state leave this `false`.
    pub just_opened: bool,
    /// Consecutive failure count after this call (clamped at the
    /// threshold; never decreases on a `record_failure` call).
    pub consecutive_failures: u8,
    /// Configured failure threshold (constant for the lifetime of the
    /// registry). Mirrors `CircuitBreakerConfig::failure_threshold`.
    pub threshold: u8,
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
        self.record_failure_outcome(account).health
    }

    /// Same as [`record_failure`](Self::record_failure) but also reports
    /// whether THIS call was the one that transitioned the breaker to
    /// [`Health::Unhealthy`], plus the current failure count and
    /// threshold. Used by the pipeline to fire a `circuit_open`
    /// notification exactly once per closed→open transition (the naive
    /// "compare `is_healthy` before and after" approach has a
    /// check-then-act race against concurrent callers).
    pub fn record_failure_outcome(&self, account: AccountId) -> FailureOutcome {
        let mut g = self.inner.lock();
        let entry = g.entry(account).or_insert(AccountBreaker {
            consecutive_failures: 0,
            state: Health::Healthy,
            unhealthy_until: None,
        });
        entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
        let just_opened =
            if entry.consecutive_failures >= self.threshold && entry.state != Health::Unhealthy {
                entry.state = Health::Unhealthy;
                entry.unhealthy_until = Some(Instant::now() + self.unhealthy_duration);
                true
            } else {
                false
            };
        FailureOutcome {
            health: entry.state,
            just_opened,
            consecutive_failures: entry.consecutive_failures,
            threshold: self.threshold,
        }
    }

    /// Test helper: force an account unhealthy now.
    #[cfg(test)]
    pub fn force_unhealthy(&self, account: AccountId) {
        let mut g = self.inner.lock();
        g.insert(
            account,
            AccountBreaker {
                consecutive_failures: self.threshold,
                state: Health::Unhealthy,
                unhealthy_until: Some(Instant::now() + self.unhealthy_duration),
            },
        );
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

    #[test]
    fn record_failure_outcome_reports_transition() {
        // The `just_opened` flag must fire EXACTLY once per
        // closed→open transition — on the call that crosses the
        // threshold. Subsequent failures that re-affirm the open
        // state must NOT set it again (otherwise the pipeline would
        // spam a `circuit_open` notification on every retryable
        // failure while the breaker is open).
        let reg = CircuitBreakerRegistry::new(&cfg(3, 60_000));
        for _ in 0..2 {
            let o = reg.record_failure_outcome(AccountId(1));
            assert!(!o.just_opened, "below threshold must not open");
            assert_eq!(o.health, Health::Healthy);
        }
        // 3rd failure crosses the threshold.
        let opened = reg.record_failure_outcome(AccountId(1));
        assert!(opened.just_opened);
        assert_eq!(opened.health, Health::Unhealthy);
        assert_eq!(opened.consecutive_failures, 3);
        assert_eq!(opened.threshold, 3);
        // 4th failure re-affirms the open state — must NOT re-fire.
        let still_open = reg.record_failure_outcome(AccountId(1));
        assert!(!still_open.just_opened);
        assert_eq!(still_open.health, Health::Unhealthy);
        assert_eq!(still_open.consecutive_failures, 4);
    }

    #[test]
    fn record_failure_outcome_reopens_after_cooldown() {
        // After the cooldown elapses and `is_healthy` flips back to
        // Healthy, a fresh threshold-crossing failure must fire
        // `just_opened` again. This is the "circuit opens, closes,
        // opens again within 24h" case the per-account dedup key
        // is designed to collapse in the notifications tray.
        let reg = CircuitBreakerRegistry::new(&cfg(2, 1));
        let o1 = reg.record_failure_outcome(AccountId(1));
        assert!(!o1.just_opened);
        let o2 = reg.record_failure_outcome(AccountId(1));
        assert!(o2.just_opened);
        // Cooldown elapses; `is_healthy` resets the counter.
        thread::sleep(StdDuration::from_millis(10));
        assert_eq!(reg.is_healthy(AccountId(1)), Health::Healthy);
        // Re-open after cooldown.
        let o3 = reg.record_failure_outcome(AccountId(1));
        assert!(!o3.just_opened);
        let o4 = reg.record_failure_outcome(AccountId(1));
        assert!(o4.just_opened, "re-opening after cooldown must re-fire");
    }
}
