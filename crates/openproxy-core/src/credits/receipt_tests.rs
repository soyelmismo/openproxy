//! Receipt-bound delivery regressions: the receipt is a capability fixed at
//! creation, a leaked order code is not enough to claim a key, retries of the
//! same receipt survive a lost response, ACK is idempotent, and legacy rows
//! without a bound receipt fail closed instead of being claimable.
use super::{matcher, orders};
use openproxy_db::MasterKey;
use openproxy_types::ids::ApiKeyId;
use rusqlite::{Connection, params};

const A: &str = "00000000-0000-4000-8000-00000000000a";
const B: &str = "00000000-0000-4000-8000-00000000000b";

fn db() -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    openproxy_db::migrations::run(&mut conn).unwrap();
    conn
}

fn key(conn: &Connection) -> ApiKeyId {
    conn.execute(
        "INSERT INTO api_keys(key_hash) VALUES ('receipt-fixture')",
        [],
    )
    .unwrap();
    ApiKeyId(conn.last_insert_rowid())
}

fn anon(conn: &Connection, receipt: &str) -> orders::CreditOrder {
    orders::create_order_with_receipt(
        conn,
        orders::CreateOrderRequest {
            api_key_id: None,
            payer_email: None,
            base_units: 5_000_000,
            ttl_days: 7,
            tier_id: None,
        },
        orders::PAYMENT_WINDOW_SECS,
        receipt,
    )
    .unwrap()
}

fn paid(conn: &Connection, master: &MasterKey, order: &orders::CreditOrder, tx: &str) {
    matcher::apply_payment(conn, master, tx, 0, 100, order.amount_units, None).unwrap();
}

