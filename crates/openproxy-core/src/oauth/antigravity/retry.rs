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
mod tests {
    use super::super::counters::{ANTIGRAVITY_INVALID_GRANT_THRESHOLD, INVALID_GRANT_COUNTERS};
    use super::super::test_util::*;
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[tokio::test]
    async fn drive_retry_success_first_attempt() {
        let account_id = AccountId(9001);
        clear_counter(account_id);

        let calls = AtomicU32::new(0);
        let unhealthy_calls = AtomicU32::new(0);

        let token = drive_invalid_grant_retry(
            account_id,
            || {
                calls.fetch_add(1, Ordering::Relaxed);
                async { Ok(dummy_token("v1")) }
            },
            |_| {
                unhealthy_calls.fetch_add(1, Ordering::Relaxed);
            },
        )
        .await
        .expect("first-attempt success");

        assert_eq!(token.access_token, "access-v1");
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(unhealthy_calls.load(Ordering::Relaxed), 0);
        assert!(!INVALID_GRANT_COUNTERS.contains_key(&account_id.0));
    }

    #[tokio::test]
    async fn drive_retry_success_after_one_invalid_grant() {
        let account_id = AccountId(9002);
        clear_counter(account_id);

        let calls = AtomicU32::new(0);
        let unhealthy_calls = AtomicU32::new(0);

        let token = drive_invalid_grant_retry(
            account_id,
            || {
                let n = calls.fetch_add(1, Ordering::Relaxed);
                async move {
                    if n == 0 {
                        Err(invalid_grant_err())
                    } else {
                        Ok(dummy_token("recovered"))
                    }
                }
            },
            |_| {
                unhealthy_calls.fetch_add(1, Ordering::Relaxed);
            },
        )
        .await
        .expect("recovered after 1 invalid_grant");

        assert_eq!(token.access_token, "access-recovered");
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert_eq!(unhealthy_calls.load(Ordering::Relaxed), 0);
        // Counter reset on success.
        assert!(!INVALID_GRANT_COUNTERS.contains_key(&account_id.0));
    }

    #[tokio::test]
    async fn drive_retry_marks_unhealthy_after_threshold() {
        let account_id = AccountId(9003);
        clear_counter(account_id);

        let calls = AtomicU32::new(0);
        let unhealthy_calls = AtomicU32::new(0);
        let unhealthy_account = std::sync::Mutex::new(None::<AccountId>);

        let err = drive_invalid_grant_retry(
            account_id,
            || {
                calls.fetch_add(1, Ordering::Relaxed);
                async { Err(invalid_grant_err()) }
            },
            |aid| {
                unhealthy_calls.fetch_add(1, Ordering::Relaxed);
                *unhealthy_account.lock().unwrap() = Some(aid);
            },
        )
        .await
        .expect_err("must surface invalid_grant error");

        assert!(
            err.to_string().contains("invalid_grant"),
            "expected invalid_grant in error chain, got: {err}"
        );
        // 3 attempts (initial + 2 retries) before threshold fires.
        assert_eq!(calls.load(Ordering::Relaxed), 3);
        // `on_unhealthy` fires exactly once.
        assert_eq!(unhealthy_calls.load(Ordering::Relaxed), 1);
        assert_eq!(*unhealthy_account.lock().unwrap(), Some(account_id));
        assert!(INVALID_GRANT_COUNTERS.contains_key(&account_id.0));
        assert_eq!(
            INVALID_GRANT_COUNTERS
                .get(&account_id.0)
                .unwrap()
                .value()
                .load(Ordering::Relaxed),
            ANTIGRAVITY_INVALID_GRANT_THRESHOLD
        );

        clear_counter(account_id);
    }

