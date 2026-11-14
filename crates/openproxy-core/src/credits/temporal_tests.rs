//! Temporal correlation, delivery recovery and plan assignment regressions.
use super::{ledger, matcher, orders, tiers};
use openproxy_db::MasterKey;
use openproxy_types::ids::ApiKeyId;
use rusqlite::{Connection, params};

fn db() -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    openproxy_db::migrations::run(&mut conn).unwrap();
    conn
}
fn key(conn: &Connection) -> ApiKeyId {
    conn.execute(
        "INSERT INTO api_keys(key_hash) VALUES ('temporal-fixture')",
        [],
    )
    .unwrap();
    ApiKeyId(conn.last_insert_rowid())
}
fn plan(conn: &Connection, name: &str, config: tiers::TierConfig) -> tiers::CreditTier {
    tiers::create(
        conn,
        name,
        &config,
        &tiers::PlanCommerce {
            price_usd: Some(5.0),
            credit_usd: None,
            ttl_days: 7,
        },
    )
    .unwrap()
}
const RECEIPT: &str = "00000000-0000-4000-8000-000000000001";

fn anon(conn: &Connection, base_units: i64, ttl_days: i64) -> orders::CreditOrder {
    orders::create_order_with_receipt(
        conn,
        orders::CreateOrderRequest {
            api_key_id: None,
            payer_email: None,
            base_units,
            ttl_days,
            tier_id: None,
        },
        orders::PAYMENT_WINDOW_SECS,
        &uuid::Uuid::new_v4().to_string(),
    )
    .unwrap()
}

#[test]
fn delayed_scan_uses_block_time_and_does_not_credit_newer_order() {
    let conn = db();
    let master = MasterKey::generate().unwrap();
    let old = anon(&conn, 5_000_000, 7);
    conn.execute(
        "UPDATE credit_orders SET created_at = datetime('now','-2 hours'), \
        expires_at = datetime('now','-100 minutes'), status = 'expired' WHERE id = ?1",
        [old.id],
    )
    .unwrap();
    let new = anon(&conn, 5_000_000, 7);
    conn.execute(
        "UPDATE credit_orders SET amount_units = ?1 WHERE id = ?2",
        params![old.amount_units, new.id],
    )
    .unwrap();
    let then: i64 = conn
        .query_row("SELECT unixepoch('now','-110 minutes')", [], |r| r.get(0))
        .unwrap();
    let result = matcher::apply_payment_at(
        &conn,
        &master,
        "old-chain-fixture",
        0,
        matcher::PaymentBlock {
            number: 100,
            timestamp: then,
        },
        old.amount_units,
        None,
    
        250_000)
    .unwrap();
    assert!(
        matches!(result, matcher::PaymentOutcome::Credited { order_code, .. } if order_code == old.order_code)
    );
    assert_eq!(
        orders::get_order(&conn, &new.order_code)
            .unwrap()
            .unwrap()
            .status,
        "pending"
    );
    let result = matcher::apply_payment(
        &conn,
        &master,
        "new-chain-fixture",
        0,
        101,
        old.amount_units,
        None,
    
        250_000)
    .unwrap();
    assert!(
        matches!(result, matcher::PaymentOutcome::Credited { order_code, .. } if order_code == new.order_code)
    );
}

#[test]
fn ambiguous_legacy_amount_is_unmatched_not_assigned_to_first_row() {
    let conn = db();
    let master = MasterKey::generate().unwrap();
    let old = anon(&conn, 5_000_000, 7);
    conn.execute(
        "UPDATE credit_orders SET status = 'expired' WHERE id = ?1",
        [old.id],
    )
    .unwrap();
    let new = anon(&conn, 5_000_000, 7);
    conn.execute(
        "UPDATE credit_orders SET amount_units = ?1 WHERE id = ?2",
        params![old.amount_units, new.id],
    )
    .unwrap();
    let amount_matches = old.amount_units;
    assert!(matches!(
        matcher::apply_payment(
            &conn,
            &master,
            "ambiguous-fixture",
            0,
            100,
            amount_matches,
            None
        ,
            250_000)
        .unwrap(),
        matcher::PaymentOutcome::Unmatched { .. }
    ));
    assert_eq!(
        orders::get_order(&conn, &old.order_code)
            .unwrap()
            .unwrap()
            .status,
        "expired"
    );
    assert_eq!(
        orders::get_order(&conn, &new.order_code)
            .unwrap()
            .unwrap()
            .status,
        "pending"
    );
}

