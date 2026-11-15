use super::*;

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
        .spawn_write(move |w| {
            apply_payment(w, mk.as_ref(), "0xleaked", 0, 100, amount, None, 250_000)
        })
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

/// Root-fix regression (audit A1-01/A4-01): the checkout surface is rate
/// limited per client IP. Before the fix, ~10 000 free anonymous order
/// creations exhausted the 10 000 jitter slots of a base price for the whole
/// window + grace and every real buyer of that price got 503 (reproduced at
/// 49 orders/s, zero 429s). With admission control the flood is cut off at
/// the configured budget while the status surface keeps answering.
#[tokio::test]
async fn checkout_admission_control_blocks_the_order_flood() {
    use openproxy_db::secrets::MasterKey;
    use openproxy_server::{router::build_router, state::AppState};

    // Strict admission config: 5 order creations / 10 min, 20 polls/min.
    let temp = openproxy_db::testing::TempDir::new("admission-e2e").expect("temp");
    let db_pool = Arc::new(openproxy_db::DbPool::open(&temp.path().join("a.db")).expect("db"));
    {
        let mut w = db_pool.writer();
        openproxy_db::migrations::run(&mut w).expect("migrate");
    }
    let master_key = Arc::new(MasterKey::generate().expect("mk"));
    let mut cfg = openproxy_core::AppConfig::default();
    cfg.credits = Some(openproxy_types::config::CreditsConfig {
        rpc_url: "http://127.0.0.1:1".into(),
        usdc_address: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913".into(),
        collector_address: "0x12c7ab78f21ba0253a21d34a0151d44ddb824950".into(),
        payment_window_minutes: 15,
        scan_interval_secs: 5,
        ttl_days: vec![7, 15, 30],
        chain_id: 8453,
        max_block_chunk: 500,
        payment_tolerance_units: 250_000,
        checkout_order_creations_per_ip: 5,
        checkout_order_window_secs: 600,
        checkout_poll_per_ip_per_min: 20,
    });
    let adapters = Arc::new(parking_lot::RwLock::new(Arc::new(vec![])));
    let state = AppState::for_test(cfg, Arc::clone(&db_pool), Arc::clone(&master_key), adapters);
    state.rebuild_adapters().await.expect("adapters");
    let router = build_router(state);

    let plan = db_pool
        .spawn_write(move |w| {
            openproxy_core::credits::tiers::create(
                w,
                "flood-plan",
                &openproxy_core::credits::TierConfig::default(),
                &openproxy_core::credits::PlanCommerce {
                    price_usd: Some(5.0),
                    credit_usd: Some(5.0),
                    ttl_days: 15,
                    stock: None,
                },
            )
            .map(|t| t.id)
        })
        .await
        .expect("plan");

    let oneshot = |mut req: axum::http::Request<axum::body::Body>| async {
        use std::net::SocketAddr;
        use tower::ServiceExt;
        if req
            .extensions()
            .get::<axum::extract::connect_info::ConnectInfo<SocketAddr>>()
            .is_none()
        {
            req.extensions_mut()
                .insert(axum::extract::connect_info::ConnectInfo(SocketAddr::from(
                    ([127, 0, 0, 1], 4444),
                )));
        }
        let resp = router.clone().oneshot(req).await.expect("oneshot");
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .expect("body");
        (status, body)
    };

    // The flood: 30 order creations from one IP. Only the first 5 pass.
    let mut ok = 0usize;
    let mut throttled = 0usize;
    let mut first_code = String::new();
    for i in 0..30 {
        let receipt = format!("admiss-0000000{i}-aaaa-bbbb-cccccccccccc");
        let req = axum::http::Request::builder()
            .method(axum::http::Method::POST)
            .uri("/v1/credits/order")
            .header("content-type", "application/json")
            .header("idempotency-key", receipt)
            .body(axum::body::Body::from(
                serde_json::json!({"tier_id": plan, "mint_new_key": true}).to_string(),
            ))
            .unwrap();
        let (status, body) = oneshot(req).await;
        match status {
            axum::http::StatusCode::OK => {
                ok += 1;
                if first_code.is_empty() {
                    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    first_code = v["order_code"].as_str().unwrap().to_string();
                }
            }
            axum::http::StatusCode::TOO_MANY_REQUESTS => throttled += 1,
            s => panic!("unexpected status {s}: {}", String::from_utf8_lossy(&body)),
        }
    }
    assert_eq!(ok, 5, "exactly the configured budget passes");
    assert_eq!(throttled, 25, "the rest of the flood is throttled");

    // The read surface keeps serving the buyer inside its own budget.
    let req = axum::http::Request::builder()
        .method(axum::http::Method::GET)
        .uri(format!("/v1/credits/order/{first_code}"))
        .body(axum::body::Body::empty())
        .unwrap();
    let (status, _) = oneshot(req).await;
    assert_eq!(status, axum::http::StatusCode::OK, "status poll unaffected");

    // Poll budget applies too: past 20/min the read surface throttles.
    let mut polls_429 = 0usize;
    for _ in 0..40 {
        let req = axum::http::Request::builder()
            .method(axum::http::Method::GET)
            .uri(format!("/v1/credits/order/{first_code}"))
            .body(axum::body::Body::empty())
            .unwrap();
        let (status, _) = oneshot(req).await;
        if status == axum::http::StatusCode::TOO_MANY_REQUESTS {
            polls_429 += 1;
        }
    }
    assert!(polls_429 > 0, "poll budget eventually throttles");
}
