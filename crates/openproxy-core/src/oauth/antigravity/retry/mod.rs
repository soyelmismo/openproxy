//! `invalid_grant` retry loop + per-account `OnUnhealthyCell` callback.
//!
//! The loop is generic over the async operation so unit tests can
//! supply a closure that returns synthetic `Err(invalid_grant)` or
//! `Ok` without touching the network.

use crate::error::{CoreError, Result};
use crate::ids::AccountId;
use crate::oauth::TokenResponse;

use super::counters::{ANTIGRAVITY_BACKOFF_MS, ANTIGRAVITY_INVALID_GRANT_THRESHOLD, bump, reset};

/// Pure helper that drives the `invalid_grant` retry loop. Generic
/// over the async operation so unit tests can supply a closure that
/// returns synthetic `Err(invalid_grant)` or `Ok` without touching
/// the network.
///
/// Behavior (per GAP-5 spec §3):
///
/// 1. Attempt the operation up to `ANTIGRAVITY_INVALID_GRANT_THRESHOLD`
///    times (3 attempts total: initial + 2 retries).
/// 2. On `Ok`, reset the counter to 0 and return the token.
/// 3. On `Err`:
///    * If the error message contains `"invalid_grant"`, increment
///      the counter. If the counter reaches the threshold, call
///      `on_unhealthy` and return the original error.
///    * Any other error short-circuits the loop and is returned
///      directly without touching the counter.
/// 4. Between attempts, sleep for the indexed backoff duration. The
///    sleep is wrapped in `tokio::time::timeout` so a cancelled
///    caller does not stall in a backoff forever (cross-spec fix N5).
pub(super) async fn drive_invalid_grant_retry<F, Fut>(
    account_id: AccountId,
    mut op: F,
    on_unhealthy: impl FnOnce(AccountId) + Send,
) -> Result<TokenResponse>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<TokenResponse>>,
{
    let mut on_unhealthy = OnUnhealthyCell::new(on_unhealthy);
    let mut last_invalid_grant_err: Option<CoreError> = None;

    for attempt in 0..ANTIGRAVITY_INVALID_GRANT_THRESHOLD {
        match op().await {
            Ok(token) => {
                reset(account_id);
                if attempt > 0 {
                    tracing::info!(
                        account = account_id.0,
                        attempt,
                        "antigravity oauth: refresh recovered after invalid_grant"
                    );
                }
                return Ok(token);
            }
            Err(e) => {
                let is_invalid_grant = e.to_string().contains("invalid_grant");
                if !is_invalid_grant {
                    // Non-`invalid_grant` errors short-circuit the loop and
                    // do NOT touch the counter (edge case #8).
                    return Err(e);
                }

                let count = bump(account_id);
                tracing::warn!(
                    account = account_id.0,
                    attempt = attempt + 1,
                    consecutive_failures = count,
                    "antigravity oauth: invalid_grant on refresh"
                );
                last_invalid_grant_err = Some(e);

                if count >= ANTIGRAVITY_INVALID_GRANT_THRESHOLD {
                    tracing::error!(
                        account = account_id.0,
                        consecutive_failures = count,
                        "antigravity oauth: marking account unhealthy after {count} consecutive invalid_grant"
                    );
                    on_unhealthy.call(account_id);
                    return Err(last_invalid_grant_err.take().unwrap_or_else(|| {
                        CoreError::Auth("antigravity refresh: invalid_grant".into())
                    }));
                }

                // Exponential backoff before the next attempt. Wrapped in
                // `tokio::time::timeout` so a cancelled caller does not
                // stall here (cross-spec fix N5 from `antigravity-gaps-p2.md`).
                let delay_ms = ANTIGRAVITY_BACKOFF_MS
                    .get(attempt as usize)
                    .copied()
                    .unwrap_or(4_000);
                let sleep = tokio::time::sleep(std::time::Duration::from_millis(delay_ms));
                // Pin the sleep future so it can be polled inside `timeout`
                // without being dropped on cancellation (cancellation-safe).
                tokio::pin!(sleep);
                if tokio::time::timeout(std::time::Duration::from_secs(delay_ms + 1), &mut sleep)
                    .await
                    .is_err()
                {
                    // Caller likely cancelled or stalled. Propagate the last
                    // `invalid_grant` error so the upstream pipeline can act.
                    return Err(last_invalid_grant_err.take().unwrap_or_else(|| {
                        CoreError::Auth("antigravity refresh: cancelled".into())
                    }));
                }
            }
        }
    }

    // Unreachable in normal flow — the loop either returns `Ok` or
    // `Err` on the last attempt. Keep a defensive fallthrough so the
    // compiler accepts a non-`!` return path.
    Err(last_invalid_grant_err
        .unwrap_or_else(|| CoreError::Auth("antigravity refresh: exhausted retries".into())))
}

/// Tiny wrapper so we can move a `FnOnce(AccountId)` into the retry
/// helper while also being able to choose to NOT call it if the loop
/// succeeds before reaching the threshold. Avoids a `OnceCell`-style
/// dance for a single-shot callback.
pub(super) struct OnUnhealthyCell<F: FnOnce(AccountId)> {
    inner: Option<F>,
}

impl<F: FnOnce(AccountId)> OnUnhealthyCell<F> {
    fn new(f: F) -> Self {
        Self { inner: Some(f) }
    }
    fn call(&mut self, account_id: AccountId) {
        if let Some(f) = self.inner.take() {
            f(account_id);
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod adversarial;