#[test]
fn receipt_retries_lost_response_until_ack_and_excludes_other_receipts() {
    let conn = db();
    let master = MasterKey::generate().unwrap();
    let receipt = "00000000-0000-4000-8000-000000000000";
    let order = orders::create_order_with_receipt(
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
    )
    .unwrap();
    matcher::apply_payment(
        &conn,
        &master,
        "delivery-fixture",
        0,
        100,
        order.amount_units,
        None,
    
        250_000)
    .unwrap();
    let stranger = "10000000-0000-4000-8000-000000000000";
    let first = matcher::reveal_minted_key_with_receipt(&conn, &master, &order.order_code, receipt)
        .unwrap()
        .unwrap();
    assert_eq!(
        matcher::reveal_minted_key_with_receipt(&conn, &master, &order.order_code, receipt)
            .unwrap(),
        Some(first)
    );
    assert!(
        matcher::reveal_minted_key_with_receipt(&conn, &master, &order.order_code, stranger)
            .unwrap()
            .is_none()
    );
    assert!(!matcher::acknowledge_minted_key(&conn, &order.order_code, stranger).unwrap());
    assert!(matcher::acknowledge_minted_key(&conn, &order.order_code, receipt).unwrap());
    assert!(matcher::acknowledge_minted_key(&conn, &order.order_code, receipt).unwrap());
    assert!(
        matcher::reveal_minted_key_with_receipt(&conn, &master, &order.order_code, receipt)
            .unwrap()
            .is_none()
    );
}

#[test]
fn admin_one_to_one_plan_grants_balance_and_exhaustion_does_not_refill() {
    let conn = db();
    let kid = key(&conn);
    let tier = plan(&conn, "one to one", tiers::TierConfig::default());
    assert!(tiers::grant_plan(&conn, kid, &tier).unwrap());
    assert_eq!(ledger::balance_units(&conn, kid).unwrap(), 5_000_000);
    ledger::record_spend(&conn, kid, 5_000_000).unwrap();
    assert!(!tiers::grant_plan(&conn, kid, &tier).unwrap());
    assert_eq!(ledger::balance_units(&conn, kid).unwrap(), 0);
}

