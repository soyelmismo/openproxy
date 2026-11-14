//! Creation idempotency survives payment, expiry, response loss and races.
use super::{matcher, orders};
use openproxy_db::MasterKey;
use rusqlite::Connection;

const RECEIPT: &str = "replay-fixture-00000000000000000000001";

fn db() -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    openproxy_db::migrations::run(&mut conn).unwrap();
    conn
}

fn request() -> orders::CreateOrderRequest {
    orders::CreateOrderRequest {
        api_key_id: None,
        payer_email: None,
        base_units: 5_000_000,
        ttl_days: 7,
        tier_id: None,
    }
}

fn create(conn: &Connection) -> orders::CreditOrder {
    orders::create_order_with_receipt(conn, request(), 900, RECEIPT).unwrap()
}

#[test]
fn retry_after_mint_and_ack_keeps_original_anonymous_target() {
    let conn = db();
    let master = MasterKey::generate().unwrap();
    let first = create(&conn);
    matcher::apply_payment(
        &conn,
        &master,
        "replay-paid-fixture",
        0,
        100,
        first.amount_units,
        None,
        250_000,
    )
    .unwrap();
    let minted = create(&conn);
    assert_eq!(first.order_code, minted.order_code);
    assert_eq!(minted.status, "minted");
    assert!(minted.api_key_id.is_some());
    assert!(minted.checkout_key_id.is_none());
    assert_eq!(minted.payment_window_secs, 900);
    matcher::reveal_minted_key_with_receipt(&conn, &master, &first.order_code, RECEIPT)
        .unwrap()
        .unwrap();
    assert!(matcher::acknowledge_minted_key(&conn, &first.order_code, RECEIPT).unwrap());
    assert_eq!(create(&conn).id, first.id);
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM credit_orders", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn minted_ciphertext_sweeps_after_ttl_and_reveal_dies_with_it() {
    let conn = db();
    let master = MasterKey::generate().unwrap();
    let order = create(&conn);
    matcher::apply_payment(
        &conn,
        &master,
        "replay-ttl-fixture",
        0,
        100,
        order.amount_units,
        None,
        250_000,
    )
    .unwrap();
    let minted = create(&conn);
    assert_eq!(minted.status, "minted");

    // Fresh mint: the sweep must not touch it, reveal still works.
    assert_eq!(super::sweep_expired_minted_ciphertexts(&conn).unwrap(), 0);
    assert!(matcher::reveal_minted_key_with_receipt(&conn, &master, &order.order_code, RECEIPT)
        .unwrap()
        .is_some());

    // Backdate the mint past the TTL: the sweep erases the checkout's copy,
    // the settled order stays readable, but the plaintext is gone for good.
    conn.execute(
        "UPDATE credit_orders SET minted_at = datetime('now','-8 days') WHERE id=?1",
        [minted.id],
    )
    .unwrap();
    assert_eq!(super::sweep_expired_minted_ciphertexts(&conn).unwrap(), 1);
    let blob: Option<String> = conn
        .query_row(
            "SELECT minted_key_encrypted FROM credit_orders WHERE id=?1",
            [minted.id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(blob.is_none());
    assert!(matcher::reveal_minted_key_with_receipt(&conn, &master, &order.order_code, RECEIPT)
        .unwrap()
        .is_none());
    // The settled order still replays by receipt (audit, plan history intact).
    assert_eq!(create(&conn).id, order.id);
}

#[test]
fn expired_retry_never_opens_a_second_payment_and_preserves_quote_window() {
    let conn = db();
    let first = create(&conn);
    conn.execute("UPDATE credit_orders SET created_at = datetime('now','-2 days'), expires_at = datetime('now','-2 days','+900 seconds'), status='expired' WHERE id=?1", [first.id]).unwrap();
    let retry = orders::create_order_with_receipt(&conn, request(), 1800, RECEIPT).unwrap();
    assert_eq!(retry.id, first.id);
    assert_eq!(retry.status, "expired");
    assert_eq!(retry.payment_window_secs, 900);
}

#[test]
fn reused_receipt_with_different_purchase_is_rejected() {
    let conn = db();
    let first = create(&conn);
    let mut other = request();
    other.base_units = 9_000_000;
    assert!(orders::create_order_with_receipt(&conn, other, 900, RECEIPT).is_err());
    assert_eq!(create(&conn).id, first.id);
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM credit_orders", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn concurrent_receipt_creation_across_connections_is_atomic() {
    let dir = openproxy_db::testing::TempDir::new("checkout-replay").unwrap();
    let path = dir.path().join("replay.db");
    let mut conn = Connection::open(&path).unwrap();
    openproxy_db::migrations::run(&mut conn).unwrap();
    let start = std::sync::Arc::new(std::sync::Barrier::new(8));
    // Materialize every spawn before joining: lazy join would deadlock at
    // the barrier before the second worker is even started.
    let handles: [_; 8] = std::array::from_fn(|_| {
        let path = path.clone();
        let start = std::sync::Arc::clone(&start);
        std::thread::spawn(move || {
            let conn = Connection::open(path).unwrap();
            conn.busy_timeout(std::time::Duration::from_secs(10))
                .unwrap();
            start.wait();
            create(&conn).order_code
        })
    });
    let mut codes: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    codes.sort();
    codes.dedup();
    assert_eq!(codes.len(), 1);
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM credit_orders", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);
}
