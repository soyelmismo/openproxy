//! Retry policy with exponential backoff and jitter.
//! Used by the pipeline when a single target fails (race_size=1 path).

use openproxy_types::config::RetriesConfig;
use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_attempts: u8,
    pub backoff_base: Duration,
    pub backoff_factor: u8,
    pub backoff_jitter_pct: u8,
    pub idle_chunk_retryable: bool,
}

impl RetryPolicy {
    pub fn from_config(c: &RetriesConfig) -> Self {
        Self {
            max_attempts: c.max_attempts,
            backoff_base: Duration::from_millis(c.backoff_base_ms),
            backoff_factor: c.backoff_factor,
            backoff_jitter_pct: c.backoff_jitter_pct,
            idle_chunk_retryable: c.idle_chunk_retryable,
        }
    }

    /// Delay before attempt N+1, or `None` once `max_attempts` is reached.
    /// `attempt` is 1-indexed (attempt 1 is the first try, no delay before).
    pub fn delay_after_attempt(&self, attempt: u8) -> Option<Duration> {
        if attempt >= self.max_attempts {
            return None;
        }
        // base * factor^(attempt-1): attempt 1 waits `base`, attempt 2 `base * factor`.
        let exp = u64::from(self.backoff_factor).saturating_pow(u32::from(attempt - 1));
        let base = (self.backoff_base.as_millis() as u64).saturating_mul(exp);
        let jitter_amp = base.saturating_mul(u64::from(self.backoff_jitter_pct)) / 100;
        let mut rng = rand::rng();
        let jitter: i64 =
            rand::RngExt::random_range(&mut rng, -(jitter_amp as i64)..=(jitter_amp as i64));
        let total = (base as i64).saturating_add(jitter).max(0) as u64;
        Some(Duration::from_millis(total))
    }
}

fn is_timeout_retryable(phase: &str, idle_chunk_retryable: bool) -> bool {
    if phase == "idle_chunk" {
        idle_chunk_retryable
    } else {
        true
    }
}

impl RetryPolicy {
    /// True if the error is retryable per spec §5.4.
    pub fn is_retryable(
        err: &openproxy_types::error::CoreError,
        idle_chunk_retryable: bool,
    ) -> bool {
        use openproxy_types::error::CoreError::{
            Cancelled, RaceLost, RateLimited, UpstreamConnection, UpstreamError, UpstreamTimeout,
        };
        match err {
            UpstreamError { provider, .. } if provider == "zai" => false,
            UpstreamTimeout { phase, .. } => is_timeout_retryable(phase, idle_chunk_retryable),
            UpstreamConnection(_) => !err.to_string().starts_with("client disconnected"),
            RateLimited {
                is_proxy_rotated, ..
            } => *is_proxy_rotated,
            UpstreamError {
                is_hard_skip: true, ..
            } => false,
            UpstreamError { status, .. } => *status != 429 && *status != 400,
            Cancelled(_) | RaceLost => false,
            _ => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_retry_policy_delay() {
        let config = RetriesConfig {
            max_attempts: 3,
            backoff_base_ms: 100,
            backoff_factor: 2,
            backoff_jitter_pct: 0, // no jitter for deterministic test
            idle_chunk_retryable: false,
            combo_max_attempts: 1,
        };
        let policy = RetryPolicy::from_config(&config);

        // 0% jitter configured, so the schedule is exact: 100ms, 200ms, then stop.
        assert_eq!(
            policy.delay_after_attempt(1),
            Some(Duration::from_millis(100))
        );
        assert_eq!(
            policy.delay_after_attempt(2),
            Some(Duration::from_millis(200))
        );
        assert_eq!(policy.delay_after_attempt(3), None);
    }
}
