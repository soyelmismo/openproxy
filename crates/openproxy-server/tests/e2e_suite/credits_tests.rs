//! E2E of the on-chain credit gateway, WITHOUT real money.
//!
//! The checkout surface is exercised through the real HTTP handlers and the
//! real matcher/ledger path. The watcher's own scan loop (mock JSON-RPC,
//! bootstrap lookback, finality) lives in `watcher_tests.rs` so each file
//! stays small.
//!
//! What must hold: exactly one credit per payment, exact-amount matching,
//! receipt-bound one-time reveal, balance equals the sum of paid base
//! amounts, and the history always points back into the chain explorer.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::harness::TestHarness;
use openproxy_core::credits::{PaymentOutcome, apply_payment};
use serde_json::json;
use std::sync::Arc;

/// Anonymous order creation REQUIRES the delivery receipt, so it must be sent
/// from the start; the same value is what later reveals the key.
async fn create_anonymous_order(
    h: &TestHarness,
    plan: i64,
    receipt: &str,
) -> (axum::http::StatusCode, serde_json::Value) {
    let mut req = h.public_request(
        axum::http::Method::POST,
        "/v1/credits/order",
        json!({"tier_id": plan, "mint_new_key": true}).to_string(),
    );
    req.headers_mut().insert(
        "idempotency-key",
        axum::http::HeaderValue::from_str(receipt).expect("receipt header"),
    );
    h.oneshot_json(req).await
}

/// Seed a sellable plan (price -> credit, TTL days) and return its id.
pub(crate) async fn seed_plan(h: &TestHarness, price: f64, credit: f64, ttl: i64) -> i64 {
    h.db_pool
        .spawn_write(move |w| {
            openproxy_core::credits::tiers::create(
                w,
                &format!("plan-{price}-{ttl}"),
                &openproxy_core::credits::TierConfig::default(),
                &openproxy_core::credits::PlanCommerce {
                    price_usd: Some(price),
                    credit_usd: Some(credit),
                    ttl_days: ttl,
                    stock: None,
                },
            )
            .map(|t| t.id)
        })
        .await
        .expect("seed plan")
}

