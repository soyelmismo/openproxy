//! Per-account consecutive-`invalid_grant` counter state.
//!
//! Lives in its own module so the rest of the provider can access the
//! counter without dragging in the retry loop / `OnUnhealthyCell`
//! implementation. Tests in `retry.rs` reach the `INVALID_GRANT_COUNTERS`
//! map via `super::super::counters::INVALID_GRANT_COUNTERS`.

use std::sync::LazyLock;
use std::sync::atomic::{AtomicU32, Ordering};

use dashmap::DashMap;

use crate::ids::AccountId;
use crate::oauth::DbRef;

/// Number of consecutive `invalid_grant` responses before marking the
/// account `Unhealthy`. Mirrors `UNHEALTHY_THRESHOLD` in
/// `crate::oauth::mod` (kept independent because this path runs
/// on-demand, not from the scheduler).
pub(crate) const ANTIGRAVITY_INVALID_GRANT_THRESHOLD: u32 = 3;

/// Backoff schedule (ms) between retries on `invalid_grant`.
/// `index 0 = before retry 1`, `index 1 = before retry 2`, `index 2 = before retry 3`.
pub(crate) const ANTIGRAVITY_BACKOFF_MS: [u64; 3] = [500, 1_000, 2_000];

/// Per-account consecutive-`invalid_grant` counter, scoped to the running
/// process. Survives until the daemon is restarted; on restart the counter
/// resets and the first `invalid_grant` is a clean slate. Acceptable because
/// the DB's `health_status` column is the source of truth for "blocked"
/// accounts.
///
/// The key is `account_id.0` (i64) so we never construct a transient
/// `String` for hashing on the hot path. The value is an `AtomicU32`
/// so concurrent refreshes for the same account don't race on the
/// counter.
pub(crate) static INVALID_GRANT_COUNTERS: LazyLock<DashMap<i64, AtomicU32>> =
    LazyLock::new(DashMap::new);

/// Increment the consecutive-`invalid_grant` counter for this account
/// and return its new value. Lock-free at the call site (the entry
/// insertion only happens on the first failure for a given account).
///
/// The counter is bounded: once it reaches `ANTIGRAVITY_INVALID_GRANT_THRESHOLD`,
/// subsequent bumps saturate at that value rather than growing without
/// limit (BUG-1 in `docs/specs/adversarial-findings.md`). `fetch_update`
/// guarantees the read-modify-write is atomic across threads, so
/// concurrent bumps for the same account converge on
/// `threshold`, not `N × threshold` (BUG-2).
pub(crate) fn bump(account_id: AccountId) -> u32 {
    let counter = INVALID_GRANT_COUNTERS
        .entry(account_id.0)
        .or_insert_with(|| AtomicU32::new(0));
    // `fetch_update` performs an atomic CAS loop and returns
    // `Ok(previous)` on success, so we compute the NEW value by adding
    // 1 to the previous one when the closure bumped it. The closure
    // caps the value at `ANTIGRAVITY_INVALID_GRANT_THRESHOLD`, so
    // concurrent bumps converge on the threshold instead of inflating
    // it (BUG-2) and repeated calls do not grow it past the threshold
    // (BUG-1).
    let previous = counter
        .value()
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            if current >= ANTIGRAVITY_INVALID_GRANT_THRESHOLD {
                Some(current)
            } else {
                Some(current + 1)
            }
        })
        // `fetch_update` only returns `Err` if the closure returns
        // `None`, which we never do. Mapping the (impossible) error
        // branch to the threshold is purely defensive.
        .unwrap_or(ANTIGRAVITY_INVALID_GRANT_THRESHOLD);
    // `previous` is the value seen inside the closure. If the closure
    // bumped it (i.e. `previous < threshold`), the new value is
    // `previous + 1`; otherwise it equals `previous`.
    if previous >= ANTIGRAVITY_INVALID_GRANT_THRESHOLD {
        previous
    } else {
        previous + 1
    }
}

/// Reset the counter to zero (called when a refresh succeeds).
/// Drop the entry entirely so the map stays bounded by the number of
/// accounts currently in a "bad streak".
pub(crate) fn reset(account_id: AccountId) {
    INVALID_GRANT_COUNTERS.remove(&account_id.0);
}

/// Mark the account as `Unhealthy` in the DB. Two execution paths:
///
/// * `DbRef::Pool` (production): spawn a `tokio::task::spawn_blocking`
///   fire-and-forget task so the synchronous SQLite write never blocks
///   the async runtime and the original `invalid_grant` error is
///   surfaced to the caller without added latency.
/// * `DbRef::Connection` (test path): lock the mutex inline because
///   tests do not own a `DbPool`.
///
/// Failures here are logged but do not propagate — we never want a
/// secondary DB error to mask the original refresh failure.
pub(crate) fn mark_account_unhealthy(db: DbRef<'_>, account_id: AccountId) {
    let log_failure = move |e: &crate::error::CoreError, path: &str| {
        tracing::warn!(
            account = account_id.0,
            error = %e,
            path = path,
            "antigravity oauth: failed to set health to unhealthy"
        );
    };
    match db {
        DbRef::Pool(pool) => {
            let pool = pool.clone();
            tokio::task::spawn_blocking(move || {
                let conn = pool.writer();
                if let Err(e) = crate::accounts::set_health(
                    &conn,
                    account_id,
                    crate::accounts::HealthStatus::Unhealthy,
                ) {
                    log_failure(&e, "spawn_blocking");
                }
            });
        }
        DbRef::Connection(mutex) => {
            let conn = mutex.lock();
            if let Err(e) =
                crate::accounts::set_health(&conn, account_id, crate::accounts::HealthStatus::Unhealthy)
            {
                log_failure(&e, "test_path");
            }
        }
    }
}
