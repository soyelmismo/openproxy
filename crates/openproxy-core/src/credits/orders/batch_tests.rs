use super::*;
use crate::error::CoreError;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const USD: i64 = crate::credits::UNITS_PER_USD;

fn db() -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    openproxy_db::migrations::run(&mut conn).unwrap();
    conn
}

/// 36-char URL-safe receipt (UUID-shaped, as the browser sends it).
fn receipt(n: u8) -> String {
    format!("00000000-0000-4000-8000-00000000000{n}")
}

/// Anonymous, plan-less checkout for `usd` whole dollars.
fn item(receipt: &str, usd: i64) -> BatchOrderRequest {
    BatchOrderRequest {
        request: CreateOrderRequest {
            api_key_id: None,
            payer_email: None,
            base_units: usd * USD,
            ttl_days: 7,
            tier_id: None,
        },
        payment_window_secs: PAYMENT_WINDOW_SECS,
        receipt: receipt.to_string(),
    }
}

fn rows(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM credit_orders", [], |r| r.get(0))
        .unwrap()
}

/// Counts COMMITs so a test can prove the batch shares one transaction.
fn commits(conn: &Connection) -> Arc<AtomicUsize> {
    let n = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&n);
    conn.commit_hook(Some(move || {
        counter.fetch_add(1, Ordering::SeqCst);
        false
    }))
    .unwrap();
    n
}

#[test]
fn batch_creates_every_item_with_a_single_commit() {
    let conn = db();
    let commits = commits(&conn);
    let items: Vec<_> = (0..5).map(|i| item(&receipt(i), 5)).collect();
    let out = create_orders_with_receipts_batch(&conn, &items).expect("batch");
    assert_eq!(out.len(), 5, "index-aligned with the request");
    assert!(out.iter().all(|item| item.is_ok()), "every item committed");
    let orders: Vec<CreditOrder> = out.into_iter().map(|o| o.unwrap()).collect();
    let amounts: std::collections::HashSet<i64> = orders.iter().map(|o| o.amount_units).collect();
    assert_eq!(amounts.len(), 5, "live amounts stay unique inside a batch");
    assert_eq!(rows(&conn), 5);
    assert_eq!(
        commits.load(Ordering::SeqCst),
        1,
        "one fsync per batch, not one per order"
    );
}

#[test]
fn batch_isolates_a_rejected_item() {
    let conn = db();
    let mut bad = item(&receipt(9), 5);
    bad.request.tier_id = Some(9_999_999); // no such plan
    let items = vec![item(&receipt(1), 5), bad, item(&receipt(2), 9)];
    let out = create_orders_with_receipts_batch(&conn, &items).expect("batch");
    assert!(out[0].is_ok());
    assert!(
        matches!(out[1].as_ref(), Err(CoreError::Validation(_))),
        "unknown plan fails only its own item, got {:?}",
        out[1]
    );
    assert!(out[2].is_ok());
    assert_eq!(rows(&conn), 2, "the rejected item wrote nothing");
}

#[test]
fn batch_replays_a_duplicate_receipt_inside_the_same_batch() {
    let conn = db();
    let r = receipt(3);
    let items = vec![item(&r, 5), item(&r, 5)];
    let out = create_orders_with_receipts_batch(&conn, &items).expect("batch");
    let first = out[0].as_ref().expect("first ok");
    let second = out[1].as_ref().expect("replay ok");
    assert_eq!(first.id, second.id, "same order, no second bill");
    assert_eq!(first.order_code, second.order_code);
    assert_eq!(rows(&conn), 1, "an uncommitted row is already replayable");
}

#[test]
fn batch_replay_guard_refuses_a_different_purchase() {
    let conn = db();
    let r = receipt(4);
    let first = create_orders_with_receipts_batch(&conn, &[item(&r, 5)]).expect("batch");
    assert!(first[0].is_ok());
    // Same capability, different money: refused, and nothing new is written.
    let second = create_orders_with_receipts_batch(&conn, &[item(&r, 9)]).expect("batch");
    assert!(
        matches!(second[0].as_ref(), Err(CoreError::Validation(_))),
        "receipt is bound to the first purchase"
    );
    assert_eq!(rows(&conn), 1);
}

#[test]
fn batch_rejects_a_bad_window_or_receipt_per_item() {
    let conn = db();
    let mut short_window = item(&receipt(5), 5);
    short_window.payment_window_secs = 5;
    let bad_receipt = item("too-short", 5);
    let items = vec![short_window, bad_receipt, item(&receipt(6), 5)];
    let out = create_orders_with_receipts_batch(&conn, &items).expect("batch");
    assert!(
        matches!(out[0].as_ref(), Err(CoreError::Validation(_))),
        "window"
    );
    assert!(
        matches!(out[1].as_ref(), Err(CoreError::Validation(_))),
        "receipt"
    );
    assert!(out[2].is_ok());
    assert_eq!(rows(&conn), 1);
}

