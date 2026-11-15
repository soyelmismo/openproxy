//! Tests for the prepaid credits module.
//!
//! The money-path guarantees under test:
//! 1. A payment log is consumed exactly once, no matter how often it is
//!    delivered or how many threads deliver it.
//! 2. A given amount can only pay one order; concurrent duplicates lose.
//! 3. Balance can never go negative and never double-credits.
//! 4. Expiry kills the remaining balance with no refund.
//! 5. Anonymous delivery is receipt-bound: a code-only caller never claims the
//!    key, retries survive a lost response, and ack is idempotent.

use crate::credits::ledger::{self, record_spend};
use crate::credits::matcher::{self, PaymentOutcome};
use crate::credits::orders;
use openproxy_db::secrets::MasterKey;
use openproxy_types::ids::ApiKeyId;
use rusqlite::Connection;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

const USD: i64 = super::UNITS_PER_USD;

fn test_conn() -> Connection {
    let mut conn = Connection::open_in_memory().expect("open memory db");
    openproxy_db::migrations::run(&mut conn).expect("run migrations");
    conn
}

/// Insert a bare API key row so key-scoped queries have a real key to point
/// at. Returns its id.
fn seed_key(conn: &Connection, label: &str) -> ApiKeyId {
    conn.execute(
        "INSERT INTO api_keys (key_hash, label) VALUES (?1, ?2)",
        rusqlite::params![format!("hash-{label}"), label],
    )
    .expect("seed key");
    ApiKeyId(conn.last_insert_rowid())
}

fn mk() -> MasterKey {
    MasterKey::generate().expect("generate master key")
}

// ── jitter / order allocation ───────────────────────────────────────

#[test]
fn create_order_jitters_within_six_decimals_and_credits_base() {
    let conn = test_conn();
    let key = seed_key(&conn, "jit1");
    let order =
        orders::create_order(&conn, Some(key), None, 10 * USD, 15, None).expect("create order");
    assert_eq!(order.base_units, 10 * USD);
    assert!(order.amount_units >= 10 * USD);
    assert!(
        order.amount_units < 10 * USD + 10_000,
        "jitter must stay sub-cent"
    );
    assert_eq!(order.status, "pending");
    assert_eq!(order.ttl_days, 15);
    // Public code shape: OP-XXXX-XXXX
    assert_eq!(order.order_code.len(), 12);
    assert!(order.order_code.starts_with("OP-"));
}

#[test]
fn create_order_rejects_non_whole_dollar_base() {
    let conn = test_conn();
    let key = seed_key(&conn, "jit2");
    assert!(orders::create_order(&conn, Some(key), None, 10 * USD + 1, 15, None).is_err());
    assert!(orders::create_order(&conn, Some(key), None, 0, 15, None).is_err());
    assert!(orders::create_order(&conn, Some(key), None, 10 * USD, 0, None).is_err());
}

#[test]
fn live_orders_never_share_an_amount() {
    // 500 sequential creations of the same plan: the sub-cent jitter space
    // (10k values) plus the unique index must keep every live amount distinct.
    let conn = test_conn();
    let key = seed_key(&conn, "jit3");
    let mut seen = std::collections::HashSet::new();
    for _ in 0..500 {
        let o = orders::create_order(&conn, Some(key), None, USD, 15, None).expect("create");
        assert!(
            seen.insert(o.amount_units),
            "duplicate live amount {}",
            o.amount_units
        );
    }
    assert_eq!(seen.len(), 500);
}

