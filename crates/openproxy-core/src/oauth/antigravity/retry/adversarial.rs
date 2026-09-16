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
    let _ = drive_invalid_grant_retry(acc_a, || async { Err(invalid_grant_err()) }, |_| {}).await;
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
        let _ =
            drive_invalid_grant_retry(account_id, || async { Err(invalid_grant_err()) }, |_| {})
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

    let _ = drive_invalid_grant_retry(account_id, || async { Err(network_err()) }, |_| {}).await;

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
        let _ =
            drive_invalid_grant_retry(account_id, || async { Err(invalid_grant_err()) }, |_| {})
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