#[test]
fn batch_commit_failure_publishes_nothing() {
    let conn = db();
    // Abort the COMMIT the way SQLite signals a rejected commit hook.
    conn.commit_hook(Some(|| true)).unwrap();
    let items: Vec<_> = (0..3).map(|i| item(&receipt(i), 5)).collect();
    let out = create_orders_with_receipts_batch(&conn, &items);
    assert!(
        out.is_err(),
        "a failed COMMIT must invalidate the whole batch, not items"
    );
    // Whatever the driver did with the open transaction, no row survives.
    let _ = conn.execute_batch("ROLLBACK");
    assert_eq!(rows(&conn), 0, "nothing is published without a commit");
}

#[test]
fn batch_empty_input_opens_no_transaction() {
    let conn = db();
    let commits = commits(&conn);
    let out = create_orders_with_receipts_batch(&conn, &[]).expect("empty batch");
    assert!(out.is_empty());
    assert_eq!(commits.load(Ordering::SeqCst), 0);
}

// ── registration quota + renewals toggle (hot checkout gates) ──────

fn set_quota(conn: &Connection, max: i64) {
    // 0 = no cap (the gate treats <=0 as unlimited).
    openproxy_db::app_config::save_credits_registration_quota_to_db(conn, max, 0).unwrap();
}

fn set_renewals(conn: &Connection, enabled: bool) {
    openproxy_db::app_config::save_credits_renewals_enabled_to_db(conn, enabled, 0).unwrap();
}

/// A sold identity: kind='customer' rows are what the quota counts.
fn insert_customer_key(conn: &Connection, hash: &str) -> i64 {
    conn.execute(
        "INSERT INTO api_keys(key_hash, kind) VALUES (?1, 'customer')",
        params![hash],
    )
    .unwrap();
    conn.last_insert_rowid()
}

/// A checkout targeting an EXISTING key (top-up / renew / change plan).
fn key_item(conn: &Connection, receipt: &str, usd: i64) -> BatchOrderRequest {
    let kid = insert_customer_key(conn, &format!("quota-test-{receipt}"));
    BatchOrderRequest {
        request: CreateOrderRequest {
            api_key_id: Some(ApiKeyId(kid)),
            payer_email: None,
            base_units: usd * USD,
            ttl_days: 7,
            tier_id: None,
        },
        payment_window_secs: PAYMENT_WINDOW_SECS,
        receipt: receipt.to_string(),
    }
}

#[test]
fn quota_blocks_new_mints_but_not_existing_key_orders() {
    let conn = db();
    set_quota(&conn, 2);
    insert_customer_key(&conn, "sold-1");
    insert_customer_key(&conn, "sold-2");
    let out = create_orders_with_receipts_batch(&conn, &[item(&receipt(1), 5)]).expect("batch");
    assert!(
        matches!(out[0].as_ref(), Err(CoreError::Validation(m)) if m.contains("quota is full")),
        "mint rejected at the cap, got {:?}",
        out[0]
    );
    assert_eq!(rows(&conn), 0, "nothing was written");
    // The SAME setup still sells to an existing key: the quota caps
    // registrations, not commerce with users that already have a key.
    let out = create_orders_with_receipts_batch(&conn, &[key_item(&conn, &receipt(2), 9)])
        .expect("batch");
    assert!(
        out[0].is_ok(),
        "top-up passes the full quota, got {:?}",
        out[0]
    );
}

#[test]
fn quota_below_the_cap_and_hot_raise_unblock_mints() {
    let conn = db();
    set_quota(&conn, 2);
    insert_customer_key(&conn, "sold-1");
    let out = create_orders_with_receipts_batch(&conn, &[item(&receipt(1), 5)]).expect("batch");
    assert!(out[0].is_ok(), "1/2 keeps minting");
    // Fill the cap, then raise it hot: the very next mint passes.
    insert_customer_key(&conn, "sold-2");
    let out = create_orders_with_receipts_batch(&conn, &[item(&receipt(2), 5)]).expect("batch");
    assert!(matches!(out[0], Err(CoreError::Validation(_))));
    set_quota(&conn, 3);
    let out = create_orders_with_receipts_batch(&conn, &[item(&receipt(3), 5)]).expect("batch");
    assert!(
        out[0].is_ok(),
        "raised quota unblocks the next mint, got {:?}",
        out[0]
    );
    // Unset (None) = unlimited.
    set_quota(&conn, 0);
    let out = create_orders_with_receipts_batch(&conn, &[item(&receipt(4), 5)]).expect("batch");
    assert!(out[0].is_ok());
}

