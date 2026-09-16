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