#[tokio::test]
async fn anonymous_buy_pay_reveal_and_history_e2e() {
    let h = TestHarness::new().await;
    let receipt = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";

    // 1. Anonymous checkout: mint a key for the $10 / 15-day plan (1:1 credit).
    let plan = seed_plan(&h, 10.0, 10.0, 15).await;
    let (status, order) = create_anonymous_order(&h, plan, receipt).await;
    assert_eq!(status, axum::http::StatusCode::OK, "create order: {order}");
    let code = order["order_code"]
        .as_str()
        .expect("order_code")
        .to_string();
    let amount_units = order["amount_units"].as_i64().expect("amount_units");
    let base_units = 10_000_000_i64;
    assert!(
        amount_units >= base_units && amount_units < base_units + 1_000_000,
        "jittered amount {amount_units} inside [base, base+1e6)"
    );
    assert!(
        order["qr_uri"]
            .as_str()
            .unwrap()
            .starts_with("ethereum:0x833589")
    );
    assert_eq!(order["no_refunds"], json!(true));
    assert!(order["qr_svg"].as_str().unwrap().starts_with("<svg"));

    // 2. Status before payment: pending.
    let (status, st) = h
        .oneshot_json(h.public_request(
            axum::http::Method::GET,
            &format!("/v1/credits/order/{code}"),
            "",
        ))
        .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(st["status"], json!("pending"));

    // 3. A stray transfer that matches no live order: no credit, no leak.
    let stray = 25_000_000 + 314_159;
    let mk = Arc::clone(&h.master_key);
    let outcome = h
        .db_pool
        .spawn_write(move |w| apply_payment(w, mk.as_ref(), "0xstray", 0, 100, stray, None, 250_000))
        .await
        .expect("apply stray");
    assert!(matches!(outcome, PaymentOutcome::Unmatched { .. }));

    // 4. The exact payment: credited once.
    let mk = Arc::clone(&h.master_key);
    let outcome = h
        .db_pool
        .spawn_write(move |w| apply_payment(w, mk.as_ref(), "0xpay1", 0, 100, amount_units, None, 250_000))
        .await
        .expect("apply pay1");
    let PaymentOutcome::Credited {
        api_key_id,
        credited_units,
        ..
    } = outcome
    else {
        panic!("expected Credited, got {outcome:?}");
    };
    assert_eq!(
        credited_units, base_units,
        "credit is the base, not the jitter"
    );

    // 5. Same (tx, log) delivered twice by a rescan: no double credit.
    let mk = Arc::clone(&h.master_key);
    let outcome = h
        .db_pool
        .spawn_write(move |w| apply_payment(w, mk.as_ref(), "0xpay1", 0, 100, amount_units, None, 250_000))
        .await
        .expect("replay pay1");
    assert!(matches!(outcome, PaymentOutcome::AlreadySeen { .. }));

    // 6. Status now paid with the chain pointer.
    let (status, st) = h
        .oneshot_json(h.public_request(
            axum::http::Method::GET,
            &format!("/v1/credits/order/{code}"),
            "",
        ))
        .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(
        st["status"],
        json!("minted"),
        "anonymous orders are paid+minted in one step"
    );
    assert_eq!(st["tx_hash"], json!("0xpay1"));

    // 7. Reveal with its receipt; a different receipt cannot claim the key.
    let (status, rev) = h.oneshot_json(reveal_request(&code, Some(receipt))).await;
    assert_eq!(status, axum::http::StatusCode::OK, "reveal: {rev}");
    let key = rev["key"].as_str().expect("revealed key").to_string();
    assert!(key.starts_with("op_"), "key format: {key}");
    let (status, _) = h
        .oneshot_json(reveal_request(
            &code,
            Some("ffffffff-0000-1111-2222-333333333333"),
        ))
        .await;
    assert_eq!(
        status,
        axum::http::StatusCode::CONFLICT,
        "another receipt must not read a delivered key"
    );

    // 8. The plan surface reports the credited balance and its lifetime.
    let kid = api_key_id;
    let (status, q) = h
        .oneshot_json(h.admin_request(
            axum::http::Method::GET,
            &format!("/admin/api/credits/quota?key_id={kid}"),
            "",
        ))
        .await;
    assert_eq!(status, axum::http::StatusCode::OK, "quota: {q}");
    assert_eq!(q["plan"]["credit_usd"], json!(10.0));
    assert_eq!(q["plan"]["balance_usd"], json!(10.0));
    assert!(
        q["plan"]["expires_at"].is_string(),
        "day-15 lot expires: {q}"
    );

    // 9. Exactly one ledger row, and it points back into the chain.
    let rows = h
        .db_pool
        .spawn_read(move |r| openproxy_db::credits_ledger::list_transactions(r, kid, 10))
        .await
        .expect("ledger rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, "topup");
    assert_eq!(rows[0].tx_hash.as_deref(), Some("0xpay1"));
    assert_eq!(rows[0].order_code.as_deref(), Some(code.as_str()));
}

/// Concurrency stress at the HTTP surface: N simultaneous top-up orders for
/// the same key (same base amount, different jitter), every payment applied
/// concurrently with one replay each. No leaks, no duplicates, ledger
/// consistent to the micro-unit.
#[tokio::test]
async fn concurrent_orders_stress_no_leaks_no_duplicates() {
    let h = TestHarness::new().await;
    const N: usize = 8;
    const BASE: i64 = 5_000_000;

    // Create N live orders (same base -> jitter must separate them).
    let plan = seed_plan(&h, 5.0, 5.0, 7).await;
    let kid = h.admin_key_id.0;
    let mut amounts = Vec::with_capacity(N);
    for i in 0..N {
        let (status, order) = h
            .oneshot_json(h.admin_request(
                axum::http::Method::POST,
                "/v1/credits/order",
                json!({"tier_id": plan, "key_id": kid}).to_string(),
            ))
            .await;
        assert_eq!(status, axum::http::StatusCode::OK, "order {i}: {order}");
        let a = order["amount_units"].as_i64().expect("amount_units");
        assert!((BASE..BASE + 1_000_000).contains(&a));
        amounts.push(a);
    }
    // The thin funnel guarantees distinct live amounts: no two orders can
    // share the exact payment amount.
    let mut sorted = amounts.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        N,
        "jitter must make all live amounts distinct"
    );

    // Apply every payment concurrently, each delivered twice (watcher
    // rescan), plus two strays. All through the real spawn_write path.
    let mut handles = Vec::new();
    for (i, amount) in amounts.iter().enumerate() {
        let pool = Arc::clone(&h.db_pool);
        let mk1 = Arc::clone(&h.master_key);
        let mk2 = Arc::clone(&h.master_key);
        let tx1 = format!("0xstress{i:02}");
        let tx2 = tx1.clone();
        let amount = *amount;
        handles.push(tokio::task::spawn(async move {
            let first = pool
                .spawn_write(move |w| apply_payment(w, mk1.as_ref(), &tx1, 0, 100, amount, None, 250_000))
                .await
                .expect("first delivery");
            let second = pool
                .spawn_write(move |w| apply_payment(w, mk2.as_ref(), &tx2, 0, 100, amount, None, 250_000))
                .await
                .expect("replay delivery");
            (first, second)
        }));
    }
    let mut credited = 0;
    let mut replayed = 0;
    for jh in handles {
        let (first, second) = jh.await.expect("task");
        assert!(
            matches!(first, PaymentOutcome::Credited { .. }),
            "first: {first:?}"
        );
        assert!(
            matches!(second, PaymentOutcome::AlreadySeen { .. }),
            "replay: {second:?}"
        );
        credited += 1;
        replayed += 1;
    }
    assert_eq!(credited, N);
    assert_eq!(replayed, N);

    // Final state: balance is the exact sum of base amounts. No micro-cent
    // leaked into or out of the ledger.
    let (units, audit) = h
        .db_pool
        .spawn_write(move |w| {
            let units = openproxy_db::credits_ledger::balance_units(w, kid)?;
            let audit = openproxy_db::credits_ledger::reconcile(w)?;
            Ok((units, audit))
        })
        .await
        .expect("final read");
    assert_eq!(units, BASE * N as i64, "balance == sum of base amounts");
    assert!(audit.is_consistent(), "audit: {audit:?}");
    assert_eq!(audit.paid_orders, N as i64);
    assert_eq!(audit.topup_rows, N as i64);

    // History: one topup row per order, all with chain pointers.
    let rows = h
        .db_pool
        .spawn_read(move |r| openproxy_db::credits_ledger::list_transactions(r, kid, 100))
        .await
        .expect("ledger rows");
    assert_eq!(rows.len(), N, "exactly one topup row per order");
    assert!(
        rows.iter().all(|t| t
            .tx_hash
            .as_deref()
            .is_some_and(|tx| tx.starts_with("0xstress"))),
        "every row carries its chain tx"
    );
}