fn delivery_state(conn: &Connection, id: i64) -> (Option<String>, i64, Option<Vec<u8>>) {
    conn.query_row(
        "SELECT reveal_receipt_hash, key_revealed, minted_key_encrypted \
         FROM credit_orders WHERE id = ?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .unwrap()
}

#[test]
fn creation_binds_receipt_and_first_reveal_attack_fails() {
    let conn = db();
    let master = MasterKey::generate().unwrap();
    let order = anon(&conn, A);
    let expected = crate::api_keys::models::hash_key(A);
    let (hash, revealed, ciphertext) = delivery_state(&conn, order.id);
    assert_eq!(
        hash.as_deref(),
        Some(expected.as_str()),
        "the hash, never the plaintext receipt, is persisted"
    );
    assert_eq!(revealed, 0);
    assert!(ciphertext.is_none(), "nothing to deliver before payment");

    paid(&conn, &master, &order, "first-reveal-fixture");

    // Attacker knows the public order code but not the receipt: cannot claim.
    assert!(
        matcher::reveal_minted_key_with_receipt(&conn, &master, &order.order_code, B)
            .unwrap()
            .is_none(),
        "a code-only caller must never win the first reveal"
    );
    let (hash, revealed, ciphertext) = delivery_state(&conn, order.id);
    assert_eq!(
        hash.as_deref(),
        Some(expected.as_str()),
        "a failed claim must not overwrite the bound receipt"
    );
    assert_eq!(revealed, 0);
    assert!(ciphertext.is_some(), "the legitimate buyer can still retry");

    // The receipt bound at creation is the only one that reads the key.
    let delivered = matcher::reveal_minted_key_with_receipt(&conn, &master, &order.order_code, A)
        .unwrap()
        .expect("bound receipt reveals");
    assert!(delivered.starts_with("op_live_"));
}

#[test]
fn invalid_receipts_are_rejected_before_any_order_exists() {
    let conn = db();
    let long = "a".repeat(129);
    let unicode = "ü".repeat(20);
    let cases: [&str; 6] = [
        "",
        "short",
        long.as_str(),
        "00000000 0000 4000 8000 00000000000a",
        unicode.as_str(),
        "00000000-0000-4000-8000-0000000!",
    ];
    for receipt in cases {
        let result = orders::create_order_with_receipt(
            &conn,
            orders::CreateOrderRequest {
                api_key_id: None,
                payer_email: None,
                base_units: 5_000_000,
                ttl_days: 7,
                tier_id: None,
            },
            orders::PAYMENT_WINDOW_SECS,
            receipt,
        );
        assert!(
            matches!(&result, Err(crate::error::CoreError::Validation(_))),
            "receipt {receipt:?} must be rejected as validation, got {result:?}"
        );
    }
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM credit_orders", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0, "a rejected receipt must not create an order");

    // Both common shapes are accepted: UUID and base64url.
    for ok in [A, "AZaz09_-_AZaz09_-_AZaz09_-_AZaz09_-_A"] {
        let order = anon(&conn, ok);
        assert_eq!(order.status, "pending");
    }
}

#[test]
fn anonymous_creation_without_receipt_fails_closed() {
    let conn = db();
    assert!(
        orders::create_order(&conn, None, None, 5_000_000, 7, None).is_err(),
        "the legacy wrapper must not mint a deliverable key without a receipt"
    );
    assert!(orders::create_order_with_window(&conn, None, None, 5_000_000, 7, None, 900).is_err());
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM credit_orders", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);

    // Key-bound top-ups do not mint a key and keep working through the legacy
    // wrapper.
    let kid = key(&conn);
    let order = orders::create_order(&conn, Some(kid), None, 5_000_000, 7, None).unwrap();
    assert_eq!(order.api_key_id, Some(kid.0));
}

#[test]
fn ack_is_idempotent_and_reveal_stays_retryable_until_ack() {
    let conn = db();
    let master = MasterKey::generate().unwrap();
    let order = anon(&conn, A);
    paid(&conn, &master, &order, "ack-idempotent-fixture");

    // ACK before a reveal must not destroy an undelivered key.
    assert!(!matcher::acknowledge_minted_key(&conn, &order.order_code, A).unwrap());
    let (_, revealed, ciphertext) = delivery_state(&conn, order.id);
    assert_eq!(revealed, 0);
    assert!(ciphertext.is_some());

    let first = matcher::reveal_minted_key_with_receipt(&conn, &master, &order.order_code, A)
        .unwrap()
        .expect("first reveal");
    let retry = matcher::reveal_minted_key_with_receipt(&conn, &master, &order.order_code, A)
        .unwrap()
        .expect("retry after a lost response");
    assert_eq!(first, retry);

    // Wrong receipt never acknowledges (and never wipes).
    assert!(!matcher::acknowledge_minted_key(&conn, &order.order_code, B).unwrap());
    assert!(
        matcher::reveal_minted_key_with_receipt(&conn, &master, &order.order_code, A)
            .unwrap()
            .is_some()
    );

    assert!(matcher::acknowledge_minted_key(&conn, &order.order_code, A).unwrap());
    // Repeated ACK with the SAME receipt is a success even with no ciphertext.
    assert!(matcher::acknowledge_minted_key(&conn, &order.order_code, A).unwrap());
    assert!(matcher::acknowledge_minted_key(&conn, &order.order_code, A).unwrap());
    let (_, revealed, ciphertext) = delivery_state(&conn, order.id);
    assert_eq!(revealed, 1);
    assert!(ciphertext.is_none(), "ack wipes the encrypted copy");
    assert!(
        matcher::reveal_minted_key_with_receipt(&conn, &master, &order.order_code, A)
            .unwrap()
            .is_none(),
        "after ack nothing can be revealed, not even with the right receipt"
    );
}

#[test]
fn legacy_order_without_receipt_hash_fails_closed() {
    let conn = db();
    let master = MasterKey::generate().unwrap();
    let order = anon(&conn, A);
    paid(&conn, &master, &order, "legacy-migration-fixture");
    // Simulate a row minted before receipt binding existed: ciphertext present,
    // no receipt hash. This is the migration surface described in the handoff.
    conn.execute(
        "UPDATE credit_orders SET reveal_receipt_hash = NULL WHERE id = ?1",
        [order.id],
    )
    .unwrap();
    for receipt in [A, B] {
        assert!(
            matcher::reveal_minted_key_with_receipt(&conn, &master, &order.order_code, receipt)
                .unwrap()
                .is_none(),
            "an unbound anonymous order must not be claimable by code + receipt"
        );
    }
    let (_, revealed, ciphertext) = delivery_state(&conn, order.id);
    assert_eq!(revealed, 0, "fail closed leaves the delivery untouched");
    assert!(
        ciphertext.is_some(),
        "the operator still has the encrypted copy for manual resolution"
    );

    // Operator-facing inventory of rows that need a manual decision.
    let pending_manual: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM credit_orders WHERE status = 'minted' \
             AND reveal_receipt_hash IS NULL AND minted_key_encrypted IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(pending_manual, 1);
}

#[test]
fn reveal_before_payment_returns_nothing_and_binding_is_untouched() {
    let conn = db();
    let master = MasterKey::generate().unwrap();
    let order = anon(&conn, A);
    assert!(
        matcher::reveal_minted_key_with_receipt(&conn, &master, &order.order_code, A)
            .unwrap()
            .is_none(),
        "a pending order has no key to deliver"
    );
    let (hash, revealed, ciphertext) = delivery_state(&conn, order.id);
    let expected = crate::api_keys::models::hash_key(A);
    assert_eq!(hash.as_deref(), Some(expected.as_str()));
    assert_eq!(revealed, 0);
    assert!(ciphertext.is_none());

    // Same receipt still works once the payment lands.
    paid(&conn, &master, &order, "before-payment-fixture");
    assert!(
        matcher::reveal_minted_key_with_receipt(&conn, &master, &order.order_code, A)
            .unwrap()
            .is_some()
    );
}

#[test]
fn receipt_binding_is_per_order_not_global() {
    // Different purchase receipts never expose one another's delivery.
    let conn = db();
    let master = MasterKey::generate().unwrap();
    let first = anon(&conn, A);
    let second = anon(&conn, B);
    paid(&conn, &master, &first, "per-order-one");
    let one = matcher::reveal_minted_key_with_receipt(&conn, &master, &first.order_code, A)
        .unwrap()
        .expect("first delivered");
    assert!(
        matcher::reveal_minted_key_with_receipt(&conn, &master, &second.order_code, A)
            .unwrap()
            .is_none(),
        "the unpaid order must not deliver anything"
    );
    assert_eq!(
        matcher::reveal_minted_key_with_receipt(&conn, &master, &first.order_code, A).unwrap(),
        Some(one),
        "retry stays stable"
    );
}

#[test]
fn receipt_hash_query_uses_bound_value_for_every_order_shape() {
    // Plan-backed anonymous checkout (tier resolved inside the transaction)
    // also stores the hash at creation.
    let conn = db();
    let tier = super::tiers::create(
        &conn,
        "receipt plan",
        &super::tiers::TierConfig::default(),
        &super::tiers::PlanCommerce {
            price_usd: Some(5.0),
            credit_usd: None,
            ttl_days: 7,
        },
    )
    .unwrap();
    let order = orders::create_order_with_receipt(
        &conn,
        orders::CreateOrderRequest {
            api_key_id: None,
            payer_email: Some("buyer@example.com".into()),
            base_units: 0,
            ttl_days: 0,
            tier_id: Some(tier.id),
        },
        orders::PAYMENT_WINDOW_SECS,
        A,
    )
    .unwrap();
    let stored: Option<String> = conn
        .query_row(
            "SELECT reveal_receipt_hash FROM credit_orders WHERE id = ?1",
            params![order.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, Some(crate::api_keys::models::hash_key(A)));
    assert_ne!(stored.as_deref(), Some(A));
}
