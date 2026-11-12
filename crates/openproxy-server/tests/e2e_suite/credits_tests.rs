//! E2E of the on-chain credit gateway, WITHOUT real money.
//!
//! The Base JSON-RPC surface (`eth_getBlockByNumber`/`eth_getLogs`) is mocked
//! by a local axum server, so the REAL watcher code path (scan -> decode ->
//! matcher -> ledger) runs against a controlled chain view. What must hold:
//! exactly one credit per payment, exact-amount matching, one-time reveal,
//! balance equals the sum of paid base amounts, and the history always points
//! back into the chain explorer.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::harness::TestHarness;
use axum::{Router, response::IntoResponse, routing::post};
use openproxy_core::credits::{PaymentOutcome, apply_payment};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::sync::Arc;

/// Shared state of the mock Base node.
#[derive(Default)]
struct MockChain {
    safe_block: Mutex<i64>,
    /// Logs served by the next `eth_getLogs` call.
    logs: Mutex<Vec<Value>>,
}

async fn rpc_entry(
    axum::extract::State(state): axum::extract::State<Arc<MockChain>>,
    body: axum::body::Bytes,
) -> axum::response::Response {
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let method = parsed.get("method").and_then(Value::as_str).unwrap_or("");
    let payload = match method {
        "eth_getBlockByNumber" => json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {"number": format!("0x{:x}", *state.safe_block.lock())}
        }),
        "eth_getLogs" => json!({
            "jsonrpc": "2.0", "id": 1,
            "result": state.logs.lock().clone()
        }),
        _ => json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32601, "message": "unknown"}}),
    };
    axum::Json(payload).into_response()
}

fn spawn_mock_rpc(state: Arc<MockChain>) -> std::net::SocketAddr {
    let app = Router::new().route("/", post(rpc_entry)).with_state(state);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock rpc");
    listener
        .set_nonblocking(true)
        .expect("mock rpc nonblocking");
    let addr = listener.local_addr().expect("mock rpc addr");
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("mock rpc runtime");
        rt.block_on(async move {
            axum::serve(tokio::net::TcpListener::from_std(listener).expect("tcp"), app)
                .await
                .expect("serve mock rpc");
        });
    });
    addr
}

/// A USDC Transfer log into the collector with an exact amount (micro-units).
fn transfer_log(amount_units: i64, tx: &str, log_index: i64) -> Value {
    let collector = "12c7ab78f21ba0253a21d34a0151d44ddb824950";
    json!({
        "address": "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
        "topics": [
            "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef",
            "0x0000000000000000000000001111111111111111111111111111111111111111",
            format!("0x000000000000000000000000{collector}")
        ],
        "data": format!("0x{:0>64}", format!("{amount_units:x}")),
        "logIndex": format!("0x{log_index:x}"),
        "blockNumber": "0x64",
        "transactionHash": tx
    })
}