/// The public checkout page is served without a session and carries the
/// client that drives the public gateway.
#[tokio::test]
async fn checkout_page_serves_without_session() {
    let h = TestHarness::new().await;
    let (status, _, body) = h
        .oneshot(h.public_request(axum::http::Method::GET, "/checkout", ""))
        .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let html = String::from_utf8_lossy(&body);
    assert!(html.contains("OpenProxy credits"), "checkout shell present");
    assert!(html.contains("Plan"), "plan picker present");
    assert!(
        html.to_lowercase().contains("no refunds"),
        "no-refunds notice"
    );
}

/// With the runtime master switch off, every buying surface refuses: the
/// public checkout returns 403 gateway_disabled and the packages route is
/// gone entirely. This is the admin "close the gateway" toggle guarantee.
#[tokio::test]
async fn gateway_toggle_closes_all_buying_surfaces() {
    let h = TestHarness::new().await;
    // Flip the switch off directly in app_config (what the admin POST does).
    h.db_pool
        .spawn_write(|w| openproxy_db::app_config::save_credits_gateway_enabled_to_db(w, false, 0))
        .await
        .expect("save flag");

    let (status, _, body) = h
        .oneshot(h.public_request(
            axum::http::Method::POST,
            "/v1/credits/order",
            r#"{"tier_id": 1, "mint_new_key": true}"#,
        ))
        .await;
    assert_eq!(
        status,
        axum::http::StatusCode::FORBIDDEN,
        "checkout refused"
    );
    assert!(String::from_utf8_lossy(&body).contains("gateway_disabled"));

    let (status, _, _) = h
        .oneshot(h.public_request(axum::http::Method::GET, "/v1/credits/packages", ""))
        .await;
    assert_eq!(
        status,
        axum::http::StatusCode::NOT_FOUND,
        "packages surface gone"
    );

    // Admin API still answers so the toggle can be turned back on.
    let (status, _, _) = h
        .oneshot(h.admin_request(axum::http::Method::GET, "/admin/api/credits/gateway", ""))
        .await;
    assert_eq!(status, axum::http::StatusCode::OK);
}

