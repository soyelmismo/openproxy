use openproxy_types::config::CircuitBreakerConfig;
use openproxy_types::ids::{AccountId, ModelRowId};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CircuitBreakerKey {
    Account(AccountId),
    Model(AccountId, ModelRowId),
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
    use std::sync::OnceLock;
    static START: OnceLock<Instant> = OnceLock::new();
    let start = *START.get_or_init(Instant::now);
    Instant::now().duration_since(start).as_millis() as u64
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
        let before = g.len();
        g.retain(|_, e| {
            e.state == Health::Unhealthy || e.last_activity_ms >= cutoff
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
