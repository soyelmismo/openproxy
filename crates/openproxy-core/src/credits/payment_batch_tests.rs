//! Group settlement must commit once, never leak partial credits, and replay.
use super::{matcher::*, orders};
use openproxy_db::MasterKey;
use rusqlite::Connection;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
fn fixture() -> (Connection, MasterKey, Vec<PaymentInput>) {
    let mut db = Connection::open_in_memory().unwrap();
    openproxy_db::migrations::run(&mut db).unwrap();
    let master = MasterKey::generate().unwrap();
    let items = (0..3)
        .map(|i| {
            let order = orders::create_order_with_receipt(
                &db,
                orders::CreateOrderRequest {
                    api_key_id: None,
                    payer_email: None,
                    base_units: 1_000_000,
                    ttl_days: 7,
                    tier_id: None,
                },
                900,
                &format!("batch-payment-receipt-000000000000{i}"),
            )
            .unwrap();
            PaymentInput {
                tx_hash: format!("batch-tx-{i}"),
                log_index: 0,
                block: PaymentBlock {
                    number: 100,
                    timestamp: chrono::Utc::now().timestamp(),
                },
                amount_units: order.amount_units,
                from_address: None,
            }
        })
        .collect();
    (db, master, items)
}
fn count(db: &Connection, table: &str) -> i64 {
    db.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}
#[test]
fn payment_batch_commits_once_and_replay_cannot_double_credit() {
    let (db, master, items) = fixture();
    let commits = Arc::new(AtomicUsize::new(0));
    let n = commits.clone();
    db.commit_hook(Some(move || {
        n.fetch_add(1, Ordering::SeqCst);
        false
    }))
    .unwrap();
    let out = apply_payments_batch(&db, &master, &items, 250_000).unwrap();
    assert!(
        out.iter()
            .all(|r| matches!(r, PaymentOutcome::Credited { .. }))
    );
    assert_eq!(commits.load(Ordering::SeqCst), 1);
    let again = apply_payments_batch(&db, &master, &items, 250_000).unwrap();
    assert!(
        again
            .iter()
            .all(|r| matches!(r, PaymentOutcome::AlreadySeen { .. }))
    );
    assert_eq!(count(&db, "credit_transactions"), 3);
    assert!(
        openproxy_db::credits_ledger::reconcile(&db)
            .unwrap()
            .is_consistent()
    );
}
#[test]
fn failure_in_later_payment_rolls_back_earlier_payment_and_retry_works() {
    let (db, master, items) = fixture();
    db.execute_batch("CREATE TRIGGER fail_second BEFORE INSERT ON credit_transactions WHEN (SELECT COUNT(*) FROM credit_transactions)=1 BEGIN SELECT RAISE(ABORT,'batch fail'); END").unwrap();
    assert!(apply_payments_batch(&db, &master, &items, 250_000).is_err());
    for table in ["credit_transactions", "matched_payments", "api_keys"] {
        assert_eq!(count(&db, table), 0, "{table}");
    }
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM credit_orders WHERE status='pending'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        3
    );
    db.execute_batch("DROP TRIGGER fail_second").unwrap();
    assert_eq!(
        apply_payments_batch(&db, &master, &items, 250_000)
            .unwrap()
            .len(),
        3
    );
    assert!(
        openproxy_db::credits_ledger::reconcile(&db)
            .unwrap()
            .is_consistent()
    );
}
#[test]
fn aborted_batch_commit_publishes_no_outcomes_or_rows() {
    let (db, master, items) = fixture();
    db.commit_hook(Some(|| true)).unwrap();
    assert!(apply_payments_batch(&db, &master, &items, 250_000).is_err());
    for table in ["credit_transactions", "matched_payments", "api_keys"] {
        assert_eq!(count(&db, table), 0, "{table}");
    }
    db.commit_hook(None::<fn() -> bool>).unwrap();
    assert_eq!(
        apply_payments_batch(&db, &master, &items, 250_000)
            .unwrap()
            .len(),
        3
    );
}