fn reveal_request(code: &str, receipt: Option<&str>) -> axum::http::Request<axum::body::Body> {
    let mut b = axum::http::Request::builder()
        .method(axum::http::Method::POST)
        .uri(format!("/v1/credits/order/{code}/reveal"))
        .header(axum::http::header::CONTENT_TYPE, "application/json");
    if let Some(r) = receipt {
        b = b.header("idempotency-key", r);
    }
    b.body(axum::body::Body::empty()).expect("reveal request")
}

fn ack_request(code: &str, receipt: &str) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::builder()
        .method(axum::http::Method::POST)
        .uri(format!("/v1/credits/order/{code}/ack"))
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .header("idempotency-key", receipt)
        .body(axum::body::Body::empty())
        .expect("ack request")
}

/// A `key_id` target on the PUBLIC route must prove ownership: anonymous
/// minting stays open, but nobody can spend on (or overwrite the plan of)
/// another tenant's key.
#[tokio::test]
async fn public_create_with_key_id_requires_the_owner_bearer_e2e() {
    let h = TestHarness::new().await;
    let plan = seed_plan(&h, 5.0, 5.0, 7).await;
    let victim = h.admin_key_id.0;

    // No credentials at all → refused.
    let (status, body) = h
        .oneshot_json(h.public_request(
            axum::http::Method::POST,
            "/v1/credits/order",
            json!({"tier_id": plan, "key_id": victim}).to_string(),
        ))
        .await;
    assert_eq!(
        status,
        axum::http::StatusCode::UNAUTHORIZED,
        "anonymous key_id must be refused: {body}"
    );

    // A different tenant's valid key → refused (not someone else's key).
    let (status, body) = h
        .oneshot_json(h.client_request(
            axum::http::Method::POST,
            "/v1/credits/order",
            json!({"tier_id": plan, "key_id": victim}).to_string(),
        ))
        .await;
    assert_eq!(
        status,
        axum::http::StatusCode::FORBIDDEN,
        "cross-tenant key_id must be refused: {body}"
    );

    // Anonymous minting (no key_id) stays unauthenticated.
    let (status, order) =
        create_anonymous_order(&h, plan, "aaaa1111-bbbb-2222-cccc-333333333333").await;
    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "anonymous mint: {order}"
    );
    assert_eq!(
        order["payment_window_secs"],
        json!(900),
        "quoted window is the real 15-minute window"
    );

    // The owner's own Bearer is accepted.
    let (status, order) = h
        .oneshot_json(h.admin_request(
            axum::http::Method::POST,
            "/v1/credits/order",
            json!({"tier_id": plan, "key_id": victim}).to_string(),
        ))
        .await;
    assert_eq!(status, axum::http::StatusCode::OK, "owner key_id: {order}");
}