#[test]
fn receipt_created_before_the_cap_keeps_its_grace() {
    let conn = db();
    let r = receipt(1);
    let first = create_orders_with_receipts_batch(&conn, &[item(&r, 5)]).expect("batch");
    let order = first[0].as_ref().expect("created while open");
    // The cap fills afterwards (another sale + quota set to that count).
    insert_customer_key(&conn, "sold-1");
    set_quota(&conn, 1);
    let out = create_orders_with_receipts_batch(&conn, &[item(&r, 5)]).expect("batch");
    let replay = out[0]
        .as_ref()
        .expect("a buyer who already holds the QR must never see quota-full after paying");
    assert_eq!(replay.id, order.id, "same order, no second bill");
}

#[test]
fn renewals_toggle_gates_only_existing_key_orders() {
    let conn = db();
    set_renewals(&conn, false);
    // Mints are unaffected: they follow the quota, not this toggle.
    let out = create_orders_with_receipts_batch(&conn, &[item(&receipt(1), 5)]).expect("batch");
    assert!(
        out[0].is_ok(),
        "mint passes with renewals off, got {:?}",
        out[0]
    );
    // An existing key cannot buy: plan purchases on keys are disabled.
    let out = create_orders_with_receipts_batch(&conn, &[key_item(&conn, &receipt(2), 9)])
        .expect("batch");
    assert!(
        matches!(out[0].as_ref(), Err(CoreError::Validation(m)) if m.contains("disabled")),
        "key_id order rejected, got {:?}",
        out[0]
    );
    // But an order created BEFORE the switch flipped still completes: the
    // replay check runs before the gate, same as the quota's grace.
    set_renewals(&conn, true);
    let kid = insert_customer_key(&conn, "quota-test-replay");
    let pre_toggle = BatchOrderRequest {
        request: CreateOrderRequest {
            api_key_id: Some(ApiKeyId(kid)),
            payer_email: None,
            base_units: 7 * USD,
            ttl_days: 7,
            tier_id: None,
        },
        payment_window_secs: PAYMENT_WINDOW_SECS,
        receipt: receipt(3),
    };
    let created = create_orders_with_receipts_batch(&conn, &[pre_toggle.clone()]).expect("batch");
    let order = created[0].as_ref().expect("created while open");
    set_renewals(&conn, false);
    let out = create_orders_with_receipts_batch(&conn, &[pre_toggle]).expect("batch");
    let replay = out[0]
        .as_ref()
        .expect("pre-toggle order replays after the flip");
    assert_eq!(replay.id, order.id, "same order, no second bill");
}

// ── per-plan stock (000130) ──────────────────────────────────────────

use crate::credits::tiers::{self, PlanCommerce, TierConfig};

fn make_tier(conn: &Connection, name: &str, stock: i64) -> i64 {
    tiers::create(
        conn,
        name,
        &TierConfig::default(),
        &PlanCommerce {
            price_usd: Some(5.0),
            credit_usd: None,
            ttl_days: 7,
            stock: Some(stock),
        },
    )
    .expect("tier")
    .id
}

fn set_tier_stock(conn: &Connection, tier_id: i64, stock: i64) {
    // Hot seat raise: the same shape the admin PATCH uses — the FULL
    // commerce payload, only stock changed (an absent price would wipe
    // the plan's sale price).
    let current = tiers::get(conn, tier_id).expect("tier").expect("exists");
    tiers::update(
        conn,
        tier_id,
        None,
        None,
        None,
        Some(&PlanCommerce {
            price_usd: current.price_usd,
            credit_usd: current.credit_usd,
            ttl_days: current.ttl_days,
            stock: Some(stock),
        }),
    )
    .expect("stock update");
}

/// A checkout targeting an EXISTING key, buying `tier_id`.
fn key_item_for(
    conn: &Connection,
    receipt: &str,
    tier_id: i64,
    key_tier: Option<i64>,
) -> BatchOrderRequest {
    conn.execute(
        "INSERT INTO api_keys(key_hash, kind, tier_id) VALUES (?1, 'customer', ?2)",
        rusqlite::params![format!("stock-test-{receipt}"), key_tier],
    )
    .unwrap();
    let kid = conn.last_insert_rowid();
    BatchOrderRequest {
        request: CreateOrderRequest {
            api_key_id: Some(ApiKeyId(kid)),
            payer_email: None,
            base_units: 0,
            ttl_days: 0,
            tier_id: Some(tier_id),
        },
        payment_window_secs: PAYMENT_WINDOW_SECS,
        receipt: receipt.to_string(),
    }
}