/// Seed a sellable plan (price -> credit, TTL days) and return its id.
async fn seed_plan(h: &TestHarness, price: f64, credit: f64, ttl: i64) -> i64 {
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

    // 1. Anonymous checkout: mint a key for the $10 / 15-day plan (1:1 credit).
    let plan = seed_plan(&h, 10.0, 10.0, 15).await;
    let (status, order) = h
        .oneshot_json(h.public_request(
            axum::http::Method::POST,
            "/v1/credits/order",
            json!({"tier_id": plan, "mint_new_key": true}).to_string(),
        ))
        .await;
    assert_eq!(status, axum::http::StatusCode::OK, "create order: {order}");
    let code = order["order_code"].as_str().expect("order_code").to_string();
    let amount_units = order["amount_units"].as_i64().expect("amount_units");
    let base_units = 10_000_000_i64;
    assert!(
        amount_units >= base_units && amount_units < base_units + 1_000_000,
        "jittered amount {amount_units} inside [base, base+1e6)"
    );
    assert!(order["qr_uri"].as_str().unwrap().starts_with("ethereum:0x833589"));
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
        .spawn_write(move |w| {
            apply_payment(w, mk.as_ref(), "0xstray", 0, 100, stray, None)
        })
        .await
        .expect("apply stray");
    assert!(matches!(outcome, PaymentOutcome::Unmatched { .. }));

    // 4. The exact payment: credited once.
    let mk = Arc::clone(&h.master_key);
    let outcome = h
        .db_pool
        .spawn_write(move |w| {
            apply_payment(w, mk.as_ref(), "0xpay1", 0, 100, amount_units, None)
        })
        .await
        .expect("apply pay1");
    let PaymentOutcome::Credited { api_key_id, credited_units, .. } = outcome else {
        panic!("expected Credited, got {outcome:?}");
    };
    assert_eq!(credited_units, base_units, "credit is the base, not the jitter");

    // 5. Same (tx, log) delivered twice by a rescan: no double credit.
    let mk = Arc::clone(&h.master_key);
    let outcome = h
        .db_pool
        .spawn_write(move |w| {
            apply_payment(w, mk.as_ref(), "0xpay1", 0, 100, amount_units, None)
        })
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

    // 7. Reveal exactly once.
    let (status, rev) = h
        .oneshot_json(h.public_request(
            axum::http::Method::POST,
            &format!("/v1/credits/order/{code}/reveal"),
            "",
        ))
        .await;
    assert_eq!(status, axum::http::StatusCode::OK, "reveal: {rev}");
    let key = rev["key"].as_str().expect("revealed key").to_string();
    assert!(key.starts_with("op_"), "key format: {key}");
    let (status, _) = h
        .oneshot_json(h.public_request(
            axum::http::Method::POST,
            &format!("/v1/credits/order/{code}/reveal"),
            "",
        ))
        .await;
    assert_eq!(status, axum::http::StatusCode::CONFLICT, "second reveal refused");

    // 8. Balance of the minted key equals exactly the credited base amount.
    let kid = api_key_id;
    let (status, bal) = h
        .oneshot_json(h.admin_request(
            axum::http::Method::GET,
            &format!("/admin/api/credits/balance?key_id={kid}"),
            "",
        ))
        .await;
    assert_eq!(status, axum::http::StatusCode::OK, "balance: {bal}");
    assert_eq!(bal["balance_units"], json!(base_units));
    assert_eq!(bal["balance_usd"], json!("10.000000"));

    // 9. History points back into the chain.
    let (status, txs) = h
        .oneshot_json(h.admin_request(
            axum::http::Method::GET,
            &format!("/admin/api/credits/transactions?key_id={kid}"),
            "",
        ))
        .await;
    assert_eq!(status, axum::http::StatusCode::OK, "history: {txs}");
    assert_eq!(txs.as_array().map(Vec::len), Some(1));
    assert_eq!(txs[0]["kind"], json!("topup"));
    assert_eq!(txs[0]["basescan_url"], json!("https://basescan.org/tx/0xpay1"));
    assert_eq!(txs[0]["order_code"], json!(code));
}

