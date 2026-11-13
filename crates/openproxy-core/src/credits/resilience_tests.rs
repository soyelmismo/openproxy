//! Regression tests for partial failures in the payment settlement path.
use super::{ledger, matcher, orders, tiers};
use openproxy_db::MasterKey;
use openproxy_types::ids::ApiKeyId;
use rusqlite::Connection;

fn database() -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    openproxy_db::migrations::run(&mut conn).unwrap();
    conn
}

fn key(conn: &Connection) -> ApiKeyId {
    conn.execute(
        "INSERT INTO api_keys (key_hash) VALUES ('resilience-fixture')",
        [],
    )
    .unwrap();
    ApiKeyId(conn.last_insert_rowid())
}

const RECEIPT: &str = "00000000-0000-4000-8000-000000000002";

fn anon(conn: &Connection) -> orders::CreditOrder {
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
        RECEIPT,
    )
    .unwrap()
}

#[test]
fn failed_settlement_rolls_back_and_same_log_can_retry() {
    let conn = database();
    let master = MasterKey::generate().unwrap();
    let order = anon(&conn);
    conn.execute_batch("CREATE TRIGGER fail_credit BEFORE INSERT ON credit_transactions BEGIN SELECT RAISE(ABORT, 'injected ledger failure'); END").unwrap();
    assert!(
        matcher::apply_payment(
            &conn,
            &master,
            "failure-fixture",
            0,
            100,
            order.amount_units,
            None
        )
        .is_err()
    );
    assert_eq!(
        orders::get_order(&conn, &order.order_code)
            .unwrap()
            .unwrap()
            .status,
        "pending"
    );
    for table in ["matched_payments", "credit_transactions", "api_keys"] {
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "partial settlement leaked into {table}");
    }
    conn.execute_batch("DROP TRIGGER fail_credit").unwrap();
    let result = matcher::apply_payment(
        &conn,
        &master,
        "failure-fixture",
        0,
        100,
        order.amount_units,
        None,
    )
    .unwrap();
    assert!(matches!(result, matcher::PaymentOutcome::Credited { .. }));
}

#[test]
fn anonymous_order_links_its_minted_key() {
    let conn = database();
    let master = MasterKey::generate().unwrap();
    let order = anon(&conn);
    let result = matcher::apply_payment(
        &conn,
        &master,
        "mint-fixture",
        0,
        100,
        order.amount_units,
        None,
    )
    .unwrap();
    let matcher::PaymentOutcome::Credited { api_key_id, .. } = result else {
        panic!("not credited")
    };
    assert_eq!(
        orders::get_order(&conn, &order.order_code)
            .unwrap()
            .unwrap()
            .api_key_id,
        Some(api_key_id)
    );
}

#[test]
fn leveraged_payment_reconciles_credit_not_charge() {
    let conn = database();
    let kid = key(&conn);
    let master = MasterKey::generate().unwrap();
    let tier = tiers::create(
        &conn,
        "leveraged fixture",
        &tiers::TierConfig::default(),
        &tiers::PlanCommerce {
            price_usd: Some(5.0),
            credit_usd: Some(30.0),
            ttl_days: 7,
        },
    )
    .unwrap();
    let order = orders::create_order(&conn, Some(kid), None, 5_000_000, 7, Some(tier.id)).unwrap();
    matcher::apply_payment(
        &conn,
        &master,
        "leverage-fixture",
        0,
        100,
        order.amount_units,
        None,
    )
    .unwrap();
    assert_eq!(ledger::balance_units(&conn, kid).unwrap(), 30_000_000);
    let audit = openproxy_db::credits_ledger::reconcile(&conn).unwrap();
    assert!(audit.is_consistent(), "{audit:?}");
}

#[test]
fn missing_existing_key_does_not_receive_or_burn_payment() {
    let conn = database();
    let kid = key(&conn);
    let master = MasterKey::generate().unwrap();
    let order = orders::create_order(&conn, Some(kid), None, 5_000_000, 7, None).unwrap();
    conn.execute("DELETE FROM api_keys WHERE id = ?1", [kid.0])
        .unwrap();
    assert!(matches!(
        matcher::apply_payment(
            &conn,
            &master,
            "deleted-key-fixture",
            0,
            100,
            order.amount_units,
            None
        )
        .unwrap(),
        matcher::PaymentOutcome::Unmatched { .. }
    ));
    assert_eq!(
        orders::get_order(&conn, &order.order_code)
            .unwrap()
            .unwrap()
            .status,
        "pending"
    );
    assert!(matches!(
        matcher::apply_payment(
            &conn,
            &master,
            "deleted-key-fixture",
            0,
            100,
            order.amount_units,
            None
        )
        .unwrap(),
        matcher::PaymentOutcome::AlreadySeen { .. }
    ));
}