#[test]
fn concurrent_order_creation_never_collides() {
    // Four writers racing on the same DB file: the partial unique index is
    // the arbiter, and every retry loop still lands a distinct amount.
    let dir = std::env::temp_dir().join(format!("op-credits-jit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join("credits.db");
    {
        let mut c = Connection::open(&path).expect("open");
        openproxy_db::migrations::run(&mut c).expect("migrate");
        seed_key(&c, "jit4");
    }

    let created = Arc::new(AtomicI64::new(0));
    let mut handles = Vec::new();
    for _ in 0..4 {
        let path = path.clone();
        let created = Arc::clone(&created);
        handles.push(std::thread::spawn(move || {
            let conn = Connection::open(&path).expect("open writer");
            conn.busy_timeout(std::time::Duration::from_secs(10)).ok();
            for _ in 0..50 {
                match orders::create_order(&conn, Some(ApiKeyId(1)), None, 5 * USD, 15, None) {
                    Ok(_) => {
                        created.fetch_add(1, Ordering::SeqCst);
                    }
                    Err(e) => panic!("create failed: {e}"),
                }
            }
        }));
    }
    for h in handles {
        h.join().expect("join writer");
    }
    assert_eq!(created.load(Ordering::SeqCst), 200);

    let conn = Connection::open(&path).expect("open reader");
    let total: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM credit_orders WHERE status = 'pending'",
            [],
            |r| r.get(0),
        )
        .expect("count");
    let distinct: i64 = conn
        .query_row(
            "SELECT COUNT(DISTINCT amount_units) FROM credit_orders",
            [],
            |r| r.get(0),
        )
        .expect("count distinct");
    assert_eq!(total, 200);
    assert_eq!(distinct, 200);
    let _ = std::fs::remove_dir_all(&dir);
}

// ── payment matching: idempotency + CAS ─────────────────────────────

fn paid_order(conn: &Connection, key: ApiKeyId, usd: i64, ttl: i64) -> orders::CreditOrder {
    orders::create_order(conn, Some(key), None, usd * USD, ttl, None).expect("create order")
}

#[test]
fn payment_credits_exactly_once_and_is_idempotent() {
    let conn = test_conn();
    let key = seed_key(&conn, "pay1");
    let mk = mk();
    let order = paid_order(&conn, key, 10, 15);

    let first = matcher::apply_payment(
        &conn,
        &mk,
        "0xdeadbeef",
        3,
        1000,
        order.amount_units,
        Some("0xbuyer"),
        250_000,
    )
    .expect("apply");
    match first {
        PaymentOutcome::Credited {
            credited_units,
            api_key_id,
            ..
        } => {
            assert_eq!(credited_units, 10 * USD);
            assert_eq!(api_key_id, key.0);
        }
        other => panic!("expected credited, got {other:?}"),
    }
    assert_eq!(ledger::balance_units(&conn, key).unwrap(), 10 * USD);

    // Same log delivered again (restart, re-scan, duplicate delivery).
    let again = matcher::apply_payment(
        &conn,
        &mk,
        "0xdeadbeef",
        3,
        1000,
        order.amount_units,
        Some("0xbuyer"),
        250_000,
    )
    .expect("apply again");
    assert!(matches!(again, PaymentOutcome::AlreadySeen { .. }));
    assert_eq!(
        ledger::balance_units(&conn, key).unwrap(),
        10 * USD,
        "replay must not credit twice"
    );

    // Same order amount but a *different* tx after the order is paid: the
    // order is no longer pending, so the deposit is unmatched (never a
    // second credit).
    let other_tx = matcher::apply_payment(
        &conn,
        &mk,
        "0xfeedface",
        0,
        1001,
        order.amount_units,
        Some("0xbuyer"),
        250_000,
    )
    .expect("apply other");
    assert!(matches!(other_tx, PaymentOutcome::Unmatched { .. }));
    assert_eq!(ledger::balance_units(&conn, key).unwrap(), 10 * USD);
}

#[test]
fn one_amount_pays_one_order_only() {
    // Two live orders with distinct amounts; each payment credits exactly
    // the order whose amount matches.
    let conn = test_conn();
    let key = seed_key(&conn, "pay2");
    let mk = mk();
    let a = paid_order(&conn, key, 5, 15);
    let b = paid_order(&conn, key, 7, 15);
    assert_ne!(a.amount_units, b.amount_units);

    matcher::apply_payment(&conn, &mk, "0xaaa", 0, 10, a.amount_units, None, 250_000)
        .expect("pay a");
    assert_eq!(ledger::balance_units(&conn, key).unwrap(), 5 * USD);

    matcher::apply_payment(&conn, &mk, "0xbbb", 0, 11, b.amount_units, None, 250_000)
        .expect("pay b");
    assert_eq!(ledger::balance_units(&conn, key).unwrap(), 12 * USD);
}

#[test]
fn expired_order_is_not_paid() {
    let conn = test_conn();
    let key = seed_key(&conn, "pay3");
    let mk = mk();
    let order = paid_order(&conn, key, 10, 15);
    // Force the payment window closed AND past the 10-minute grace window:
    // within grace a late confirmation still credits (the money arrived for
    // an issued order); beyond grace the order is dead and the payment is
    // unmatched.
    conn.execute(
        "UPDATE credit_orders SET expires_at = datetime('now', '-11 minutes') WHERE id = ?1",
        rusqlite::params![order.id],
    )
    .expect("expire window");

    let outcome = matcher::apply_payment(
        &conn,
        &mk,
        "0xlate",
        0,
        10,
        order.amount_units,
        None,
        250_000,
    )
    .expect("apply");
    assert!(matches!(outcome, PaymentOutcome::Unmatched { .. }));
    assert_eq!(ledger::balance_units(&conn, key).unwrap(), 0);
}

#[test]
fn late_confirmed_payment_within_grace_still_credits() {
    // Broadcast before the deadline, confirmed 30 seconds after it: Base
    // confirms in seconds, so this is the normal late-confirmation shape.
    // The order must credit instead of stranding the buyer's money.
    let conn = test_conn();
    let key = seed_key(&conn, "pay4");
    let mk = mk();
    let order = paid_order(&conn, key, 10, 15);
    conn.execute(
        "UPDATE credit_orders SET expires_at = datetime('now', '-30 seconds') WHERE id = ?1",
        rusqlite::params![order.id],
    )
    .expect("expire window");

    let outcome = matcher::apply_payment(
        &conn,
        &mk,
        "0xlate",
        0,
        10,
        order.amount_units,
        None,
        250_000,
    )
    .expect("apply");
    assert!(
        matches!(outcome, PaymentOutcome::Credited { .. }),
        "grace-window payment must credit, got {outcome:?}"
    );
    assert_eq!(ledger::balance_units(&conn, key).unwrap(), order.base_units);
}

#[test]
fn a_transfer_short_by_the_exchange_fee_still_pays_the_order() {
    // Binance deducts its withdrawal fee from the sent amount: the buyer pays
    // the quoted 5.004103 and 4.804103 arrive. Within the configured
    // tolerance the order must credit — an exact-only matcher strands that
    // money in the collector. ROOT-FIX INVARIANT: the credit is PRO RATA
    // what arrived (the tolerance recognizes the fee, it does not subsidize
    // it), so a deliberately short payment never yields free credit.
    let conn = test_conn();
    let key = seed_key(&conn, "tol1");
    let mk = mk();
    let order = paid_order(&conn, key, 5, 15);
    let fee = 200_000; // 0.2 USDC Base withdrawal fee
    let received = order.amount_units - fee;
    let expected_credit = openproxy_db::credits_ledger::prorated_credit_units(
        order.base_units,
        received,
        order.amount_units,
    );

    let outcome = matcher::apply_payment(&conn, &mk, "0xshort", 0, 10, received, None, 250_000)
        .expect("apply");
    match outcome {
        PaymentOutcome::Credited {
            order_code,
            credited_units,
            received_units,
            ..
        } => {
            assert_eq!(order_code, order.order_code);
            assert_eq!(credited_units, expected_credit);
            assert_eq!(received_units, received);
        }
        other => panic!("short transfer within tolerance must credit, got {other:?}"),
    }
    assert_eq!(
        ledger::balance_units(&conn, key).unwrap(),
        expected_credit
    );
    // A deliberate `quoted - tolerance` payment buys ~tolerance less credit,
    // never the full plan: the self-service discount is gone.
    assert!(
        expected_credit < order.base_units && order.base_units - expected_credit > 190_000,
        "prorated credit must reflect the {fee}-unit shortfall"
    );
}

#[test]
fn a_transfer_short_beyond_tolerance_stays_unmatched() {
    let conn = test_conn();
    let key = seed_key(&conn, "tol2");
    let mk = mk();
    let order = paid_order(&conn, key, 5, 15);
    let received = order.amount_units - 300_000; // 0.3 short, tol 0.25

    let outcome = matcher::apply_payment(&conn, &mk, "0xtooshort", 0, 10, received, None, 250_000)
        .expect("apply");
    assert!(matches!(outcome, PaymentOutcome::Unmatched { .. }));
    assert_eq!(ledger::balance_units(&conn, key).unwrap(), 0);
}

#[test]
fn an_ambiguous_short_payment_between_two_live_orders_stays_unmatched() {
    // Two live orders of the same plan and one short payment: the fee reorders
    // distances (4.809 is closer to 5.004 than to 5.009), so picking the
    // closest candidate would guess which buyer sent it. Nothing is credited
    // and a human resolves the deposit; both orders stay payable.
    let conn = test_conn();
    let key = seed_key(&conn, "tol3");
    let mk = mk();
    let a = paid_order(&conn, key, 5, 15);
    let b = paid_order(&conn, key, 5, 15);
    conn.execute(
        "UPDATE credit_orders SET amount_units = ?1 WHERE id = ?2",
        rusqlite::params![5 * USD + 4_103, a.id],
    )
    .expect("pin a");
    conn.execute(
        "UPDATE credit_orders SET amount_units = ?1 WHERE id = ?2",
        rusqlite::params![5 * USD + 9_217, b.id],
    )
    .expect("pin b");
    let received = (5 * USD + 9_217) - 200_000; // b minus the exchange fee

    let outcome =
        matcher::apply_payment(&conn, &mk, "0xtie", 0, 10, received, None, 250_000).expect("apply");
    assert!(
        matches!(outcome, PaymentOutcome::Unmatched { .. }),
        "two candidates must never guess a buyer, got {outcome:?}"
    );
    assert_eq!(ledger::balance_units(&conn, key).unwrap(), 0);
    for o in [&a, &b] {
        let status: String = conn
            .query_row(
                "SELECT status FROM credit_orders WHERE id = ?1",
                [o.id],
                |r| r.get(0),
            )
            .expect("status");
        assert_eq!(status, "pending", "both orders stay payable");
    }
}

#[test]
fn an_exact_payment_wins_even_when_another_order_sits_in_the_window() {
    // The exact quoted amount is always unambiguous (unique live-amount index)
    // and must credit even though another live order sits well inside the
    // tolerance window: exactness IS the identification.
    let conn = test_conn();
    let key = seed_key(&conn, "tol5");
    let mk = mk();
    let a = paid_order(&conn, key, 5, 15);
    let b = paid_order(&conn, key, 5, 15);
    conn.execute(
        "UPDATE credit_orders SET amount_units = ?1 WHERE id = ?2",
        rusqlite::params![5 * USD + 4_103, a.id],
    )
    .expect("pin a");
    conn.execute(
        "UPDATE credit_orders SET amount_units = ?1 WHERE id = ?2",
        rusqlite::params![5 * USD + 9_217, b.id],
    )
    .expect("pin b");

    let outcome =
        matcher::apply_payment(&conn, &mk, "0xexact", 0, 10, 5 * USD + 9_217, None, 250_000)
            .expect("apply");
    match outcome {
        PaymentOutcome::Credited { order_code, .. } => {
            assert_eq!(order_code, b.order_code);
        }
        other => panic!("exact payment must credit, got {other:?}"),
    }
}

#[test]
fn a_fee_short_payment_credits_when_plan_prices_separate_the_windows() {
    // Real multi-plan shape: $5 and $20 orders live at once. A Binance-short
    // payment for the $5 order has exactly ONE candidate inside 0.25 — the
    // $20 order is 15 dollars away — so it must credit without hesitation.
    let conn = test_conn();
    let key = seed_key(&conn, "tol6");
    let mk = mk();
    let cheap = paid_order(&conn, key, 5, 15);
    let dear = paid_order(&conn, key, 20, 15);
    let received = cheap.amount_units - 200_000;

    let outcome = matcher::apply_payment(&conn, &mk, "0xshort5", 0, 10, received, None, 250_000)
        .expect("apply");
    match outcome {
        PaymentOutcome::Credited { order_code, .. } => {
            assert_eq!(order_code, cheap.order_code);
        }
        other => panic!("short payment with one candidate must credit, got {other:?}"),
    }
    // PRO RATA: the $5 order paid 0.2 short credits 5M × received/quoted,
    // not the full 5M (root fix for the tolerance discount).
    let expected_credit = openproxy_db::credits_ledger::prorated_credit_units(
        cheap.base_units,
        received,
        cheap.amount_units,
    );
    assert_eq!(ledger::balance_units(&conn, key).unwrap(), expected_credit);
    let _ = dear; // still live and unpaid; nothing about it changed
}

#[test]
fn overpaid_payment_within_tolerance_credits() {
    // A buyer who fat-fingers one decimal more still gets the plan; the
    // overage stays in the collector for manual settlement instead of
    // bouncing a paid order.
    let conn = test_conn();
    let key = seed_key(&conn, "tol7");
    let mk = mk();
    let order = paid_order(&conn, key, 5, 15);
    let received = order.amount_units + 100_000; // 0.1 over

    let outcome = matcher::apply_payment(&conn, &mk, "0xover", 0, 10, received, None, 250_000)
        .expect("apply");
    assert!(matches!(outcome, PaymentOutcome::Credited { .. }));
    assert_eq!(ledger::balance_units(&conn, key).unwrap(), 5 * USD);
}

#[test]
fn zero_tolerance_restores_exact_only_matching() {
    // The operator knob that switches the feature OFF must behave exactly like
    // the pre-tolerance matcher: short → unmatched, exact → credited.
    let conn = test_conn();
    let key = seed_key(&conn, "tol8");
    let mk = mk();
    let order = paid_order(&conn, key, 5, 15);

    let short = matcher::apply_payment(&conn, &mk, "0xs", 0, 10, order.amount_units - 1, None, 0)
        .expect("apply short");
    assert!(matches!(short, PaymentOutcome::Unmatched { .. }));

    let exact = matcher::apply_payment(&conn, &mk, "0xe", 0, 11, order.amount_units, None, 0)
        .expect("apply exact");
    assert!(matches!(exact, PaymentOutcome::Credited { .. }));
}

#[test]
fn the_same_fee_short_log_credits_once_under_concurrent_delivery() {
    // Idempotency is keyed on (tx, log), not on the amount: the same short
    // transfer delivered 10× concurrently must credit once, like an exact one.
    let dir = std::env::temp_dir().join(format!("op-credits-tol-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join("credits.db");
    let short_amount;
    {
        let mut c = Connection::open(&path).expect("open");
        openproxy_db::migrations::run(&mut c).expect("migrate");
        let key = seed_key(&c, "tol9");
        let o = paid_order(&c, key, 5, 15);
        short_amount = o.amount_units - 200_000;
    }

    let credited = Arc::new(AtomicI64::new(0));
    let mut handles = Vec::new();
    for _ in 0..8 {
        let path = path.clone();
        let credited = Arc::clone(&credited);
        handles.push(std::thread::spawn(move || {
            let conn = Connection::open(&path).expect("open");
            conn.busy_timeout(std::time::Duration::from_secs(10)).ok();
            let mk = MasterKey::generate().expect("mk");
            for _ in 0..10 {
                match matcher::apply_payment(
                    &conn,
                    &mk,
                    "0xshortdup",
                    0,
                    500,
                    short_amount,
                    None,
                    250_000,
                ) {
                    Ok(PaymentOutcome::Credited { .. }) => {
                        credited.fetch_add(1, Ordering::SeqCst);
                    }
                    Ok(PaymentOutcome::AlreadySeen { .. }) => {}
                    Ok(other) => panic!("unexpected outcome: {other:?}"),
                    Err(e) => panic!("apply failed: {e}"),
                }
            }
        }));
    }
    for h in handles {
        h.join().expect("join");
    }
    assert_eq!(credited.load(Ordering::SeqCst), 1, "exactly one credit");
    let conn = Connection::open(&path).expect("open reader");
    let key_id: i64 = conn
        .query_row("SELECT id FROM api_keys WHERE label = 'tol9'", [], |r| {
            r.get(0)
        })
        .expect("key");
    // PRO RATA: one short payment credits once, the prorated amount.
    let quoted: i64 = conn
        .query_row(
            "SELECT amount_units FROM credit_orders WHERE api_key_id = ?1",
            [key_id],
            |r| r.get(0),
        )
        .expect("quoted");
    let expected_credit = openproxy_db::credits_ledger::prorated_credit_units(
        5 * USD,
        short_amount,
        quoted,
    );
    assert_eq!(
        ledger::balance_units(&conn, ApiKeyId(key_id)).unwrap(),
        expected_credit,
        "credited exactly once, pro rata"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn stress_fee_short_payments_amid_many_live_orders_never_miscredit() {
    // The adversarial flood: 30 live orders of the SAME plan (jitters within a
    // cent of each other) and 30 fee-short transfers, delivered concurrently
    // by 8 threads. EVERY short payment has 30 candidates inside the 0.25
    // window, so the matcher must refuse them ALL — crediting the closest
    // would guess a buyer (4.809 sits closer to 5.004 than to 5.009). Zero
    // credits, zero balance, all orders still pending. The money waits in the
    // collector for a human instead of going to the wrong key.
    let dir = std::env::temp_dir().join(format!("op-credits-flood-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join("credits.db");
    let mut shorts: Vec<i64> = Vec::new();
    {
        let mut c = Connection::open(&path).expect("open");
        openproxy_db::migrations::run(&mut c).expect("migrate");
        let key = seed_key(&c, "flood");
        for i in 0..30 {
            let o = paid_order(&c, key, 5, 15);
            shorts.push(o.amount_units - 200_000);
            let _ = i;
        }
    }

    let unmatched = Arc::new(AtomicI64::new(0));
    let credited = Arc::new(AtomicI64::new(0));
    let mut handles = Vec::new();
    for t in 0..8 {
        let path = path.clone();
        let unmatched = Arc::clone(&unmatched);
        let credited = Arc::clone(&credited);
        let mine: Vec<i64> = shorts
            .iter()
            .enumerate()
            .filter(|(i, _)| i % 8 == t)
            .map(|(_, a)| *a)
            .collect();
        handles.push(std::thread::spawn(move || {
            let conn = Connection::open(&path).expect("open");
            conn.busy_timeout(std::time::Duration::from_secs(15)).ok();
            let mk = MasterKey::generate().expect("mk");
            for (n, amount) in mine.iter().enumerate() {
                match matcher::apply_payment(
                    &conn,
                    &mk,
                    &format!("0xflood{t}-{n}"),
                    0,
                    500,
                    *amount,
                    None,
                    250_000,
                ) {
                    Ok(PaymentOutcome::Unmatched { .. }) => {
                        unmatched.fetch_add(1, Ordering::SeqCst);
                    }
                    Ok(PaymentOutcome::Credited { .. }) => {
                        credited.fetch_add(1, Ordering::SeqCst);
                    }
                    Ok(other) => panic!("unexpected outcome: {other:?}"),
                    Err(e) => panic!("apply failed: {e}"),
                }
            }
        }));
    }
    for h in handles {
        h.join().expect("join");
    }
    assert_eq!(
        credited.load(Ordering::SeqCst),
        0,
        "an ambiguous flood must never credit"
    );
    assert_eq!(
        unmatched.load(Ordering::SeqCst),
        shorts.len() as i64,
        "every short payment must fall back to unmatched"
    );

    let conn = Connection::open(&path).expect("open reader");
    let paid: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM credit_orders WHERE status IN ('paid','minted')",
            [],
            |r| r.get(0),
        )
        .expect("paid count");
    assert_eq!(paid, 0, "no order may flip to paid");
    let key_id: i64 = conn
        .query_row("SELECT id FROM api_keys WHERE label = 'flood'", [], |r| {
            r.get(0)
        })
        .expect("key");
    assert_eq!(
        ledger::balance_units(&conn, ApiKeyId(key_id)).unwrap(),
        0,
        "no balance may appear"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn stress_exact_payments_amid_many_live_orders_all_credit_their_order() {
    // The mirror flood: 30 live same-plan orders, 30 EXACT payments delivered
    // concurrently. Exactness identifies every order (unique live-amount
    // index), so all 30 must credit their own order and the total balance
    // must be exactly 30× the plan price — no cross-crediting, no losses.
    let dir = std::env::temp_dir().join(format!("op-credits-exactflood-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join("credits.db");
    let mut exacts: Vec<i64> = Vec::new();
    {
        let mut c = Connection::open(&path).expect("open");
        openproxy_db::migrations::run(&mut c).expect("migrate");
        let key = seed_key(&c, "exactflood");
        for _ in 0..30 {
            let o = paid_order(&c, key, 5, 15);
            exacts.push(o.amount_units);
        }
    }

    let credited = Arc::new(AtomicI64::new(0));
    let mut handles = Vec::new();
    for t in 0..8 {
        let path = path.clone();
        let credited = Arc::clone(&credited);
        let mine: Vec<i64> = exacts
            .iter()
            .enumerate()
            .filter(|(i, _)| i % 8 == t)
            .map(|(_, a)| *a)
            .collect();
        handles.push(std::thread::spawn(move || {
            let conn = Connection::open(&path).expect("open");
            conn.busy_timeout(std::time::Duration::from_secs(15)).ok();
            let mk = MasterKey::generate().expect("mk");
            for (n, amount) in mine.iter().enumerate() {
                match matcher::apply_payment(
                    &conn,
                    &mk,
                    &format!("0xexactflood{t}-{n}"),
                    0,
                    500,
                    *amount,
                    None,
                    250_000,
                ) {
                    Ok(PaymentOutcome::Credited { .. }) => {
                        credited.fetch_add(1, Ordering::SeqCst);
                    }
                    Ok(other) => panic!("unexpected outcome: {other:?}"),
                    Err(e) => panic!("apply failed: {e}"),
                }
            }
        }));
    }
    for h in handles {
        h.join().expect("join");
    }
    assert_eq!(
        credited.load(Ordering::SeqCst),
        30,
        "every exact payment credits"
    );

    let conn = Connection::open(&path).expect("open reader");
    let key_id: i64 = conn
        .query_row(
            "SELECT id FROM api_keys WHERE label = 'exactflood'",
            [],
            |r| r.get(0),
        )
        .expect("key");
    assert_eq!(
        ledger::balance_units(&conn, ApiKeyId(key_id)).unwrap(),
        30 * 5 * USD,
        "total credited equals the sum of the plans"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn concurrent_duplicate_payments_credit_once() {
    // The stress test that matters: 8 threads deliver the SAME log 10 times
    // each against a shared DB file. Exactly one may win; the balance must
    // equal the single top-up.
    let dir = std::env::temp_dir().join(format!("op-credits-dup-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join("credits.db");
    let order_amount;
    {
        let mut c = Connection::open(&path).expect("open");
        openproxy_db::migrations::run(&mut c).expect("migrate");
        let key = seed_key(&c, "dup");
        let o = paid_order(&c, key, 20, 15);
        order_amount = o.amount_units;
    }

    let winners = Arc::new(AtomicI64::new(0));
    let mut handles = Vec::new();
    for _ in 0..8 {
        let path = path.clone();
        let winners = Arc::clone(&winners);
        handles.push(std::thread::spawn(move || {
            let conn = Connection::open(&path).expect("open");
            conn.busy_timeout(std::time::Duration::from_secs(10)).ok();
            let mk = MasterKey::generate().expect("mk");
            for _ in 0..10 {
                match matcher::apply_payment(
                    &conn,
                    &mk,
                    "0xsamelog",
                    0,
                    500,
                    order_amount,
                    None,
                    250_000,
                ) {
                    Ok(PaymentOutcome::Credited { .. }) => {
                        winners.fetch_add(1, Ordering::SeqCst);
                    }
                    Ok(_) => {}
                    Err(e) => panic!("apply failed: {e}"),
                }
            }
        }));
    }
    for h in handles {
        h.join().expect("join");
    }
    assert_eq!(
        winners.load(Ordering::SeqCst),
        1,
        "exactly one delivery may credit the order"
    );

    let conn = Connection::open(&path).expect("open reader");
    let balance: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(remaining_units), 0) FROM credit_transactions \
             WHERE status = 'active'",
            [],
            |r| r.get(0),
        )
        .expect("sum");
    assert_eq!(balance, 20 * USD, "no duplicate credits, no leaks");
    let _ = std::fs::remove_dir_all(&dir);
}

// ── ledger: FIFO spend + expiry ─────────────────────────────────────

#[test]
fn spend_is_fifo_by_expiry_and_cannot_overdraw() {
    let conn = test_conn();
    let key = seed_key(&conn, "spend1");
    let mk = mk();
    // Two lots: $3 expiring in 1 day, $5 expiring in 30.
    let soon = paid_order(&conn, key, 3, 1);
    matcher::apply_payment(&conn, &mk, "0xsoon", 0, 1, soon.amount_units, None, 250_000)
        .expect("pay soon");
    let late = paid_order(&conn, key, 5, 30);
    matcher::apply_payment(&conn, &mk, "0xlate", 0, 2, late.amount_units, None, 250_000)
        .expect("pay late");
    assert_eq!(ledger::balance_units(&conn, key).unwrap(), 8 * USD);

    // Spend $4: takes the whole $3 lot, then $1 from the $5 lot.
    record_spend(&conn, key, 4 * USD).expect("spend");
    assert_eq!(ledger::balance_units(&conn, key).unwrap(), 4 * USD);

    let lots = ledger::balance_view(&conn, key).unwrap().lots;
    assert_eq!(lots.len(), 1, "the soon lot must be exhausted");
    assert!((lots[0].remaining_usd - 4.0).abs() < 1e-9);

    // LAST-DRAW: a spend beyond the balance consumes the whole remainder and
    // closes the plan at exactly 0.
    record_spend(&conn, key, 5 * USD).expect("last-draw spend");
    assert_eq!(ledger::balance_units(&conn, key).unwrap(), 0);

    // An exhausted balance charges nothing and reports it.
    record_spend(&conn, key, 4 * USD).expect("no balance left to charge");
    assert_eq!(ledger::balance_units(&conn, key).unwrap(), 0);
}

#[test]
fn expiry_kills_remaining_without_refund() {
    let conn = test_conn();
    let key = seed_key(&conn, "exp1");
    let mk = mk();
    let order = paid_order(&conn, key, 10, 15);
    matcher::apply_payment(&conn, &mk, "0xexp", 0, 1, order.amount_units, None, 250_000)
        .expect("pay");
    record_spend(&conn, key, 4 * USD).expect("spend");
    assert_eq!(ledger::balance_units(&conn, key).unwrap(), 6 * USD);

    // Age the lot past its expiry.
    conn.execute(
        "UPDATE credit_transactions SET expires_at = datetime('now', '-1 second') \
         WHERE kind = 'topup'",
        [],
    )
    .expect("age lot");

    assert_eq!(
        ledger::balance_units(&conn, key).unwrap(),
        0,
        "expired money is gone"
    );
    let killed = ledger::expire_due(&conn).expect("expire");
    assert_eq!(killed, 1);
    let remaining: i64 = conn
        .query_row(
            "SELECT remaining_units FROM credit_transactions WHERE kind = 'topup'",
            [],
            |r| r.get(0),
        )
        .expect("remaining");
    assert_eq!(remaining, 6 * USD, "the audit trail records what died");
    let status: String = conn
        .query_row(
            "SELECT status FROM credit_transactions WHERE kind = 'topup'",
            [],
            |r| r.get(0),
        )
        .expect("status");
    assert_eq!(status, "expired");

    // Spending against an exhausted balance charges nothing.
    assert_eq!(record_spend(&conn, key, 1).unwrap().spent_units, 0);
    assert_eq!(ledger::balance_units(&conn, key).unwrap(), 0);
}

#[test]
fn history_exposes_the_on_chain_pointer() {
    let conn = test_conn();
    let key = seed_key(&conn, "hist1");
    let mk = mk();
    let order = paid_order(&conn, key, 10, 15);
    matcher::apply_payment(
        &conn,
        &mk,
        "0xaudit",
        7,
        999,
        order.amount_units,
        None,
        250_000,
    )
    .expect("pay");

    let rows = ledger::history(&conn, key, 50).expect("history");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, "topup");
    assert_eq!(rows[0].tx_hash.as_deref(), Some("0xaudit"));
    assert_eq!(rows[0].order_id, Some(order.id));
    assert_eq!(rows[0].amount_units, 10 * USD);
}

#[test]
fn balance_view_reports_lots_and_credit_based() {
    let conn = test_conn();
    let key = seed_key(&conn, "view1");
    assert!(!ledger::is_credit_based(&conn, key).unwrap());
    let mk = mk();
    let order = paid_order(&conn, key, 10, 15);
    matcher::apply_payment(
        &conn,
        &mk,
        "0xview",
        0,
        1,
        order.amount_units,
        None,
        250_000,
    )
    .expect("pay");
    let view = ledger::balance_view(&conn, key).unwrap();
    assert!(view.credit_based);
    assert!((view.balance_usd - 10.0).abs() < 1e-9);
    assert_eq!(view.lots.len(), 1);
    assert!((view.lots[0].amount_usd - 10.0).abs() < 1e-9);
}

// ── anonymous checkout: mint + balance ──────────────────────────────

#[test]
fn anonymous_payment_mints_a_chat_key_with_the_credited_balance() {
    let conn = test_conn();
    let mk = mk();
    let receipt = "00000000-0000-4000-8000-000000000010";
    let order = orders::create_order_with_receipt(
        &conn,
        orders::CreateOrderRequest {
            api_key_id: None,
            payer_email: Some("jordan.lee18@outlook.com".into()),
            base_units: 10 * USD,
            ttl_days: 15,
            tier_id: None,
        },
        15 * 60,
        receipt,
    )
    .expect("create anon order");
    let outcome = matcher::apply_payment(
        &conn,
        &mk,
        "0xanon",
        0,
        42,
        order.amount_units,
        Some("0xbuyer"),
        250_000,
    )
    .expect("apply");
    let (minted, api_key_id) = match outcome {
        PaymentOutcome::Credited {
            minted_key: Some(k),
            api_key_id,
            ..
        } => (k, api_key_id),
        other => panic!("expected minted key, got {other:?}"),
    };
    assert!(minted.starts_with("op_live_"));
    // The key is a real chat-scoped row with the balance attached.
    let scopes: String = conn
        .query_row(
            "SELECT scopes_json FROM api_keys WHERE id = ?1",
            rusqlite::params![api_key_id],
            |r| r.get(0),
        )
        .expect("scopes");
    assert!(scopes.contains("chat"));
    assert_eq!(
        ledger::balance_units(&conn, ApiKeyId(api_key_id)).unwrap(),
        10 * USD
    );
}

#[test]
fn session_order_credits_the_existing_key_without_minting() {
    let conn = test_conn();
    let key = seed_key(&conn, "sess1");
    let mk = mk();
    let order = paid_order(&conn, key, 15, 15);
    let outcome = matcher::apply_payment(
        &conn,
        &mk,
        "0xsess",
        0,
        1,
        order.amount_units,
        None,
        250_000,
    )
    .expect("apply");
    match outcome {
        PaymentOutcome::Credited {
            minted_key,
            api_key_id,
            ..
        } => {
            assert!(minted_key.is_none(), "session top-up must not mint a key");
            assert_eq!(api_key_id, key.0);
        }
        other => panic!("expected credited, got {other:?}"),
    }
    let key_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM api_keys", [], |r| r.get(0))
        .expect("count");
    assert_eq!(key_count, 1, "no extra key created");
}

// ── tiers + leveraged packages ──────────────────────────────────────

fn tier_config(models: &[&str], weekly: f64) -> crate::credits::TierConfig {
    crate::credits::TierConfig {
        weekly_usd_cap: Some(weekly),
        five_hour_usd_cap: None,
        allowed_models: Some(models.iter().map(|m| m.to_string()).collect()),
        allowed_providers: None,
        allowed_combos: None,
        only_whitelisted: None,
    }
}

/// Sellable plan commerce: what the buyer pays, what they get, how long it lives.
fn plan(price: f64, credit: f64, ttl: i64) -> crate::credits::PlanCommerce {
    crate::credits::PlanCommerce {
        stock: None,
        price_usd: Some(price),
        credit_usd: Some(credit),
        ttl_days: ttl,
    }
}

/// A draft plan (no price): not listed for sale.
fn draft_plan() -> crate::credits::PlanCommerce {
    crate::credits::PlanCommerce {
        stock: None,
        price_usd: None,
        credit_usd: None,
        ttl_days: 30,
    }
}

#[test]
fn tier_crud_and_apply_forces_the_whitelist_gate() {
    let conn = test_conn();
    let key = seed_key(&conn, "tiered");

    let t = crate::credits::tiers::create(
        &conn,
        "flash",
        &tier_config(&["m-a", "m-b"], 30.0),
        &draft_plan(),
    )
    .expect("create tier");
    assert_eq!(t.key_count, 0);
    assert!(t.active);

    // Validation: blank name rejected, duplicate rejected by UNIQUE.
    assert!(
        crate::credits::tiers::create(&conn, "  ", &tier_config(&["m"], 1.0), &draft_plan())
            .is_err()
    );
    assert!(
        crate::credits::tiers::create(&conn, "flash", &tier_config(&["m"], 1.0), &draft_plan())
            .is_err()
    );

    crate::credits::tiers::apply_to_key(&conn, key, &t.config).expect("apply");
    let models: String = conn
        .query_row(
            "SELECT allowed_models_json FROM api_keys WHERE id = ?1",
            [key.0],
            |r| r.get(0),
        )
        .expect("models");
    assert_eq!(models, r#"["m-a","m-b"]"#);
    let gate: i64 = conn
        .query_row(
            "SELECT only_whitelisted FROM api_keys WHERE id = ?1",
            [key.0],
            |r| r.get(0),
        )
        .expect("gate");
    // Money invariant: an allowlist is a hard gate, not advisory.
    assert_eq!(gate, 1, "allowlist must force only_whitelisted");
    let caps = crate::api_key_quotas::get_caps(&conn, key)
        .expect("caps")
        .expect("caps row");
    assert_eq!(caps.weekly_usd_cap, Some(30.0));

    // Update replaces the payload wholesale.
    crate::credits::tiers::update(
        &conn,
        t.id,
        Some("flash2"),
        Some(&tier_config(&["m-c"], 40.0)),
        Some(false),
        None,
    )
    .expect("update");
    let t2 = crate::credits::tiers::get(&conn, t.id)
        .expect("get")
        .expect("tier");
    assert_eq!(t2.name, "flash2");
    assert!(!t2.active);
    assert_eq!(
        t2.config.allowed_models.as_deref(),
        Some(&["m-c".to_string()][..])
    );

    // Unused tier deletes cleanly.
    crate::credits::tiers::delete(&conn, t.id).expect("delete");
    assert!(
        crate::credits::tiers::get(&conn, t.id)
            .expect("get")
            .is_none()
    );
}

#[test]
fn tier_allowlist_partitions_combos_from_models() {
    let conn = test_conn();
    let key = seed_key(&conn, "part");

    // A live combo row — the bare-name detection target.
    conn.execute(
        "INSERT INTO combos (name, strategy) VALUES ('autos', 'priority')",
        [],
    )
    .expect("combo row");

    // The admin UI sends ONE merged selection: `combo:autos` rides inside
    // allowed_models. The stored tier must end up canonical: models in
    // models_json, combo NAMES in combos_json, never a combo in models.
    let t = crate::credits::tiers::create(
        &conn,
        "mixed",
        &crate::credits::TierConfig {
            allowed_models: Some(vec!["combo:autos".into(), "deepseek-v4.1-flash".into()]),
            allowed_combos: None,
            ..tier_config(&["deepseek-v4.1-flash"], 10.0)
        },
        &draft_plan(),
    )
    .expect("create mixed tier");
    let row_models: String = conn
        .query_row(
            "SELECT models_json FROM credit_tiers WHERE id = ?1",
            [t.id],
            |r| r.get(0),
        )
        .expect("row models");
    let row_combos: String = conn
        .query_row(
            "SELECT combos_json FROM credit_tiers WHERE id = ?1",
            [t.id],
            |r| r.get(0),
        )
        .expect("row combos");
    assert_eq!(
        row_models, r#"["deepseek-v4.1-flash"]"#,
        "combo must leave models_json"
    );
    assert_eq!(
        row_combos, r#"["autos"]"#,
        "combo name must land in combos_json"
    );

    // Applying to a key resolves the combo NAME to the numeric id the
    // routing gate consumes, and the allowlist still forces the hard gate.
    crate::credits::tiers::apply_to_key(&conn, key, &t.config).expect("apply");
    let key_combos: String = conn
        .query_row(
            "SELECT allowed_combos_json FROM api_keys WHERE id = ?1",
            [key.0],
            |r| r.get(0),
        )
        .expect("key combos");
    assert_eq!(
        key_combos, "[1]",
        "combo name must resolve to its numeric id"
    );
    let gate: i64 = conn
        .query_row(
            "SELECT only_whitelisted FROM api_keys WHERE id = ?1",
            [key.0],
            |r| r.get(0),
        )
        .expect("gate");
    assert_eq!(gate, 1, "allowlist must force only_whitelisted");
}

#[test]
fn leveraged_plan_charges_less_than_it_credits() {
    let conn = test_conn();
    let key = seed_key(&conn, "lev");
    let mk = mk();

    // The tier IS the product: price 1, credit 6 (6x), lives 7 days.
    let t = crate::credits::tiers::create(
        &conn,
        "flash",
        &tier_config(&["m-1"], 25.0),
        &plan(1.0, 6.0, 7),
    )
    .expect("tier");

    // The buyer is charged the PRICE (whole dollar + jitter); the order
    // records the CREDIT (6x) for the matcher to pay out.
    let order =
        orders::create_order(&conn, Some(key), None, USD, t.ttl_days, Some(t.id)).expect("order");
    assert_eq!(
        order.credit_units,
        6 * USD,
        "credit_units must carry the leverage"
    );
    assert!(order.amount_units >= USD && order.amount_units < 2 * USD);

    let outcome =
        matcher::apply_payment(&conn, &mk, "0xlev", 0, 1, order.amount_units, None, 250_000)
            .expect("apply");
    let credited = match outcome {
        PaymentOutcome::Credited { credited_units, .. } => credited_units,
        other => panic!("expected credited, got {other:?}"),
    };
    assert_eq!(
        credited,
        6 * USD,
        "the balance must be the leveraged credit"
    );

    // The tier shaping landed on the key together with the credit.
    let models: String = conn
        .query_row(
            "SELECT allowed_models_json FROM api_keys WHERE id = ?1",
            [key.0],
            |r| r.get(0),
        )
        .expect("models");
    assert_eq!(models, r#"["m-1"]"#);
    let gate: i64 = conn
        .query_row(
            "SELECT only_whitelisted FROM api_keys WHERE id = ?1",
            [key.0],
            |r| r.get(0),
        )
        .expect("gate");
    assert_eq!(gate, 1);
    let caps = crate::api_key_quotas::get_caps(&conn, key)
        .expect("caps")
        .expect("caps row");
    assert_eq!(caps.weekly_usd_cap, Some(25.0));
}

mod sweep;

#[test]
fn expired_order_within_grace_still_holds_its_seat() {
    // Root-fix regression (seat oversell, audit A1-02): an anonymous order
    // whose window expired 60s ago sits INSIDE the 10-minute confirmation
    // grace, so the matcher would still accept a late payment for it. The
    // seat must stay reserved: a second buyer taking the "free" seat while
    // the first one paid inside the grace oversold the plan (stock=1 sold
    // twice, reproduced with both payments credited).
    let conn = test_conn();
    let tier = crate::credits::tiers::create(
        &conn,
        "founders-seat",
        &crate::credits::TierConfig::default(),
        &crate::credits::PlanCommerce {
            price_usd: Some(5.0),
            credit_usd: Some(5.0),
            ttl_days: 15,
            stock: Some(1),
        },
    )
    .expect("tier");

    // Buyer A takes the only seat with an anonymous order, then lets the
    // window expire 60s ago (inside the grace).
    let a = orders::create_order_with_receipt(
        &conn,
        orders::CreateOrderRequest {
            api_key_id: None,
            payer_email: None,
            base_units: 0,
            ttl_days: 0,
            tier_id: Some(tier.id),
        },
        900,
        "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
    )
    .expect("order A");
    conn.execute(
        "UPDATE credit_orders SET status = 'expired', \
         expires_at = datetime('now', '-60 seconds') WHERE id = ?1",
        rusqlite::params![a.id],
    )
    .expect("expire A");

    let taken = crate::credits::tiers::seats_taken(&conn, tier.id).expect("seats");
    assert_eq!(taken, 1, "the grace window must keep the seat reserved");
    let err = orders::create_order_with_receipt(
        &conn,
        orders::CreateOrderRequest {
            api_key_id: None,
            payer_email: None,
            base_units: 0,
            ttl_days: 0,
            tier_id: Some(tier.id),
        },
        900,
        "aaaaaaaa-bbbb-cccc-dddd-ffffffffffff",
    );
    assert!(
        matches!(&err, Err(openproxy_types::CoreError::Validation(m)) if m.contains("is full")),
        "buyer B must not take a seat the late-paying A still owns: {err:?}"
    );

    // Outside the grace (10 min + margin) the seat frees for the next buyer.
    conn.execute(
        "UPDATE credit_orders SET expires_at = datetime('now', '-700 seconds') WHERE id = ?1",
        rusqlite::params![a.id],
    )
    .expect("age out A");
    assert_eq!(
        crate::credits::tiers::seats_taken(&conn, tier.id).expect("seats"),
        0,
        "past the grace the seat is really free"
    );
}