#[test]
fn plan_switch_replaces_contract_and_prevents_free_grant_after_purchase() {
    let conn = db();
    let kid = key(&conn);
    let master = MasterKey::generate().unwrap();
    let limited = plan(
        &conn,
        "restricted",
        tiers::TierConfig {
            allowed_models: Some(vec!["only-one".into()]),
            allowed_providers: Some(vec!["provider".into()]),
            ..Default::default()
        },
    );
    tiers::grant_plan(&conn, kid, &limited).unwrap();
    let unrestricted = plan(&conn, "unrestricted", tiers::TierConfig::default());
    let order =
        orders::create_order(&conn, Some(kid), None, 999, 1, Some(unrestricted.id)).unwrap();
    assert_eq!(
        order.base_units, 5_000_000,
        "commerce resolved atomically from tier"
    );
    assert_eq!(order.ttl_days, 7);
    matcher::apply_payment(
        &conn,
        &master,
        "switch-fixture",
        0,
        100,
        order.amount_units,
        None,
    
        250_000)
    .unwrap();
    let shape: (Option<String>, Option<String>, i64) = conn.query_row(
        "SELECT allowed_models_json, allowed_providers_json, only_whitelisted FROM api_keys WHERE id = ?1", [kid.0], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
    assert_eq!(shape, (None, None, 0));
    assert_eq!(ledger::balance_units(&conn, kid).unwrap(), 5_000_000);
    assert!(!tiers::grant_plan(&conn, kid, &unrestricted).unwrap());
    assert_eq!(ledger::balance_units(&conn, kid).unwrap(), 5_000_000);
}

#[test]
fn grant_failure_rolls_back_previous_balance_and_contract() {
    let conn = db();
    let kid = key(&conn);
    let first = plan(&conn, "first", tiers::TierConfig::default());
    tiers::grant_plan(&conn, kid, &first).unwrap();
    let second = plan(
        &conn,
        "second",
        tiers::TierConfig {
            allowed_models: Some(vec!["new-model".into()]),
            ..Default::default()
        },
    );
    conn.execute_batch("CREATE TRIGGER fail_grant BEFORE INSERT ON credit_transactions BEGIN SELECT RAISE(ABORT, 'injected grant failure'); END").unwrap();
    assert!(tiers::grant_plan(&conn, kid, &second).is_err());
    assert_eq!(ledger::balance_units(&conn, kid).unwrap(), 5_000_000);
    let tier: i64 = conn
        .query_row("SELECT tier_id FROM api_keys WHERE id = ?1", [kid.0], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(tier, first.id);
}

#[test]
fn deleting_paid_key_preserves_reconciliation_and_cancels_pending_purchase() {
    let conn = db();
    let kid = key(&conn);
    let master = MasterKey::generate().unwrap();
    let paid = orders::create_order(&conn, Some(kid), None, 5_000_000, 7, None).unwrap();
    matcher::apply_payment(
        &conn,
        &master,
        "retained-fixture",
        0,
        100,
        paid.amount_units,
        None,
    
        250_000)
    .unwrap();
    let pending = orders::create_order(&conn, Some(kid), None, 5_000_000, 7, None).unwrap();
    crate::api_keys::hard_delete(&conn, kid).unwrap();
    assert_eq!(ledger::balance_units(&conn, kid).unwrap(), 0);
    assert!(
        openproxy_db::credits_ledger::reconcile(&conn)
            .unwrap()
            .is_consistent()
    );
    assert_eq!(
        orders::get_order(&conn, &pending.order_code)
            .unwrap()
            .unwrap()
            .status,
        "cancelled"
    );
    assert!(matches!(
        matcher::apply_payment(
            &conn,
            &master,
            "cancelled-fixture",
            0,
            101,
            pending.amount_units,
            None
        ,
            250_000)
        .unwrap(),
        matcher::PaymentOutcome::Unmatched { .. }
    ));
}

#[test]
fn configured_payment_window_is_persisted_and_invalid_windows_fail() {
    let conn = db();
    let kid = key(&conn);
    let order =
        orders::create_order_with_window(&conn, Some(kid), None, 5_000_000, 7, None, 1800).unwrap();
    let duration: i64 = conn
        .query_row(
            "SELECT unixepoch(expires_at)-unixepoch(created_at) FROM credit_orders WHERE id=?1",
            [order.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(duration, 1800);
    assert!(
        orders::create_order_with_window(&conn, Some(kid), None, 5_000_000, 7, None, 0).is_err()
    );
    assert!(
        orders::create_order_with_window(&conn, Some(kid), None, 5_000_000, 7, None, i64::MAX)
            .is_err()
    );
    // The receipt-bound path validates the same window before it touches money.
    assert!(
        orders::create_order_with_receipt(
            &conn,
            orders::CreateOrderRequest {
                api_key_id: None,
                payer_email: None,
                base_units: 5_000_000,
                ttl_days: 7,
                tier_id: None,
            },
            0,
            RECEIPT,
        )
        .is_err()
    );
}

#[test]
fn invalid_money_and_lifetimes_are_rejected_before_checkout() {
    let conn = db();
    for amount in [f64::NAN, f64::INFINITY, -1.0, 0.0, 0.000_000_1, 1e30] {
        assert!(super::usd_to_units(amount).is_none());
        assert!(
            tiers::create(
                &conn,
                "invalid",
                &Default::default(),
                &tiers::PlanCommerce {
                    price_usd: Some(amount),
                    credit_usd: None,
                    ttl_days: 7
                }
            )
            .is_err()
        );
    }
    assert_eq!(super::usd_to_units(4.99), Some(4_990_000));
    assert_eq!(super::units_to_usd_str(i64::MIN), "-9223372036854.775808");
}