#[test]
fn stock_blocks_mints_and_pending_orders_reserve_their_seat() {
    let conn = db();
    let tier = make_tier(&conn, "Lite", 1);
    // First mint takes the last seat AS A RESERVATION (pending order).
    let out = create_orders_with_receipts_batch(
        &conn,
        &[BatchOrderRequest {
            request: CreateOrderRequest {
                api_key_id: None,
                payer_email: None,
                base_units: 0,
                ttl_days: 0,
                tier_id: Some(tier),
            },
            payment_window_secs: PAYMENT_WINDOW_SECS,
            receipt: receipt(1),
        }],
    )
    .expect("batch");
    assert!(out[0].is_ok(), "first seat sells, got {:?}", out[0]);
    // A second buyer cannot take a seat that is only reserved: both a
    // mint AND a key-bound plan CHANGE into the full plan bounce.
    let out = create_orders_with_receipts_batch(
        &conn,
        &[BatchOrderRequest {
            request: CreateOrderRequest {
                api_key_id: None,
                payer_email: None,
                base_units: 0,
                ttl_days: 0,
                tier_id: Some(tier),
            },
            payment_window_secs: PAYMENT_WINDOW_SECS,
            receipt: receipt(2),
        }],
    )
    .expect("batch");
    assert!(
        matches!(out[0].as_ref(), Err(CoreError::Validation(m)) if m.contains("full") && m.contains("1/1")),
        "pending reservation blocks the next mint, got {:?}",
        out[0]
    );
    let out =
        create_orders_with_receipts_batch(&conn, &[key_item_for(&conn, &receipt(3), tier, None)])
            .expect("batch");
    assert!(
        matches!(out[0].as_ref(), Err(CoreError::Validation(_))),
        "plan change into a full plan bounces, got {:?}",
        out[0]
    );
    // An UNLIMITED plan (stock 0) never blocks.
    let free = make_tier(&conn, "Unlimited", 0);
    let out =
        create_orders_with_receipts_batch(&conn, &[key_item_for(&conn, &receipt(4), free, None)])
            .expect("batch");
    assert!(out[0].is_ok());
}

#[test]
fn renewing_own_plan_takes_no_seat() {
    let conn = db();
    let tier = make_tier(&conn, "Solo", 1);
    let out = create_orders_with_receipts_batch(
        &conn,
        &[key_item_for(&conn, &receipt(1), tier, Some(tier))],
    )
    .expect("batch");
    assert!(
        out[0].is_ok(),
        "a key renewing its own full plan is fine: it already holds the seat, got {:?}",
        out[0]
    );
    // But switching INTO this full plan from another plan is a new seat.
    let other = make_tier(&conn, "Other", 1);
    let out = create_orders_with_receipts_batch(
        &conn,
        &[key_item_for(&conn, &receipt(2), tier, Some(other))],
    )
    .expect("batch");
    assert!(matches!(out[0].as_ref(), Err(CoreError::Validation(_))));
}

#[test]
fn seat_grace_and_hot_stock_raise() {
    let conn = db();
    let tier = make_tier(&conn, "Grace", 1);
    let mint = BatchOrderRequest {
        request: CreateOrderRequest {
            api_key_id: None,
            payer_email: None,
            base_units: 0,
            ttl_days: 0,
            tier_id: Some(tier),
        },
        payment_window_secs: PAYMENT_WINDOW_SECS,
        receipt: receipt(1),
    };
    let created = create_orders_with_receipts_batch(&conn, &[mint.clone()]).expect("batch");
    let order = created[0].as_ref().expect("sold while seats were left");
    // The plan fills (a key lands on it); the pending order replays anyway.
    conn.execute(
        "INSERT INTO api_keys(key_hash, kind, tier_id) VALUES ('sold-seat', 'customer', ?1)",
        rusqlite::params![tier],
    )
    .unwrap();
    let out = create_orders_with_receipts_batch(&conn, &[mint]).expect("batch");
    let replay = out[0].as_ref().expect("pre-fill order keeps its grace");
    assert_eq!(replay.id, order.id, "same order, no second bill");
    // Fresh mints are blocked until the operator raises the stock hot.
    let out = create_orders_with_receipts_batch(
        &conn,
        &[BatchOrderRequest {
            request: CreateOrderRequest {
                api_key_id: None,
                payer_email: None,
                base_units: 0,
                ttl_days: 0,
                tier_id: Some(tier),
            },
            payment_window_secs: PAYMENT_WINDOW_SECS,
            receipt: receipt(2),
        }],
    )
    .expect("batch");
    assert!(matches!(out[0], Err(CoreError::Validation(_))));
    set_tier_stock(&conn, tier, 3);
    let out = create_orders_with_receipts_batch(
        &conn,
        &[BatchOrderRequest {
            request: CreateOrderRequest {
                api_key_id: None,
                payer_email: None,
                base_units: 0,
                ttl_days: 0,
                tier_id: Some(tier),
            },
            payment_window_secs: PAYMENT_WINDOW_SECS,
            receipt: receipt(3),
        }],
    )
    .expect("batch");
    assert!(
        out[0].is_ok(),
        "raised stock reopens the plan, got {:?}",
        out[0]
    );
}