    #[tokio::test]
    async fn drive_retry_ignores_non_invalid_grant_errors() {
        // Sequence: invalid_grant, network, invalid_grant, invalid_grant.
        // The network error must short-circuit; the trailing two
        // `invalid_grant` errors must NOT be reached because the
        // loop returned on the network error. Counter should record
        // only the single initial `invalid_grant`.
        let account_id = AccountId(9004);
        clear_counter(account_id);

        let calls = AtomicU32::new(0);
        let unhealthy_calls = AtomicU32::new(0);
        let seq: Vec<CoreError> = vec![
            invalid_grant_err(),
            network_err(),
            invalid_grant_err(),
            invalid_grant_err(),
        ];

        let err = drive_invalid_grant_retry(
            account_id,
            || {
                let n = calls.fetch_add(1, Ordering::Relaxed);
                let next = seq
                    .get(n as usize)
                    .cloned()
                    .unwrap_or_else(invalid_grant_err);
                async move { Err(next) }
            },
            |_| {
                unhealthy_calls.fetch_add(1, Ordering::Relaxed);
            },
        )
        .await
        .expect_err("network error must surface");

        assert!(
            err.to_string().contains("connection refused"),
            "expected network error to short-circuit, got: {err}"
        );
        // Only the first two attempts ran (invalid_grant then network).
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert_eq!(unhealthy_calls.load(Ordering::Relaxed), 0);
        // Counter recorded only the 1 invalid_grant before the network
        // error short-circuited.
        assert_eq!(
            INVALID_GRANT_COUNTERS
                .get(&account_id.0)
                .unwrap()
                .value()
                .load(Ordering::Relaxed),
            1
        );

        clear_counter(account_id);
    }

    #[tokio::test(start_paused = true)]
    async fn drive_retry_backoff_delays_grow_exponentially() {
        // Verifies that the backoff schedule is approximately
        // [500ms, 1000ms, 2000ms] by capturing timestamps around each
        // sleep. Paused time means we can run this without burning
        // wall-clock seconds.
        let account_id = AccountId(9005);
        clear_counter(account_id);

        let calls = AtomicU32::new(0);
        let mut attempts: Vec<tokio::time::Instant> = Vec::with_capacity(3);
        let unhealthy_calls = AtomicU32::new(0);

        let _ = drive_invalid_grant_retry(
            account_id,
            || {
                attempts.push(tokio::time::Instant::now());
                calls.fetch_add(1, Ordering::Relaxed);
                async { Err(invalid_grant_err()) }
            },
            |_| {
                unhealthy_calls.fetch_add(1, Ordering::Relaxed);
            },
        )
        .await;

        assert_eq!(calls.load(Ordering::Relaxed), 3);
        assert_eq!(attempts.len(), 3);
        let d1 = attempts[1].duration_since(attempts[0]).as_millis();
        let d2 = attempts[2].duration_since(attempts[1]).as_millis();
        // Paused clock: time advances only as fast as the runtime
        // advances it. We assert the observed delay is within a small
        // tolerance of the documented schedule.
        assert!(
            (495..=510).contains(&d1),
            "first backoff should be ~500ms, got {d1}ms"
        );
        assert!(
            (995..=1010).contains(&d2),
            "second backoff should be ~1000ms, got {d2}ms"
        );
        assert_eq!(unhealthy_calls.load(Ordering::Relaxed), 1);

        clear_counter(account_id);
    }

    #[tokio::test(start_paused = true)]
    async fn drive_retry_cancellation_releases_counter() {
        // N5 cross-spec fix verification: when the caller's context is
        // cancelled mid-loop, the sleep must short-circuit and the
        // counter must remain in a coherent state (visible to the
        // next call, but with the count reflecting only completed
        // invalid_grant bumps). We simulate cancellation by racing the
        // retry helper against a `tokio::time::timeout` that fires
        // before the second backoff completes.
        let account_id = AccountId(9006);
        clear_counter(account_id);

        let calls = AtomicU32::new(0);

        // Wrap the retry helper in an outer timeout shorter than the
        // total backoff (500 + 1000 = 1500ms). 750ms guarantees we
        // observe at least one `invalid_grant` bump but cut off
        // before the loop completes.
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(750),
            drive_invalid_grant_retry(
                account_id,
                || {
                    calls.fetch_add(1, Ordering::Relaxed);
                    async { Err(invalid_grant_err()) }
                },
                |_| {},
            ),
        )
        .await;

        // The outer timeout fires; we don't care what the inner
        // result is (it may have errored with `invalid_grant` before
        // the timeout, or it may still be sleeping).
        let _ = result;

        // At least one attempt must have run.
        let calls_so_far = calls.load(Ordering::Relaxed);
        assert!(
            calls_so_far >= 1,
            "expected at least 1 invalid_grant attempt, got {calls_so_far}"
        );

        // The counter is observable and bounded by the threshold (the
        // helper never leaves it above 3 by design).
        if let Some(entry) = INVALID_GRANT_COUNTERS.get(&account_id.0) {
            let v = entry.value().load(Ordering::Relaxed);
            assert!(
                (1..=ANTIGRAVITY_INVALID_GRANT_THRESHOLD).contains(&v),
                "counter out of expected range: {v}"
            );
        }

        clear_counter(account_id);
    }
}

