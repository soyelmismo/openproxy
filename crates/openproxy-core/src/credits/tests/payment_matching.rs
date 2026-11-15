use super::*;

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
    assert_eq!(ledger::balance_units(&conn, key).unwrap(), expected_credit);
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
    let expected_credit =
        openproxy_db::credits_ledger::prorated_credit_units(5 * USD, short_amount, quoted);
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
