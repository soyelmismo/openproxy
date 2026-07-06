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
    ///
    /// ## Policy (revised)
    ///
    /// **Almost everything is retryable.** The only exception is
    /// `idle_chunk` timeouts, which are gated by the `idle_chunk_retryable`
    /// switch (default: false). This is because idle_chunk fires
    /// mid-stream (after bytes were already sent to the client), so
    /// retrying would write a second stream on top of the first.
    ///
    /// All other errors — connect timeouts, TTFT timeouts, total
    /// timeouts, connection errors, rate limits, 4xx, 5xx — are
    /// retryable. The combo walk's per-target retry loop will retry
    /// the same target up to `max_attempts` times, then fall through
    /// to the next target. This ensures every target in the combo
    /// gets its full retry budget before the request fails.
    ///
    /// ### Why even 4xx is retryable
    ///
    /// A 400 from one provider (e.g. MiniMax's "(2013) tool call
    /// result does not follow tool call") is a provider-specific
    /// validation error. The next target in the combo (e.g. a
    /// different provider) may accept the same request just fine.
    /// Treating 4xx as non-retryable would abort the per-target
    /// retry loop early AND, more importantly, give the operator
    /// the false impression that the whole combo is broken when
    /// only one provider rejected the request.
    ///
    /// ### Mid-stream safety
    ///
    /// `body` and `total` timeouts are retryable here because the
    /// pipeline's streaming dispatch only calls `record_and_fail`
    /// (which returns to the combo walk) when the error occurs
    /// BEFORE any byte was sent to the client. If bytes were
    /// already sent, the streaming loop's failure path records
    /// the error and returns a `PipelineResult` with `error: Some(_)`,
    /// but the combo walk's per-target retry loop checks
    /// `is_retryable` BEFORE deciding to retry — and a mid-stream
    /// error would have already been recorded as a partial response,
    /// so the retry would produce a second stream. The streaming
    /// dispatch handles this correctly by NOT returning to the
    /// combo walk after bytes were sent; it returns the error
    /// directly to the client.
    ///
    /// `idle_chunk` is the exception because it specifically fires
    /// mid-stream (after content chunks were sent), and the
    /// `idle_chunk_retryable` switch lets the operator decide
    /// whether to treat it as retryable (fall through to next
    /// target) or as a hard failure (abort the walk).
    pub fn is_retryable(err: &crate::error::CoreError, idle_chunk_retryable: bool) -> bool {
        use crate::error::CoreError::*;
        match err {
            UpstreamTimeout { phase, .. } => match phase.as_str() {
                // idle_chunk is the ONLY switchable exception. It
                // fires mid-stream and retrying would corrupt the
                // output, so it's gated by `idle_chunk_retryable`.
                "idle_chunk" => idle_chunk_retryable,
                // ALL other timeout phases (dns, dial, tls, write,
                // headers, body, total) are retryable. The streaming
                // dispatch ensures body/total timeouts that fire
                // mid-stream do not return to the combo walk.
                _ => true,
            },
            UpstreamConnection(_) => {
                // UpstreamConnection is normally retryable, BUT if
                // the error message starts with "client disconnected"
                // it means the SSE channel was closed (the client or
                // proxy dropped the connection). In that case, ALL
                // subsequent targets will also fail because the sink
                // is closed — retrying is pointless. Return false to
                // abort the combo walk immediately.
                let msg = err.to_string();
                !msg.starts_with("client disconnected")
            }
            RateLimited { .. } => true,
            // 4xx is retryable: a provider-specific validation error
            // (e.g. MiniMax 2013) should not abort the whole combo.
            // The next provider may accept the same request.
            UpstreamError { .. } => true,
            // ClientDisconnected is NOT retryable — the client is
            // gone, there's no point trying more targets.
            ClientDisconnected => false,
            // RaceLost is NOT retryable — another target already won.
            RaceLost => false,
            // Everything else (internal errors, config errors, etc.)
            // is retryable to be safe — the combo walk will exhaust
            // all targets before surfacing the error.
            _ => true,
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
            idle_chunk_retryable: false,
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
        assert!(RetryPolicy::is_retryable(&err, false));
    }

    #[test]
    fn retryable_429() {
        let err = CoreError::RateLimited {
            provider: "p".into(),
            retry_after_ms: 1000,
        };
        assert!(RetryPolicy::is_retryable(&err, false));
    }

    #[test]
    fn retryable_4xx() {
        // 4xx is now retryable: a provider-specific validation error
        // (e.g. MiniMax 2013) should not abort the whole combo.
        // The next provider may accept the same request.
        let err = CoreError::UpstreamError {
            status: 400,
            provider: "p".into(),
            model: "m".into(),
            body: "b".into(),
        };
        assert!(
            RetryPolicy::is_retryable(&err, false),
            "4xx errors must be retryable so the combo walk tries the next target"
        );
    }

    #[test]
    fn not_retryable_client_disconnected() {
        // ClientDisconnected is NOT retryable — the client is gone.
        let err = CoreError::ClientDisconnected;
        assert!(!RetryPolicy::is_retryable(&err, false));
    }

    #[test]
    fn not_retryable_race_lost() {
        // RaceLost is NOT retryable — another target already won.
        let err = CoreError::RaceLost;
        assert!(!RetryPolicy::is_retryable(&err, false));
    }

    #[test]
    fn not_retryable_idle_chunk_timeout() {
        let err = CoreError::UpstreamTimeout {
            phase: "idle_chunk".into(),
            ms: 120_000,
        };
        assert!(!RetryPolicy::is_retryable(&err, false));
    }

    #[test]
    fn retryable_idle_chunk_when_configured() {
        let err = CoreError::UpstreamTimeout {
            phase: "idle_chunk".into(),
            ms: 120_000,
        };
        assert!(RetryPolicy::is_retryable(&err, true));
    }

    #[test]
    fn retryable_total_timeout() {
        // total timeout is now retryable — the streaming dispatch
        // ensures mid-stream errors don't return to the combo walk.
        let err = CoreError::UpstreamTimeout {
            phase: "total".into(),
            ms: 300_000,
        };
        assert!(
            RetryPolicy::is_retryable(&err, false),
            "total timeout must be retryable so the combo walk tries the next target"
        );
    }

    #[test]
    fn retryable_body_timeout() {
        // body timeout is now retryable — same rationale as total.
        let err = CoreError::UpstreamTimeout {
            phase: "body".into(),
            ms: 10_000,
        };
        assert!(
            RetryPolicy::is_retryable(&err, false),
            "body timeout must be retryable so the combo walk tries the next target"
        );
    }

    #[test]
    fn retryable_connect_timeout() {
        let err = CoreError::UpstreamTimeout {
            phase: "connect".into(),
            ms: 5000,
        };
        assert!(RetryPolicy::is_retryable(&err, false));
    }

    #[test]
    fn from_config_mapping() {
        let cfg = RetriesConfig {
            max_attempts: 4,
            combo_max_attempts: 5,
            backoff_base_ms: 150,
            backoff_factor: 3,
            backoff_jitter_pct: 20,
            idle_chunk_retryable: true,
        };
        let p = RetryPolicy::from_config(&cfg);
        assert_eq!(p.max_attempts, 4);
        assert_eq!(p.backoff_base, Duration::from_millis(150));
        assert_eq!(p.backoff_factor, 3);
        assert_eq!(p.backoff_jitter_pct, 20);
        assert!(p.idle_chunk_retryable);
    }
}
