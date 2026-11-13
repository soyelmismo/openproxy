//! Unit tests for the watcher's pure planning helpers: the bootstrap origin
//! (timestamp of the first block a fresh cursor must replay) and the bounded
//! per-tick scan window.
//!
//! Kept in a submodule so the production source stays under the file-size
//! budget. The real tick — scan → decode → matcher → ledger → cursor — is
//! exercised end-to-end against the mock JSON-RPC node in
//! `tests/e2e_suite/watcher_tests.rs`; these tests only pin the arithmetic
//! that decides which blocks are looked at.

use super::logs::parse_quantity;
use super::*;
use openproxy_core::credits::orders::PAYMENT_WINDOW_SECS;

#[test]
fn bootstrap_target_comes_from_the_oldest_unsettled_order() {
    let now = 1_700_000_000;
    // No unsettled order: the configured window + grace + margin is the
    // bound (a buyer cannot pay an order that does not exist yet).
    assert_eq!(
        bootstrap_target_ts(now, PAYMENT_WINDOW_SECS, None),
        now - PAYMENT_WINDOW_SECS - CONFIRMATION_GRACE_SECS - BOOTSTRAP_TIME_MARGIN_SECS
    );
    // A 24-hour window is honoured, not silently 15 minutes.
    assert_eq!(
        bootstrap_target_ts(now, 86_400, None),
        now - 86_400 - CONFIRMATION_GRACE_SECS - BOOTSTRAP_TIME_MARGIN_SECS
    );
    // An order unsettled for DAYS wins over the window: a payment made in
    // that order's own window carries a block timestamp from days ago, even
    // though the order expired long before this boot. Ignoring it (the old
    // "still payable NOW" filter) is the lost-payment bug.
    let three_days = 3 * 24 * 60 * 60;
    assert_eq!(
        bootstrap_target_ts(now, PAYMENT_WINDOW_SECS, Some(now - three_days)),
        now - three_days - BOOTSTRAP_TIME_MARGIN_SECS
    );
    // An order unsettled for a QUARTER still wins: there is no age clamp on
    // the origin, so an on-time deposit older than any fixed horizon keeps its
    // timestamp target instead of being silently dropped.
    let a_quarter = 100 * 24 * 60 * 60;
    assert_eq!(
        bootstrap_target_ts(now, PAYMENT_WINDOW_SECS, Some(now - a_quarter)),
        now - a_quarter - BOOTSTRAP_TIME_MARGIN_SECS
    );
    // Never before genesis, even with a degenerate window/clock.
    assert_eq!(bootstrap_target_ts(0, 900, Some(0)), 0);
    assert_eq!(bootstrap_target_ts(0, 0, None), 0);
}

#[test]
fn first_boot_scans_from_the_bootstrap_block() {
    // Oldest unsettled order created around block 70_910 (~3 days before
    // the head): the first bounded chunk starts there, not "from now" and
    // not from genesis.
    assert_eq!(scan_window(0, 200_000, 70_910, 500), Some((70_910, 71_409)));
    // A finality head behind the origin scans only the head: no block at or
    // below it can carry that payment yet, and the cursor gets pinned so
    // the following ticks stay incremental.
    assert_eq!(scan_window(0, 100, 70_910, 500), Some((100, 100)));
    // Origin at genesis.
    assert_eq!(scan_window(0, 50, 0, 500), Some((0, 50)));
    // Head 0 (empty chain view) scans only block 0.
    assert_eq!(scan_window(0, 0, 0, 500), Some((0, 0)));
}

#[test]
fn an_ancient_bootstrap_origin_is_walked_in_bounded_chunks() {
    // The origin may be months behind the head (an `expired` order still owed
    // an on-time payment). No horizon clamp exists to drop it; the first
    // request is still ONE bounded chunk and the cursor advances chunk by
    // chunk on the following ticks.
    assert_eq!(
        scan_window(0, 4_000_000, 4_900, 500),
        Some((4_900, 5_399)),
        "old origin is scanned from the origin itself, never moved forward"
    );
    assert_eq!(scan_window(5_399, 4_000_000, 0, 500), Some((5_400, 5_899)));
}

#[test]
fn incremental_scan_is_one_bounded_chunk() {
    assert_eq!(scan_window(4600, 5000, 0, 500), Some((4601, 5000)));
    assert_eq!(scan_window(100, 10_000, 0, 500), Some((101, 600)));
    // The chunk is the provider's cap: a node that rejects >10 blocks gets
    // windows of 10, never a range it will refuse.
    assert_eq!(scan_window(100, 10_000, 0, 10), Some((101, 110)));
    assert_eq!(
        scan_window(0, 5, 0, 0),
        Some((0, 0)),
        "a zero chunk clamps to 1"
    );
    assert_eq!(
        scan_window(5000, 5000, 0, 500),
        None,
        "covered head scans nothing"
    );
    assert_eq!(scan_window(5001, 5000, 0, 500), None, "head behind cursor");
}

#[test]
fn parse_quantity_stays_strict_for_rpc_quantities() {
    assert_eq!(parse_quantity("0x0").unwrap(), 0);
    assert_eq!(parse_quantity("0x10").unwrap(), 16);
    assert_eq!(parse_quantity("0x8a2f").unwrap(), 35375);
    assert!(parse_quantity("zz").is_err());
    assert!(parse_quantity("16").is_err(), "JSON numbers are rejected");
    assert!(parse_quantity("0x").is_err(), "empty quantity");
}