/// The receipt flow: same receipt retries the reveal and then acks it.
#[tokio::test]
async fn reveal_with_receipt_is_retryable_until_ack_e2e() {
    let h = TestHarness::new().await;
    let receipt = "01234567-89ab-cdef-0123-456789abcdef"; // 36 chars
    let plan = seed_plan(&h, 10.0, 10.0, 15).await;
    let (status, order) = create_anonymous_order(&h, plan, receipt).await;
    assert_eq!(status, axum::http::StatusCode::OK, "create: {order}");
    let code = order["order_code"].as_str().expect("code").to_string();
    let amount = order["amount_units"].as_i64().expect("amount");
    let mk = Arc::clone(&h.master_key);
    let outcome = h
        .db_pool
        .spawn_write(move |w| apply_payment(w, mk.as_ref(), "0xreceipt", 0, 100, amount, None, 250_000))
        .await
        .expect("apply payment");
    assert!(matches!(outcome, PaymentOutcome::Credited { .. }));

    let (s1, k1) = h.oneshot_json(reveal_request(&code, Some(receipt))).await;
    assert_eq!(s1, axum::http::StatusCode::OK, "first reveal: {k1}");
    let (s2, k2) = h.oneshot_json(reveal_request(&code, Some(receipt))).await;
    assert_eq!(
        s2,
        axum::http::StatusCode::OK,
        "retry must not lose the key: {k2}"
    );
    assert_eq!(k1["key"], k2["key"], "same plaintext while unacked");

    let (s3, ack) = h.oneshot_json(ack_request(&code, receipt)).await;
    assert_eq!(s3, axum::http::StatusCode::OK, "ack: {ack}");
    // A repeated ACK of the same receipt is a no-op success: a lost ACK
    // response must not surface as an error to the buyer.
    let (s3b, ack2) = h.oneshot_json(ack_request(&code, receipt)).await;
    assert_eq!(s3b, axum::http::StatusCode::OK, "repeated ack: {ack2}");
    let (s4, _) = h.oneshot_json(reveal_request(&code, Some(receipt))).await;
    assert_eq!(s4, axum::http::StatusCode::CONFLICT, "wiped after ack");

    // A too-short receipt is rejected, never silently treated as legacy.
    let (s5, body) = h.oneshot_json(reveal_request(&code, Some("short"))).await;
    assert_eq!(
        s5,
        axum::http::StatusCode::BAD_REQUEST,
        "short receipt: {body}"
    );
}

/// The header-less legacy reveal is gone: an order code alone is NOT a
/// delivery capability. Without the receipt the ciphertext stays intact, so
/// the buyer's own retry still gets the key.
#[tokio::test]
async fn reveal_without_receipt_cannot_consume_the_key_e2e() {
    let h = TestHarness::new().await;
    let receipt = "12345678-1234-1234-1234-123456789abc";
    let plan = seed_plan(&h, 10.0, 10.0, 15).await;
    let (status, order) = create_anonymous_order(&h, plan, receipt).await;
    assert_eq!(status, axum::http::StatusCode::OK, "create: {order}");
    let code = order["order_code"].as_str().expect("code").to_string();
    let amount = order["amount_units"].as_i64().expect("amount");
    let mk = Arc::clone(&h.master_key);
    let outcome = h
        .db_pool
        .spawn_write(move |w| apply_payment(w, mk.as_ref(), "0xnoheader", 0, 100, amount, None, 250_000))
        .await
        .expect("apply payment");
    assert!(matches!(outcome, PaymentOutcome::Credited { .. }));

    // No header: refused, and the order still has its encrypted key.
    let (status, body) = h.oneshot_json(reveal_request(&code, None)).await;
    assert_eq!(
        status,
        axum::http::StatusCode::BAD_REQUEST,
        "missing receipt: {body}"
    );
    let code_q = code.clone();
    let still_deliverable: bool = h
        .db_pool
        .spawn_read(move |r| {
            r.query_row(
                "SELECT minted_key_encrypted IS NOT NULL AND key_revealed = 0 \
                 FROM credit_orders WHERE order_code = ?1",
                [&code_q],
                |row| row.get(0),
            )
            .map_err(openproxy_db::error::map_db_error)
        })
        .await
        .expect("read order");
    assert!(still_deliverable, "refused reveal must not wipe the key");

    // The rightful receipt still delivers it.
    let (status, rev) = h.oneshot_json(reveal_request(&code, Some(receipt))).await;
    assert_eq!(status, axum::http::StatusCode::OK, "receipt reveal: {rev}");
}