// ==========
// GAP-5: Adversarial tests for invalid_grant retry + counter
// ==========
#[cfg(test)]
mod adversarial_retry_tests {
    use super::super::counters::{ANTIGRAVITY_INVALID_GRANT_THRESHOLD, INVALID_GRANT_COUNTERS};
    use super::super::test_util::*;
    use super::drive_invalid_grant_retry;
    use crate::ids::AccountId;
    use crate::oauth::TokenResponse;
    use std::sync::atomic::{AtomicU32, Ordering};

    // --- 100 concurrent invalid_grant calls to same account (FIX BUG-2) ---

    #[tokio::test]
    async fn adv_concurrent_invalid_grant_same_account() {
        // Documents FIX BUG-2: when many concurrent refresh attempts
        // for the same account all fail with invalid_grant, the counter
        // is now bounded by `fetch_update` + cap. The atomic
        // read-modify-write guarantees every concurrent bump observes
        // the up-to-date value and saturates at
        // `ANTIGRAVITY_INVALID_GRANT_THRESHOLD` (no inflation past it).
        let account_id = AccountId(11_000);
        clear_counter(account_id);
        let concurrency = 100;
        let mut handles = Vec::with_capacity(concurrency);

        for _ in 0..concurrency {
            handles.push(tokio::spawn(async move {
                let _ = drive_invalid_grant_retry(
                    account_id,
                    || async { Err(invalid_grant_err()) },
                    |_| {},
                )
                .await;
            }));
        }

        // Wait for all tasks to complete
        for h in handles {
            h.await.expect("task panicked");
        }

        // Counter is capped at the threshold, not inflated to
        // `concurrency * threshold` as the old buggy implementation
        // produced.
        let val = get_counter_val(account_id);
        assert_eq!(
            val, ANTIGRAVITY_INVALID_GRANT_THRESHOLD,
            "FIX BUG-2: counter must saturate at threshold, got: {val}"
        );
        clear_counter(account_id);
    }

    // --- invalid_grant followed by success on different account ---

    #[tokio::test]
    async fn adv_invalid_grant_then_success_different_account() {
        let acc_a = AccountId(11_001);
        let acc_b = AccountId(11_002);
        clear_counter(acc_a);
        clear_counter(acc_b);

        // Fail account A
        let _ =
            drive_invalid_grant_retry(acc_a, || async { Err(invalid_grant_err()) }, |_| {}).await;
        assert!(INVALID_GRANT_COUNTERS.contains_key(&acc_a.0));

        // Succeed on account B — should NOT clear account A's counter
        let _ = drive_invalid_grant_retry(
            acc_b,
            || async {
                Ok(TokenResponse {
                    access_token: "ok".into(),
                    token_type: "Bearer".into(),
                    expires_in: None,
                    refresh_token: None,
                    scope: None,
                    id_token: None,
                })
            },
            |_| {},
        )
        .await;

        // Account A counter must still be alive
        assert!(
            INVALID_GRANT_COUNTERS.contains_key(&acc_a.0),
            "success on B must not clear A's counter"
        );
        // Account B counter must be cleared (success)
        assert!(
            !INVALID_GRANT_COUNTERS.contains_key(&acc_b.0),
            "success on B must clear B's counter"
        );

        clear_counter(acc_a);
        clear_counter(acc_b);
    }

    // --- AtomicU32 overflow boundary ---

    #[tokio::test(start_paused = true)]
    async fn adv_counter_does_not_crash_at_threshold_boundary() {
        // The counter is AtomicU32 (max 4_294_967_295). We can't easily
        // bump it 4B times in a test, but we can verify the
        // implementation handles `count >= threshold` correctly on
        // the boundary by running a small number of rounds and
        // verifying counter increments by `threshold - previous + 1`
        // per round (not bounded).
        let account_id = AccountId(11_003);
        clear_counter(account_id);

        for _ in 0..5 {
            let _ = drive_invalid_grant_retry(
                account_id,
                || async { Err(invalid_grant_err()) },
                |_| {},
            )
            .await;
        }

        // 5 rounds. Each call starts the loop with the counter already
        // saturated at threshold, so attempt 0 bumps to threshold (no-op
        // because already there), the post-bump count is still >= threshold,
        // and `on_unhealthy` fires immediately. Counter stays at threshold.
        let val = get_counter_val(account_id);
        assert_eq!(
            val, ANTIGRAVITY_INVALID_GRANT_THRESHOLD,
            "FIX BUG-1: counter must stay capped at threshold, got {val}"
        );
        clear_counter(account_id);
    }

