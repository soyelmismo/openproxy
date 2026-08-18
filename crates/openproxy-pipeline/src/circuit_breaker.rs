use openproxy_types::combos::ComboTarget;
use openproxy_types::config::CircuitBreakerConfig;
use openproxy_types::ids::{AccountId, ModelRowId};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CircuitBreakerKey {
    Account(AccountId),
    Model(AccountId, ModelRowId),
}

impl CircuitBreakerKey {
    pub fn from_target(
        aid: AccountId,
        scope: openproxy_types::providers::RateLimitScope,
        model_row_id: Option<ModelRowId>,
    ) -> Self {
        if scope == openproxy_types::providers::RateLimitScope::Model {
            CircuitBreakerKey::Model(aid, model_row_id.unwrap_or(ModelRowId(0)))
        } else {
            CircuitBreakerKey::Account(aid)
        }
    }
}

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
    last_activity_ms: u64,
}

fn now_ms() -> u64 {
    use std::sync::LazyLock;
    static START: LazyLock<Instant> = LazyLock::new(Instant::now);
    Instant::now().duration_since(*START).as_millis() as u64
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailureOutcome {
    pub health: Health,
    pub just_opened: bool,
    pub consecutive_failures: u8,
    pub threshold: u8,
}

#[derive(Clone)]
pub struct CircuitBreakerRegistry {
    inner: Arc<Mutex<HashMap<CircuitBreakerKey, AccountBreaker>>>,
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

    pub fn is_healthy(&self, account: CircuitBreakerKey) -> Health {
        let mut g = self.inner.lock();
        let entry = g.entry(account).or_insert_with(|| AccountBreaker {
            consecutive_failures: 0,
            state: Health::Healthy,
            unhealthy_until: None,
            last_activity_ms: now_ms(),
        });
        if entry.state == Health::Unhealthy
            && let Some(until) = entry.unhealthy_until
            && Instant::now() >= until
        {
            entry.state = Health::Healthy;
            entry.consecutive_failures = 0;
            entry.unhealthy_until = None;
        }
        entry.last_activity_ms = now_ms();
        entry.state
    }

    pub fn is_target_healthy(&self, target: &ComboTarget) -> bool {
        match target.account_id {
            Some(aid) => {
                let key = CircuitBreakerKey::from_target(
                    aid,
                    target.rate_limit_scope,
                    target.model_row_id,
                );
                self.is_healthy(key) == Health::Healthy
            }
            None => true,
        }
    }

    pub fn record_success(&self, account: CircuitBreakerKey) {
        let mut g = self.inner.lock();
        if let Some(entry) = g.get_mut(&account) {
            entry.consecutive_failures = 0;
            entry.state = Health::Healthy;
            entry.unhealthy_until = None;
            entry.last_activity_ms = now_ms();
        }
    }

    pub fn record_failure(&self, account: CircuitBreakerKey) -> Health {
        self.record_failure_outcome(account).health
    }

    pub fn record_failure_outcome(&self, account: CircuitBreakerKey) -> FailureOutcome {
        let mut g = self.inner.lock();
        let entry = g.entry(account).or_insert_with(|| AccountBreaker {
            consecutive_failures: 0,
            state: Health::Healthy,
            unhealthy_until: None,
            last_activity_ms: now_ms(),
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
        entry.last_activity_ms = now_ms();
        FailureOutcome {
            health: entry.state,
            just_opened,
            consecutive_failures: entry.consecutive_failures,
            threshold: self.threshold,
        }
    }

    #[cfg(test)]
    pub fn force_unhealthy(&self, account: CircuitBreakerKey) {
        let mut g = self.inner.lock();
        g.insert(
            account,
            AccountBreaker {
                consecutive_failures: self.threshold,
                state: Health::Unhealthy,
                unhealthy_until: Some(Instant::now() + self.unhealthy_duration),
                last_activity_ms: now_ms(),
            },
        );
    }

    pub fn prune_idle(&self, max_idle: Duration) -> usize {
        let mut g = self.inner.lock();
        let cutoff = now_ms().saturating_sub(max_idle.as_millis() as u64);
        let now = Instant::now();
        let before = g.len();
        g.retain(|_, e| {
            let is_actively_unhealthy =
                e.state == Health::Unhealthy && e.unhealthy_until.is_some_and(|until| now < until);
            is_actively_unhealthy || e.last_activity_ms >= cutoff
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_circuit_breaker_transitions() {
        let config = CircuitBreakerConfig {
            failure_threshold: 3,
            unhealthy_duration_ms: 100,
        };
        let cb = CircuitBreakerRegistry::new(&config);
        let key = CircuitBreakerKey::Account(AccountId(1));

        assert_eq!(cb.is_healthy(key), Health::Healthy);

        assert_eq!(cb.record_failure(key), Health::Healthy);
        assert_eq!(cb.record_failure(key), Health::Healthy);

        // 3rd failure triggers Unhealthy
        let outcome = cb.record_failure_outcome(key);
        assert_eq!(outcome.health, Health::Unhealthy);
        assert!(outcome.just_opened);

        assert_eq!(cb.is_healthy(key), Health::Unhealthy);

        // Sleep to let it recover
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(cb.is_healthy(key), Health::Healthy);
    }

    #[test]
    fn test_prune_idle_unhealthy_expired() {
        let config = CircuitBreakerConfig {
            failure_threshold: 1,
            unhealthy_duration_ms: 20,
        };
        let cb = CircuitBreakerRegistry::new(&config);
        let key = CircuitBreakerKey::Account(AccountId(10));

        assert_eq!(cb.record_failure(key), Health::Unhealthy);
        assert_eq!(cb.len(), 1);

        // Immediately, it's actively unhealthy, so prune_idle should not prune it
        assert_eq!(cb.prune_idle(Duration::from_millis(0)), 0);
        assert_eq!(cb.len(), 1);

        // Wait for unhealthy duration to expire
        std::thread::sleep(Duration::from_millis(30));

        // Now that unhealthy_until has expired and activity is older than max_idle (0ms), it gets pruned
        assert_eq!(cb.prune_idle(Duration::from_millis(0)), 1);
        assert_eq!(cb.len(), 0);
    }

    #[test]
    fn test_is_target_healthy() {
        let config = CircuitBreakerConfig {
            failure_threshold: 1,
            unhealthy_duration_ms: 100,
        };
        let cb = CircuitBreakerRegistry::new(&config);

        let target_no_acc = ComboTarget {
            id: openproxy_types::ids::ComboTargetId(1),
            combo_id: openproxy_types::ids::ComboId(1),
            provider_id: openproxy_types::ids::ProviderId::new("openai"),
            account_id: None,
            model_row_id: None,
            sub_combo_id: None,
            priority_order: 0,
            weight: 1,
            active: true,
            rate_limit_scope: openproxy_types::providers::RateLimitScope::Account,
            cooldown_mode: None,
            cooldown_base_secs: None,
            cooldown_max_secs: None,
            cooldown_factor: None,
        };
        assert!(cb.is_target_healthy(&target_no_acc));

        let mut target_acc = target_no_acc;
        target_acc.account_id = Some(AccountId(42));
        assert!(cb.is_target_healthy(&target_acc));

        let key = CircuitBreakerKey::Account(AccountId(42));
        cb.record_failure(key);
        assert!(!cb.is_target_healthy(&target_acc));
    }
}