/// The delivery receipt is a CREATION precondition: no anonymous order exists
/// without it, and a rejected create leaves no payable row behind.
#[tokio::test]
async fn anonymous_order_requires_receipt_at_creation_e2e() {
    let h = TestHarness::new().await;
    let plan = seed_plan(&h, 5.0, 5.0, 7).await;
    let count_orders = |h: &TestHarness| {
        let pool = Arc::clone(&h.db_pool);
        async move {
            pool.spawn_read(|r| {
                r.query_row("SELECT COUNT(*) FROM credit_orders", [], |row| {
                    row.get::<_, i64>(0)
                })
                .map_err(openproxy_db::error::map_db_error)
            })
            .await
            .expect("count orders")
        }
    };

    // No header: refused before any DB write.
    let (status, body) = h
        .oneshot_json(h.public_request(
            axum::http::Method::POST,
            "/v1/credits/order",
            json!({"tier_id": plan, "mint_new_key": true}).to_string(),
        ))
        .await;
    assert_eq!(
        status,
        axum::http::StatusCode::BAD_REQUEST,
        "missing receipt: {body}"
    );
    assert_eq!(body["error"]["code"], json!("missing_receipt"));
    assert_eq!(count_orders(&h).await, 0, "no order may be created");

    // A receipt outside 32..=128 URL-safe chars is rejected too.
    let mut req = h.public_request(
        axum::http::Method::POST,
        "/v1/credits/order",
        json!({"tier_id": plan, "mint_new_key": true}).to_string(),
    );
    req.headers_mut().insert(
        "idempotency-key",
        axum::http::HeaderValue::from_static("short"),
    );
    let (status, body) = h.oneshot_json(req).await;
    assert_eq!(
        status,
        axum::http::StatusCode::BAD_REQUEST,
        "short receipt: {body}"
    );
    assert_eq!(body["error"]["code"], json!("invalid_receipt"));
    assert_eq!(count_orders(&h).await, 0, "still no order");

    // With a receipt the order is created and its hash is persisted up front.
    let receipt = "99999999-8888-7777-6666-555555555555";
    let (status, order) = create_anonymous_order(&h, plan, receipt).await;
    assert_eq!(status, axum::http::StatusCode::OK, "create: {order}");
    let code = order["order_code"].as_str().expect("code").to_string();
    let bound: i64 = h
        .db_pool
        .spawn_read(move |r| {
            r.query_row(
                "SELECT reveal_receipt_hash IS NOT NULL FROM credit_orders \
                 WHERE order_code = ?1",
                [&code],
                |row| row.get(0),
            )
            .map_err(openproxy_db::error::map_db_error)
        })
        .await
        .expect("read bound hash");
    assert_eq!(bound, 1, "receipt hash is bound at creation, not at reveal");
}

