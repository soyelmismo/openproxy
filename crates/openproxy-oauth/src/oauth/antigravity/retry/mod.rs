//! `invalid_grant` retry loop + per-account `OnUnhealthyCell` callback. The loop
//! is generic over the async operation so tests can drive synthetic
//! `Err(invalid_grant)` / `Ok` without the network.

use crate::error::{CoreError, Result};
use crate::ids::AccountId;
use crate::oauth::TokenResponse;

use super::counters::{ANTIGRAVITY_BACKOFF_MS, ANTIGRAVITY_INVALID_GRANT_THRESHOLD, bump, reset};

/// Drives the `invalid_grant` retry loop.
///
/// Up to `ANTIGRAVITY_INVALID_GRANT_THRESHOLD` attempts: `Ok` resets the counter
/// and returns the token. An error containing `"invalid_grant"` bumps the counter,
/// and reaching the threshold calls `on_unhealthy` and returns the original
/// error. Any other error returns immediately without touching the counter.
///
/// Backoff sleeps are wrapped in `tokio::time::timeout` so a cancelled caller does
/// not stall (cross-spec fix N5).
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
                // a non-`invalid_grant` error leaves the counter untouched
                if !is_invalid_grant {
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

                let delay_ms = ANTIGRAVITY_BACKOFF_MS
                    .get(attempt as usize)
                    .copied()
                    .unwrap_or(4_000);
                let sleep = tokio::time::sleep(std::time::Duration::from_millis(delay_ms));
                // pin so `timeout` can poll the sleep without dropping it
                tokio::pin!(sleep);
                if tokio::time::timeout(std::time::Duration::from_secs(delay_ms + 1), &mut sleep)
                    .await
                    .is_err()
                {
                    // caller cancelled or stalled: propagate the last error so
                    // the upstream pipeline can act
                    return Err(last_invalid_grant_err.take().unwrap_or_else(|| {
                        CoreError::Auth("antigravity refresh: cancelled".into())
                    }));
                }
            }
        }
    }

    // the loop returns on every path; this fallthrough satisfies the non-`!`
    // return type
    Err(last_invalid_grant_err
        .unwrap_or_else(|| CoreError::Auth("antigravity refresh: exhausted retries".into())))
}

/// Lets a `FnOnce(AccountId)` move into the retry helper and still stay uncalled
/// when the loop succeeds before the threshold.
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