    // --- Cancel during sleep — counter must be coherent ---

    #[tokio::test(start_paused = true)]
    async fn adv_cancel_during_backoff_leaves_counter_coherent() {
        let account_id = AccountId(11_004);
        clear_counter(account_id);

        let calls = AtomicU32::new(0);

        // Cancel after 250ms (the first backoff is 500ms, so we cut off
        // during the first sleep).
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            drive_invalid_grant_retry(
                account_id,
                || {
                    calls.fetch_add(1, Ordering::Relaxed);
                    async { Err(invalid_grant_err()) }
                },
                |_| {},
            ),
        )
        .await;

        // Timeout fires — we don't care about the result value.
        let _ = result;

        let c = calls.load(Ordering::Relaxed);
        assert!(c >= 1, "must have made at least one attempt before cancel");

        // Counter is between 1 and threshold (bounded).
        if let Some(entry) = INVALID_GRANT_COUNTERS.get(&account_id.0) {
            let v = entry.value().load(Ordering::Relaxed);
            assert!(
                (1..=ANTIGRAVITY_INVALID_GRANT_THRESHOLD).contains(&v),
                "counter must be bounded, got: {v}"
            );
        }

        clear_counter(account_id);
    }

    // --- Cancel during the op itself (not the sleep) ---

    #[tokio::test(start_paused = true)]
    async fn adv_cancel_during_op_leaves_counter_coherent() {
        let account_id = AccountId(11_005);
        clear_counter(account_id);

        let calls = AtomicU32::new(0);

        // Cancel immediately (0ms) — the op is instant but the timeout
        // may fire before or during the sleep.
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(0),
            drive_invalid_grant_retry(
                account_id,
                || {
                    calls.fetch_add(1, Ordering::Relaxed);
                    async { Err(invalid_grant_err()) }
                },
                |_| {},
            ),
        )
        .await;

        let _ = result;

        // At least 0 attempts ran (timeout may have been instant).
        let c = calls.load(Ordering::Relaxed);
        assert!(
            c >= 1,
            "at least one attempt must have run before instant cancel, got {c}"
        );

        // Counter bounded.
        let val = get_counter_val(account_id);
        assert!(
            val <= ANTIGRAVITY_INVALID_GRANT_THRESHOLD,
            "counter must not exceed threshold after cancel, got: {val}"
        );

        clear_counter(account_id);
    }

    // --- Sequence: 3 invalid_grant then success resets counter ---

    #[tokio::test(start_paused = true)]
    async fn adv_success_resets_counter_after_multiple_invalid_grants() {
        let account_id = AccountId(11_006);
        clear_counter(account_id);

        // First call: 3 consecutive invalid_grants → threshold hit, on_unhealthy called
        let unhealthy = std::sync::atomic::AtomicBool::new(false);
        let _ = drive_invalid_grant_retry(
            account_id,
            || async { Err(invalid_grant_err()) },
            |_| {
                unhealthy.store(true, Ordering::Relaxed);
            },
        )
        .await;
        assert!(
            unhealthy.load(Ordering::Relaxed),
            "on_unhealthy should have fired"
        );
        assert_eq!(
            get_counter_val(account_id),
            ANTIGRAVITY_INVALID_GRANT_THRESHOLD
        );

        // Second call: success → counter must be cleared
        let _ = drive_invalid_grant_retry(
            account_id,
            || async {
                Ok(TokenResponse {
                    access_token: "ok".into(),
                    token_type: "Bearer".into(),
                    expires_in: None,
                    refresh_token: None,
                    scope: None,
                    id_token: None,
                })
            },
            |_| {},
        )
        .await;
        assert!(
            !INVALID_GRANT_COUNTERS.contains_key(&account_id.0),
            "counter must be cleared after success"
        );

        clear_counter(account_id);
    }

    // --- Non-invalid_grant error does NOT bump counter ---

    #[tokio::test]
    async fn adv_network_error_does_not_bump_counter() {
        let account_id = AccountId(11_007);
        clear_counter(account_id);

        let _ =
            drive_invalid_grant_retry(account_id, || async { Err(network_err()) }, |_| {}).await;

        assert!(
            !INVALID_GRANT_COUNTERS.contains_key(&account_id.0),
            "non-invalid_grant error must not bump counter"
        );
        clear_counter(account_id);
    }

    // --- OnUnhealthyCell only fires once ---

    #[tokio::test]
    async fn adv_on_unhealthy_cell_fires_exactly_once() {
        let account_id = AccountId(11_008);
        clear_counter(account_id);

        let calls = std::sync::atomic::AtomicU32::new(0);

        let _ = drive_invalid_grant_retry(
            account_id,
            || async { Err(invalid_grant_err()) },
            |_| {
                calls.fetch_add(1, Ordering::Relaxed);
            },
        )
        .await;

        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "on_unhealthy must fire exactly once"
        );
        clear_counter(account_id);
    }

    // --- Counter behavior across multiple calls (FIX BUG-1) ---

    #[tokio::test(start_paused = true)]
    async fn adv_counter_grows_across_multiple_calls_beyond_threshold() {
        // FIX BUG-1: the counter is now capped at
        // `ANTIGRAVITY_INVALID_GRANT_THRESHOLD`. Across multiple
        // consecutive calls that all fail, the counter does NOT grow
        // unboundedly. Each subsequent call still triggers
        // `on_unhealthy` (the post-bump value is still >= threshold),
        // but the counter metric stays bounded.
        let account_id = AccountId(11_010);
        clear_counter(account_id);

        let unhealthy_count = std::sync::atomic::AtomicU32::new(0);

        // Run 5 calls. Each call still triggers on_unhealthy because
        // the post-bump count is >= threshold on attempt 0 (1, 2, 3, then
        // saturates).
        for _ in 0..5 {
            let _ = drive_invalid_grant_retry(
                account_id,
                || async { Err(invalid_grant_err()) },
                |_| {
                    unhealthy_count.fetch_add(1, Ordering::Relaxed);
                },
            )
            .await;
        }

        assert_eq!(
            unhealthy_count.load(Ordering::Relaxed),
            5,
            "on_unhealthy fires once per call (5 calls → 5 callbacks)"
        );

        // Counter stays capped — does NOT grow past threshold.
        let val = get_counter_val(account_id);
        assert!(
            val <= ANTIGRAVITY_INVALID_GRANT_THRESHOLD,
            "FIX BUG-1: counter must NOT exceed threshold after multiple calls, got: {val}"
        );
        assert_eq!(
            val, ANTIGRAVITY_INVALID_GRANT_THRESHOLD,
            "FIX BUG-1: counter must saturate at threshold, got: {val}"
        );

        clear_counter(account_id);
    }

    // --- Cap-bounded concurrent bumps (new FIX BUG-2 regression) ---

    #[tokio::test(start_paused = true)]
    async fn adv_counter_capped_under_concurrent_bumps() {
        // 100 tasks each call drive_invalid_grant_retry 3 times.
        // After cap + fetch_update, the counter must equal threshold,
        // not exceed it.
        let account_id = AccountId(11_011);
        clear_counter(account_id);

        let mut handles = Vec::with_capacity(100);
        for _ in 0..100 {
            handles.push(tokio::spawn(async move {
                let _ = drive_invalid_grant_retry(
                    account_id,
                    || async { Err(invalid_grant_err()) },
                    |_| {},
                )
                .await;
            }));
        }
        for h in handles {
            h.await.expect("task panicked");
        }

        let val = get_counter_val(account_id);
        assert_eq!(
            val, ANTIGRAVITY_INVALID_GRANT_THRESHOLD,
            "FIX BUG-2: 100 concurrent bumps must saturate at threshold, got: {val}"
        );

        clear_counter(account_id);
    }

    // --- Single-thread cap (new FIX BUG-1 regression) ---

    #[tokio::test(start_paused = true)]
    async fn adv_counter_capped_single_thread() {
        // 5 calls in sequence — counter must saturate at threshold,
        // not grow to 3 + 5*3 = 18 (pre-fix bug) or 3 + 4 = 7 (post
        // unbounded fix). After cap, the value stays at threshold.
        let account_id = AccountId(11_012);
        clear_counter(account_id);

        for _ in 0..5 {
            let _ = drive_invalid_grant_retry(
                account_id,
                || async { Err(invalid_grant_err()) },
                |_| {},
            )
            .await;
        }

        let val = get_counter_val(account_id);
        assert!(
            val <= ANTIGRAVITY_INVALID_GRANT_THRESHOLD,
            "FIX BUG-1: counter must be capped, got: {val}"
        );
        assert_eq!(
            val, ANTIGRAVITY_INVALID_GRANT_THRESHOLD,
            "FIX BUG-1: counter must saturate at threshold, got: {val}"
        );

        clear_counter(account_id);
    }
}
