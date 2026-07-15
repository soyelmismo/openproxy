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
        let mut rng = rand::rng();
        let jitter: i64 =
            rand::RngExt::random_range(&mut rng, -(jitter_amp as i64)..=(jitter_amp as i64));
        let total = (base as i64).saturating_add(jitter).max(0) as u64;
        Some(Duration::from_millis(total))
    }

    /// True if the error is retryable per spec §5.4.
    pub fn is_retryable(err: &openproxy_types::error::CoreError, idle_chunk_retryable: bool) -> bool {
        use openproxy_types::error::CoreError::*;
        match err {
            UpstreamTimeout { phase, .. } => match phase.as_str() {
                // idle_chunk is the ONLY switchable exception. It
                // fires mid-stream and retrying would corrupt the
                // output, so it's gated by `idle_chunk_retryable`.
                "idle_chunk" => idle_chunk_retryable,
                // ALL other timeout phases (dns, dial, tls, write,
                // headers, body, total) are retryable.
                _ => true,
            },
            UpstreamConnection(_) => {
                let msg = err.to_string();
                !msg.starts_with("client disconnected")
            }
            RateLimited { .. } => true,
            UpstreamError { .. } => true,
            ClientDisconnected => false,
            RaceLost => false,
            _ => true,
        }
    }
}
