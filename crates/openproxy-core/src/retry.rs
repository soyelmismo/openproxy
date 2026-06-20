//! Retry policy with exponential backoff and jitter.
//! Used by the pipeline when a single target fails (race_size=1 path).

use crate::config::RetriesConfig;
use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_attempts: u8,
    pub backoff_base: Duration,
    pub backoff_factor: u8,
    pub backoff_jitter_pct: u8,
}

impl RetryPolicy {
    pub fn from_config(c: &RetriesConfig) -> Self {
        Self {
            max_attempts: c.max_attempts,
            backoff_base: Duration::from_millis(c.backoff_base_ms),
            backoff_factor: c.backoff_factor,
            backoff_jitter_pct: c.backoff_jitter_pct,
        }
    }

    /// Returns Some(duration) if we should retry after attempt N, or None if max reached.
    /// Attempt is 1-indexed: attempt 1 is the first try (no delay before).
    /// After attempt N fails, returns delay before attempt N+1.
    pub fn delay_after_attempt(&self, attempt: u8) -> Option<Duration> {
        if attempt >= self.max_attempts {
            return None;
        }
        // exp backoff: base * factor^(attempt-1) (for attempt=1 first retry, base * factor^0 = base)
        let exp = (self.backoff_factor as u64).saturating_pow((attempt - 1) as u32);
        let base = (self.backoff_base.as_millis() as u64).saturating_mul(exp);
        // jitter ±N%
        let jitter_amp = base.saturating_mul(self.backoff_jitter_pct as u64) / 100;
        let mut rng = rand::thread_rng();
        let jitter: i64 = rand::Rng::gen_range(&mut rng, -(jitter_amp as i64)..=(jitter_amp as i64));
        let total = (base as i64).saturating_add(jitter).max(0) as u64;
        Some(Duration::from_millis(total))
    }

    /// True if the error is retryable per spec §5.4:
    /// 5xx, 429, network/timeout errors. NOT 4xx (except 429).
    ///
    /// Mid-stream timeouts (idle_chunk, body, total) are NOT retryable:
    /// bytes were already sent to the client, so retrying would write
    /// a second stream on top of the first, corrupting the output.
    /// Connect/TTFT timeouts happen before any bytes reach the client,
    /// so they are safe to retry.
    pub fn is_retryable(err: &crate::error::CoreError) -> bool {
        use crate::error::CoreError::*;
        match err {
            UpstreamTimeout { phase, .. } => {
                !matches!(phase.as_str(), "idle_chunk" | "body" | "total")
            }
            UpstreamConnection(_) => true,
            RateLimited { .. } => true,
            UpstreamError { status, .. } => *status >= 500 || *status == 429,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::CoreError;

    fn policy(max: u8, base_ms: u64, factor: u8, jitter: u8) -> RetryPolicy {
        RetryPolicy {
            max_attempts: max,
            backoff_base: Duration::from_millis(base_ms),
            backoff_factor: factor,
            backoff_jitter_pct: jitter,
        }
    }

    #[test]
    fn max_attempts_3_gives_2_delays() {
        let p = policy(3, 100, 2, 0);
        assert!(p.delay_after_attempt(1).is_some());
        assert!(p.delay_after_attempt(2).is_some());
        assert!(p.delay_after_attempt(3).is_none());
    }

    #[test]
    fn backoff_grows() {
        let p = policy(5, 100, 2, 0);
        let d1 = p.delay_after_attempt(1).unwrap();
        let d2 = p.delay_after_attempt(2).unwrap();
        assert!(d2 >= d1, "expected d2 ({:?}) >= d1 ({:?})", d2, d1);
        assert_eq!(d1, Duration::from_millis(100));
        assert_eq!(d2, Duration::from_millis(200));
    }

    #[test]
    fn jitter_within_bounds() {
        // base 200ms, factor 2, jitter 50% => first delay in [100, 300]ms.
        let p = policy(5, 200, 2, 50);
        for _ in 0..200 {
            let d = p.delay_after_attempt(1).unwrap();
            let ms = d.as_millis() as u64;
            assert!(
                (100..=300).contains(&ms),
                "delay {}ms out of [100, 300] range",
                ms
            );
        }
    }

    #[test]
    fn retryable_5xx() {
        let err = CoreError::UpstreamError {
            status: 502,
            provider: "p".into(),
            model: "m".into(),
            body: "b".into(),
        };
        assert!(RetryPolicy::is_retryable(&err));
    }

    #[test]
    fn retryable_429() {
        let err = CoreError::RateLimited {
            provider: "p".into(),
            retry_after_ms: 1000,
        };
        assert!(RetryPolicy::is_retryable(&err));
    }

    #[test]
    fn not_retryable_4xx() {
        let err = CoreError::UpstreamError {
            status: 400,
            provider: "p".into(),
            model: "m".into(),
            body: "b".into(),
        };
        assert!(!RetryPolicy::is_retryable(&err));
    }

    #[test]
    fn not_retryable_validation() {
        let err = CoreError::Validation("bad input".into());
        assert!(!RetryPolicy::is_retryable(&err));
    }

    #[test]
    fn not_retryable_idle_chunk_timeout() {
        let err = CoreError::UpstreamTimeout {
            phase: "idle_chunk".into(),
            ms: 120_000,
        };
        assert!(!RetryPolicy::is_retryable(&err));
    }

    #[test]
    fn not_retryable_total_timeout() {
        let err = CoreError::UpstreamTimeout {
            phase: "total".into(),
            ms: 300_000,
        };
        assert!(!RetryPolicy::is_retryable(&err));
    }

    #[test]
    fn retryable_connect_timeout() {
        let err = CoreError::UpstreamTimeout {
            phase: "connect".into(),
            ms: 5000,
        };
        assert!(RetryPolicy::is_retryable(&err));
    }
}