/// The REAL watcher loop against the mock chain: scan -> decode -> matcher ->
/// ledger -> cursor. Exactly one credit, cursor advances, replay credits
/// nothing, and the ledger self-audit stays consistent.
#[tokio::test]
async fn watcher_scan_credits_exactly_once_e2e() {
    let chain = Arc::new(MockChain::default());
    let rpc_addr = spawn_mock_rpc(Arc::clone(&chain));
    let mut h = TestHarness::new().await;

    // Point the app config's rpc_url at the mock by rebuilding the credits
    // config the watcher service will use.
    let mut cfg = h
        .app_state
        .config()
        .credits
        .clone()
        .expect("credits config in harness");
    cfg.rpc_url = format!("http://{rpc_addr}");

    // A live order for the admin session key ($5 / 7-day plan, 1:1 credit).
    let plan = seed_plan(&h, 5.0, 5.0, 7).await;
    let (status, order) = h
        .oneshot_json(h.admin_request(
            axum::http::Method::POST,
            &format!("/admin/api/credits/topup?key_id={}", h.admin_key_id.0),
            json!({"tier_id": plan}).to_string(),
        ))
        .await;
    assert_eq!(status, axum::http::StatusCode::OK, "topup order: {order}");
    let amount_units = order["amount_units"].as_i64().expect("amount_units");
    let base_units = 5_000_000_i64;
    let kid = h.admin_key_id.0;

    let service = openproxy_server::background::credits_watcher::CreditsWatcherService {
        db_pool: Arc::clone(&h.db_pool),
        config: cfg.clone(),
        master_key: Arc::clone(&h.master_key),
        upstream_client: Arc::clone(h.app_state.upstream_client()),
    };

    // Tick 1 at head 50: initializes the cursor, replays nothing.
    *chain.safe_block.lock() = 50;
    let init = service.tick().await.expect("init tick");
    assert_eq!(init.logs_seen, 0, "init: {init:?}");
    assert_eq!(init.from_block, 50, "init: {init:?}");

    // Tick 2 at head 100: discovers both logs, credits exactly one.
    *chain.safe_block.lock() = 100;
    *chain.logs.lock() = vec![
        transfer_log(amount_units, "0xwatch1", 0),
        transfer_log(77_000_000 + 424_242, "0xstray2", 1),
    ];
    let report = service.tick().await.expect("scan tick");
    assert_eq!(report.logs_seen, 2, "report: {report:?}");
    assert_eq!(report.credited, 1, "report: {report:?}");
    assert_eq!(report.unmatched, 1, "report: {report:?}");

    // Balance credited exactly once, base amount only.
    let (credit_based, units, _lots) = h
        .db_pool
        .spawn_read(move |w| {
            Ok((
                openproxy_db::credits_ledger::is_credit_based(w, kid)?,
                openproxy_db::credits_ledger::balance_units(w, kid)?,
                openproxy_db::credits_ledger::active_lots(w, kid)?,
            ))
        })
        .await
        .expect("read balance");
    assert!(credit_based);
    assert_eq!(units, base_units, "no leak: balance == base, stray ignored");

    // Cursor advanced to the safe block: a second tick sees nothing new.
    *chain.logs.lock() = vec![transfer_log(amount_units, "0xreplay", 2)];
    let report2 = service.tick().await.expect("second tick");
    assert_eq!(report2.credited, 0, "no double credit: {report2:?}");
    assert_eq!(report2.logs_seen, 0, "cursor excludes old blocks");

    // Ledger self-audit: consistent after every path.
    let audit = h
        .db_pool
        .spawn_write(move |w| openproxy_db::credits_ledger::reconcile(w))
        .await
        .expect("reconcile");
    assert!(audit.is_consistent(), "audit: {audit:?}");
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
    let mut amounts = Vec::with_capacity(N);
    for i in 0..N {
        let (status, order) = h
            .oneshot_json(h.admin_request(
                axum::http::Method::POST,
                &format!("/admin/api/credits/topup?key_id={}", h.admin_key_id.0),
                json!({"tier_id": plan}).to_string(),
            ))
            .await;
        assert_eq!(status, axum::http::StatusCode::OK, "order {i}: {order}");
        let a = order["amount_units"].as_i64().expect("amount_units");
        assert!(a >= BASE && a < BASE + 1_000_000);
        amounts.push(a);
    }
    // The thin funnel guarantees distinct live amounts: no two orders can
    // share the exact payment amount.
    let mut sorted = amounts.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), N, "jitter must make all live amounts distinct");

    // Apply every payment concurrently, each delivered twice (watcher
    // rescan), plus two strays. All through the real spawn_write path.
    let kid = h.admin_key_id.0;
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
                .spawn_write(move |w| {
                    apply_payment(w, mk1.as_ref(), &tx1, 0, 100, amount, None)
                })
                .await
                .expect("first delivery");
            let second = pool
                .spawn_write(move |w| {
                    apply_payment(w, mk2.as_ref(), &tx2, 0, 100, amount, None)
                })
                .await
                .expect("replay delivery");
            (first, second)
        }));
    }
    let mut credited = 0;
    let mut replayed = 0;
    for jh in handles {
        let (first, second) = jh.await.expect("task");
        assert!(matches!(first, PaymentOutcome::Credited { .. }), "first: {first:?}");
        assert!(matches!(second, PaymentOutcome::AlreadySeen { .. }), "replay: {second:?}");
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
    let (status, txs) = h
        .oneshot_json(h.admin_request(
            axum::http::Method::GET,
            &format!("/admin/api/credits/transactions?key_id={kid}&limit=100"),
            "",
        ))
        .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let arr = txs.as_array().expect("array");
    assert_eq!(arr.len(), N, "exactly one topup row per order");
    assert!(
        arr.iter().all(|t| t["basescan_url"].as_str().unwrap().contains("basescan.org/tx/0xstress")),
        "every row points into the chain explorer"
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
    assert!(html.to_lowercase().contains("no refunds"), "no-refunds notice");
}

/// With the runtime master switch off, every buying surface refuses: the
/// public checkout returns 403 gateway_disabled and the packages route is
/// gone entirely. This is the admin "close the gateway" toggle guarantee.
#[tokio::test]
async fn gateway_toggle_closes_all_buying_surfaces() {
    let h = TestHarness::new().await;
    // Flip the switch off directly in app_config (what the admin POST does).
    h.db_pool
        .spawn_write(|w| {
            openproxy_db::app_config::save_credits_gateway_enabled_to_db(w, false, 0)
        })
        .await
        .expect("save flag");

    let (status, _, body) = h
        .oneshot(
            h.public_request(
                axum::http::Method::POST,
                "/v1/credits/order",
                r#"{"tier_id": 1, "mint_new_key": true}"#,
            ),
        )
        .await;
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN, "checkout refused");
    assert!(String::from_utf8_lossy(&body).contains("gateway_disabled"));

    let (status, _, _) = h
        .oneshot(h.public_request(axum::http::Method::GET, "/v1/credits/packages", ""))
        .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND, "packages surface gone");

    // Admin API still answers so the toggle can be turned back on.
    let (status, _, _) = h
        .oneshot(h.admin_request(axum::http::Method::GET, "/admin/api/credits/gateway", ""))
        .await;
    assert_eq!(status, axum::http::StatusCode::OK);
}