/// A retried create with the SAME `Idempotency-Key` must resolve to the order
/// the first attempt created — same code, same amount — instead of quoting a
/// second payable amount for one payment. A different receipt still gets its
/// own order (the replay is keyed on the capability, not the client).
#[tokio::test]
async fn retried_create_with_same_receipt_reuses_the_order_e2e() {
    let h = TestHarness::new().await;
    let plan = seed_plan(&h, 9.0, 9.0, 30).await;
    let receipt = "aaaabbbb-cccc-dddd-eeee-ffff00001111";

    let (status, first) = create_anonymous_order(&h, plan, receipt).await;
    assert_eq!(status, axum::http::StatusCode::OK, "first create: {first}");
    let (status, retried) = create_anonymous_order(&h, plan, receipt).await;
    assert_eq!(status, axum::http::StatusCode::OK, "retry: {retried}");
    assert_eq!(
        first["order_code"], retried["order_code"],
        "the retry must return the original order, not a second payable amount"
    );
    assert_eq!(first["amount_units"], retried["amount_units"]);
    assert_eq!(first["expires_at"], retried["expires_at"]);

    let count: i64 = h
        .db_pool
        .spawn_read(|r| {
            r.query_row("SELECT COUNT(*) FROM credit_orders", [], |row| {
                row.get::<_, i64>(0)
            })
            .map_err(openproxy_db::error::map_db_error)
        })
        .await
        .expect("count orders");
    assert_eq!(count, 1, "one receipt must not open two orders");

    // Another browser's receipt is a different capability: own order, own row.
    let other_receipt = "bbbbcccc-dddd-eeee-ffff-000011112222";
    let (status, other) = create_anonymous_order(&h, plan, other_receipt).await;
    assert_eq!(status, axum::http::StatusCode::OK, "second buyer: {other}");
    assert_ne!(other["order_code"], first["order_code"]);

    // Once paid, api_key_id changes to the minted key. A retry must still
    // identify the anonymous purchase, not allocate or classify a renewal.
    let amount = first["amount_units"].as_i64().unwrap();
    let mk = Arc::clone(&h.master_key);
    h.db_pool
        .spawn_write(move |w| apply_payment(w, &mk, "replayed-mint", 0, 100, amount, None, 250_000))
        .await
        .unwrap();
    let (status, paid_retry) = create_anonymous_order(&h, plan, receipt).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{paid_retry}");
    assert_eq!(paid_retry["order_code"], first["order_code"]);
    assert_eq!(paid_retry["status"], "minted");
    assert!(paid_retry["api_key_id"].is_null(), "mint is not renewal");
    let code = paid_retry["order_code"].as_str().unwrap();
    let (status, key) = h.oneshot_json(reveal_request(code, Some(receipt))).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{key}");

    // Same capability with a different purchase is an explicit rejection.
    let different = seed_plan(&h, 10.0, 10.0, 7).await;
    let (status, _) = create_anonymous_order(&h, different, receipt).await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
}

/// A leaked order code is not a delivery capability: only the receipt sent at
/// creation can claim the minted key, even before anyone revealed it.
#[tokio::test]
async fn order_code_alone_cannot_claim_a_receipt_bound_key_e2e() {
    let h = TestHarness::new().await;
    let creator = "aaaa0000-bbbb-1111-cccc-222222222222";
    let attacker = "dddd3333-eeee-4444-ffff-555555555555";
    let plan = seed_plan(&h, 10.0, 10.0, 15).await;
    let (status, order) = create_anonymous_order(&h, plan, creator).await;
    assert_eq!(status, axum::http::StatusCode::OK, "create: {order}");
    let code = order["order_code"].as_str().expect("code").to_string();
    let amount = order["amount_units"].as_i64().expect("amount");
    let mk = Arc::clone(&h.master_key);
    let outcome = h
        .db_pool
        .spawn_write(move |w| apply_payment(w, mk.as_ref(), "0xleaked", 0, 100, amount, None, 250_000))
        .await
        .expect("apply payment");
    assert!(matches!(outcome, PaymentOutcome::Credited { .. }));

    // Code + a different receipt: denied, and the ciphertext survives.
    let (status, body) = h.oneshot_json(reveal_request(&code, Some(attacker))).await;
    assert_eq!(
        status,
        axum::http::StatusCode::CONFLICT,
        "foreign receipt must not claim the key: {body}"
    );

    // The creator's receipt still delivers it.
    let (status, rev) = h.oneshot_json(reveal_request(&code, Some(creator))).await;
    assert_eq!(status, axum::http::StatusCode::OK, "creator reveal: {rev}");
    assert!(
        rev["key"].as_str().is_some_and(|k| k.starts_with("op_")),
        "key delivered: {rev}"
    );
}
