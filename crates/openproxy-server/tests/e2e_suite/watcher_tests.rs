//! Watcher E2E against a mocked Base JSON-RPC node: scan -> decode -> matcher
//! -> ledger -> cursor.
//!
//! Kept separate from `credits_tests.rs` (checkout/reveal surface): the mock
//! node plus its strict log fixtures is a coherent unit, and both files stay
//! well under the 800-line budget.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::credits_tests::seed_plan;
use crate::harness::TestHarness;
use axum::{Router, response::IntoResponse, routing::post};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::sync::Arc;

/// Shared state of the mock Base node.
struct MockChain {
    /// `None` = the node does not answer the `safe` tag (returns null).
    safe_block: Mutex<Option<i64>>,
    /// `None` = the node does not answer the `finalized` tag.
    finalized_block: Mutex<Option<i64>>,
    chain_id: Mutex<i64>,
    /// Logs held by the mock; `eth_getLogs` filters them by range/address.
    logs: Mutex<Vec<Value>>,
    /// Simulates a node lagging behind the chain: hex-tag block lookups and
    /// `eth_getLogs` answers above this block come back empty (`null` / no
    /// logs), while the `safe`/`finalized` tags still report the true head —
    // exactly the split-brain an RPC load balancer can produce (audit round
    /// 2, V-03/A4-09). `None` = healthy node.
    hide_blocks_above: Mutex<Option<i64>>,
    /// Seconds between blocks of the mock chain clock.
    block_secs: i64,
    /// Frozen genesis timestamp. `None` anchors the safe head at the wall clock,
    /// so a transfer at `head - k` is `k * block_secs` seconds old. Tests that
    /// replay a days-old order pin it instead.
    genesis_ts: Mutex<Option<i64>>,
}

impl Default for MockChain {
    fn default() -> Self {
        Self {
            safe_block: Mutex::new(Some(0)),
            finalized_block: Mutex::new(Some(0)),
            chain_id: Mutex::new(8453),
            logs: Mutex::new(Vec::new()),
            hide_blocks_above: Mutex::new(None),
            block_secs: 2,
            genesis_ts: Mutex::new(None),
        }
    }
}

impl MockChain {
    /// Deterministic linear block clock: `genesis + n * block_secs`.
    ///
    /// The old mock stamped EVERY block with `now`, which hid the whole class of
    /// "payment made before the process died" bugs: the watcher locates a block
    /// by timestamp, so block times must actually advance with the block number
    /// and a payment at `head - k` must be `k * block_secs` seconds old.
    fn block_ts(&self, block: i64) -> i64 {
        let secs = self.block_secs.max(1);
        if let Some(genesis) = *self.genesis_ts.lock() {
            return genesis + block * secs;
        }
        let head = (*self.safe_block.lock())
            .or(*self.finalized_block.lock())
            .unwrap_or(0);
        unix_now() - (head.max(block) - block) * secs
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn hex_qty(v: i64) -> String {
    format!("0x{v:x}")
}

async fn rpc_entry(
    axum::extract::State(state): axum::extract::State<Arc<MockChain>>,
    body: axum::body::Bytes,
) -> axum::response::Response {
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let method = parsed.get("method").and_then(Value::as_str).unwrap_or("");
    let params = parsed.get("params").cloned().unwrap_or(Value::Null);
    let payload = match method {
        "eth_chainId" => json!({
            "jsonrpc": "2.0", "id": 1, "result": hex_qty(*state.chain_id.lock())
        }),
        "eth_getBlockByNumber" => {
            let tag = params
                .as_array()
                .and_then(|p| p.first())
                .and_then(Value::as_str)
                .unwrap_or("safe");
            let number = match tag {
                "safe" => *state.safe_block.lock(),
                "finalized" => *state.finalized_block.lock(),
                hex => {
                    let n = i64::from_str_radix(hex.trim_start_matches("0x"), 16).unwrap_or(0);
                    // A lagging node cannot produce blocks it has not seen.
                    let hidden = *state.hide_blocks_above.lock();
                    if hidden.is_some_and(|limit| n > limit) {
                        None
                    } else {
                        Some(n)
                    }
                }
            };
            // Block times follow the linear chain clock, so a transfer in a
            // days-old block carries a days-old timestamp (the matcher's whole
            // "payment made before the outage" rule depends on it).
            let result = number.map_or(
                Value::Null,
                |n| json!({"number": hex_qty(n), "timestamp": hex_qty(state.block_ts(n))}),
            );
            json!({"jsonrpc": "2.0", "id": 1, "result": result})
        }
        "eth_getLogs" => {
            let filter = params
                .as_array()
                .and_then(|p| p.first())
                .cloned()
                .unwrap_or(Value::Null);
            let parse = |k: &str, default: i64| {
                filter
                    .get(k)
                    .and_then(Value::as_str)
                    .and_then(|h| i64::from_str_radix(h.trim_start_matches("0x"), 16).ok())
                    .unwrap_or(default)
            };
            let from = parse("fromBlock", 0);
            let to = parse("toBlock", i64::MAX);
            // A lagging node also misses the logs of the blocks it has not
            // seen, even when the filter range covers them.
            let visible_up_to = *state.hide_blocks_above.lock();
            let address = filter
                .get("address")
                .and_then(Value::as_str)
                .map(str::to_lowercase);
            let logs: Vec<Value> = state
                .logs
                .lock()
                .iter()
                .filter(|l| {
                    let block = l
                        .get("blockNumber")
                        .and_then(Value::as_str)
                        .and_then(|h| i64::from_str_radix(h.trim_start_matches("0x"), 16).ok())
                        .unwrap_or(-1);
                    let addr_ok = address.as_ref().is_none_or(|a| {
                        l.get("address")
                            .and_then(Value::as_str)
                            .is_some_and(|x| x.eq_ignore_ascii_case(a))
                    });
                    let visible = visible_up_to.is_none_or(|limit| block <= limit);
                    block >= from && block <= to && addr_ok && visible
                })
                .cloned()
                .collect();
            json!({"jsonrpc": "2.0", "id": 1, "result": logs})
        }
        "eth_getTransactionReceipt" => {
            // Receipt exists (status 0x1) while the mock chain still carries
            // a non-removed log for the tx; a dropped/reorged-out tx returns
            // null — exactly what the reorg audit probes.
            let tx = params
                .as_array()
                .and_then(|p| p.first())
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let known = state.logs.lock().iter().any(|l| {
                l.get("transactionHash").and_then(Value::as_str) == Some(tx.as_str())
                    && l.get("removed").and_then(Value::as_bool) != Some(true)
            });
            let result = if known {
                json!({"status": "0x1"})
            } else {
                Value::Null
            };
            json!({"jsonrpc": "2.0", "id": 1, "result": result})
        }
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
            axum::serve(
                tokio::net::TcpListener::from_std(listener).expect("tcp"),
                app,
            )
            .await
            .expect("serve mock rpc");
        });
    });
    addr
}

/// A USDC Transfer log into the collector with an exact amount (micro-units).
fn transfer_log(amount_units: i64, tx: &str, log_index: i64) -> Value {
    transfer_log_at(amount_units, tx, log_index, 100)
}

/// Same, at an explicit block (the watcher validates the requested range and
/// the cursor is block-based).
fn transfer_log_at(amount_units: i64, tx: &str, log_index: i64, block: i64) -> Value {
    let collector = "12c7ab78f21ba0253a21d34a0151d44ddb824950";
    // Strict decoding requires a real 32-byte hash: pad the (hex) seed to 64
    // chars so tests exercise the production validation, not a bypass.
    let seed = tx.strip_prefix("0x").unwrap_or(tx);
    let padded = format!("{seed:0>64}").to_lowercase();
    json!({
        "address": "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
        "topics": [
            "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef",
            "0x0000000000000000000000001111111111111111111111111111111111111111",
            format!("0x000000000000000000000000{collector}")
        ],
        "data": format!("0x{:0>64}", format!("{amount_units:x}")),
        "logIndex": format!("0x{log_index:x}"),
        "blockNumber": format!("0x{block:x}"),
        "transactionHash": format!("0x{padded}")
    })
}

/// Move an order — and its payment window — `secs` into the past, forcing its
/// status. Models the checkout that happened BEFORE the outage the watcher is
/// replaying: the mock chain clock places the buyer's block at that age.
async fn backdate_order(h: &TestHarness, amount_units: i64, secs: i64, status: &str) {
    let status = status.to_string();
    h.db_pool
        .spawn_write(move |w| {
            w.execute(
                "UPDATE credit_orders \
                 SET created_at = datetime(created_at, ?2), \
                     expires_at = datetime(expires_at, ?2), \
                     status = ?3 \
                 WHERE amount_units = ?1",
                rusqlite::params![amount_units, format!("-{secs} seconds"), status],
            )
            .map_err(openproxy_db::error::map_db_error)?;
            Ok(())
        })
        .await
        .expect("backdate order");
}

/// The REAL watcher loop against the mock chain: scan -> decode -> matcher ->
/// ledger -> cursor. Exactly one credit, cursor advances, replay credits
/// nothing, and the ledger self-audit stays consistent.
#[tokio::test]
async fn watcher_scan_credits_exactly_once_e2e() {
    let chain = Arc::new(MockChain::default());
    let rpc_addr = spawn_mock_rpc(Arc::clone(&chain));
    let h = TestHarness::new().await;

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
    let kid = h.admin_key_id.0;
    let (status, order) = h
        .oneshot_json(h.admin_request(
            axum::http::Method::POST,
            "/v1/credits/order",
            json!({"tier_id": plan, "key_id": kid}).to_string(),
        ))
        .await;
    assert_eq!(status, axum::http::StatusCode::OK, "plan order: {order}");
    let amount_units = order["amount_units"].as_i64().expect("amount_units");
    let base_units = 5_000_000_i64;

    let service = openproxy_server::background::credits_watcher::CreditsWatcherService {
        db_pool: Arc::clone(&h.db_pool),
        config: cfg.clone(),
        master_key: Arc::clone(&h.master_key),
        upstream_client: Arc::clone(h.app_state.upstream_client()),
    };

    // Tick 1 at head 50: first boot scans the BOUNDED lookback (so an order
    // opened moments before the process started is not skipped) and sees no
    // logs because the mock holds none yet.
    *chain.safe_block.lock() = Some(50);
    let init = service.tick().await.expect("init tick");
    assert_eq!(init.logs_seen, 0, "init: {init:?}");
    assert_eq!(
        init.from_block, 0,
        "lookback is clamped to genesis: {init:?}"
    );
    assert_eq!(init.to_block, 50, "init: {init:?}");
    assert_eq!(init.head_tag, "safe", "init: {init:?}");

    // Tick 2 at head 100: discovers both creditable logs, credits exactly
    // one. A 0-value transfer and a > i64 uint256 transfer are VALID ERC-20
    // logs a dust attacker can emit every block: they must be ignored
    // terminally, not abort the tick and wedge the cursor.
    *chain.safe_block.lock() = Some(100);
    let mut big = transfer_log(1, "0x0e4a11", 3);
    big["data"] = json!(format!("0x{}", "f".repeat(64)));
    *chain.logs.lock() = vec![
        transfer_log(amount_units, "0x0a7c41", 0),
        transfer_log(77_000_000 + 424_242, "0x57a1a2", 1),
        transfer_log(0, "0x0d0570", 2),
        big,
    ];
    let report = service.tick().await.expect("scan tick");
    assert_eq!(
        report.logs_seen, 2,
        "ignored logs are not creditable: {report:?}"
    );
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
    *chain.logs.lock() = vec![transfer_log(amount_units, "0x7e91a4", 2)];
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

/// First boot must not skip an order that was already open when the process
/// started: the origin is the block carrying that order's `created_at`, located
/// by the bounded timestamp search (not "now", not a fixed lookback).
#[tokio::test]
async fn watcher_first_boot_credits_recent_open_order_e2e() {
    let chain = Arc::new(MockChain::default());
    let rpc_addr = spawn_mock_rpc(Arc::clone(&chain));
    let h = TestHarness::new().await;

    let mut cfg = h
        .app_state
        .config()
        .credits
        .clone()
        .expect("credits config in harness");
    cfg.rpc_url = format!("http://{rpc_addr}");

    let plan = seed_plan(&h, 5.0, 5.0, 7).await;
    let kid = h.admin_key_id.0;
    let (status, order) = h
        .oneshot_json(h.admin_request(
            axum::http::Method::POST,
            "/v1/credits/order",
            json!({"tier_id": plan, "key_id": kid}).to_string(),
        ))
        .await;
    assert_eq!(status, axum::http::StatusCode::OK, "order: {order}");
    let amount_units = order["amount_units"].as_i64().expect("amount_units");

    // The order (and its window) was created 5 minutes before the process
    // started: the mock chain clock puts block 4_990 twenty seconds ago, i.e.
    // inside this order's window but after its creation.
    backdate_order(&h, amount_units, 300, "pending").await;

    // The payment landed BEFORE the watcher's first tick, inside the lookback
    // but before the safe head (a delayed first start).
    *chain.safe_block.lock() = Some(5000);
    *chain.logs.lock() = vec![transfer_log_at(amount_units, "0xb00701", 0, 4990)];

    let service = openproxy_server::background::credits_watcher::CreditsWatcherService {
        db_pool: Arc::clone(&h.db_pool),
        config: cfg,
        master_key: Arc::clone(&h.master_key),
        upstream_client: Arc::clone(h.app_state.upstream_client()),
    };
    let report = service.tick().await.expect("bootstrap tick");
    assert_eq!(report.credited, 1, "bootstrap must credit: {report:?}");
    // The origin is the block carrying the oldest unsettled order's
    // `created_at` (block 4_790 at 2 s/block), not the configured window and
    // not the finality head.
    assert!(
        report.from_block <= 4990 && report.from_block > 4000,
        "order-anchored bootstrap: {report:?}"
    );
    assert_eq!(report.to_block, 5000, "{report:?}");
    assert_eq!(report.backlog_blocks, 0, "{report:?}");

    let (units, last) = h
        .db_pool
        .spawn_read(move |r| {
            Ok((
                openproxy_db::credits_ledger::balance_units(r, kid)?,
                openproxy_db::credits_ledger::last_scanned_block(r)?,
            ))
        })
        .await
        .expect("read balance/cursor");
    assert_eq!(units, 5_000_000, "base amount credited exactly once");
    assert_eq!(last, 5000, "cursor advanced to the scanned head");
}

/// A long outage (hours of blocks) must catch up inside ONE tick: the tick
/// scans consecutive bounded chunks instead of stopping after the first 500
/// blocks and waiting for the next interval.
#[tokio::test]
async fn watcher_catches_up_after_long_downtime_in_one_tick() {
    let chain = Arc::new(MockChain::default());
    let rpc_addr = spawn_mock_rpc(Arc::clone(&chain));
    let h = TestHarness::new().await;

    let mut cfg = h
        .app_state
        .config()
        .credits
        .clone()
        .expect("credits config in harness");
    cfg.rpc_url = format!("http://{rpc_addr}");

    let plan = seed_plan(&h, 5.0, 5.0, 7).await;
    let kid = h.admin_key_id.0;
    let (status, order) = h
        .oneshot_json(h.admin_request(
            axum::http::method::Method::POST,
            "/v1/credits/order",
            json!({"tier_id": plan, "key_id": kid}).to_string(),
        ))
        .await;
    assert_eq!(status, axum::http::StatusCode::OK, "order: {order}");
    let amount_units = order["amount_units"].as_i64().expect("amount_units");

    let service = openproxy_server::background::credits_watcher::CreditsWatcherService {
        db_pool: Arc::clone(&h.db_pool),
        config: cfg,
        master_key: Arc::clone(&h.master_key),
        upstream_client: Arc::clone(h.app_state.upstream_client()),
    };

    // Cursor seeded at 100, then the process is "down" while the chain
    // advances thousands of blocks and a payment lands in the middle. The order
    // was created 10 minutes before that payment; on the mock clock block 3_900
    // is ~3 minutes old, i.e. comfortably inside its window.
    backdate_order(&h, amount_units, 600, "pending").await;
    *chain.safe_block.lock() = Some(100);
    service.tick().await.expect("seed cursor");
    *chain.safe_block.lock() = Some(4000);
    *chain.logs.lock() = vec![transfer_log_at(amount_units, "0xc0ffee", 0, 3900)];

    let report = service.tick().await.expect("catch-up tick");
    assert_eq!(report.from_block, 101, "{report:?}");
    assert_eq!(
        report.to_block, 4000,
        "one tick reaches the head: {report:?}"
    );
    assert_eq!(report.backlog_blocks, 0, "{report:?}");
    assert_eq!(
        report.credited, 1,
        "delayed payment still credits: {report:?}"
    );

    let (units, last) = h
        .db_pool
        .spawn_read(move |r| {
            Ok((
                openproxy_db::credits_ledger::balance_units(r, kid)?,
                openproxy_db::credits_ledger::last_scanned_block(r)?,
            ))
        })
        .await
        .expect("read balance/cursor");
    assert_eq!(units, 5_000_000, "credited exactly once");
    assert_eq!(last, 4000, "cursor fully caught up");
}

/// First boot WITHOUT a cursor after a long outage: the order's window closed
/// days ago and the process that should have credited it is gone. The payment
/// was made inside the window, so the matcher settles it against the BLOCK
/// timestamp — which means the bootstrap must reach back to the block carrying
/// the order's `created_at`, not the configured window and not "orders still
/// payable at startup". This is the regression: the order is `expired` days
/// before the boot, the payment is valid, and the deposit must not strand.
#[tokio::test]
async fn watcher_first_boot_credits_a_payment_days_after_the_order_expired_e2e() {
    let chain = Arc::new(MockChain::default());
    let rpc_addr = spawn_mock_rpc(Arc::clone(&chain));
    let h = TestHarness::new().await;

    let mut cfg = h
        .app_state
        .config()
        .credits
        .clone()
        .expect("credits config in harness");
    cfg.rpc_url = format!("http://{rpc_addr}");

    let plan = seed_plan(&h, 5.0, 5.0, 7).await;
    let kid = h.admin_key_id.0;
    let (status, order) = h
        .oneshot_json(h.admin_request(
            axum::http::method::Method::POST,
            "/v1/credits/order",
            json!({"tier_id": plan, "key_id": kid}).to_string(),
        ))
        .await;
    assert_eq!(status, axum::http::StatusCode::OK, "order: {order}");
    let amount_units = order["amount_units"].as_i64().expect("amount_units");

    // Chain head 200_000 at 2 s/block: block 71_000 is ~2.99 days old. The
    // order was created one minute before that block, so the payment sits
    // inside `[created_at, expires_at + grace]`; its window closed days before
    // this boot, exactly as a crash + restart days later finds it.
    *chain.safe_block.lock() = Some(200_000);
    *chain.logs.lock() = vec![transfer_log_at(amount_units, "0xd1a900", 0, 71_000)];
    let payment_age_secs = (200_000 - 71_000) * chain.block_secs;
    backdate_order(&h, amount_units, payment_age_secs + 60, "expired").await;

    let service = openproxy_server::background::credits_watcher::CreditsWatcherService {
        db_pool: Arc::clone(&h.db_pool),
        config: cfg,
        master_key: Arc::clone(&h.master_key),
        upstream_client: Arc::clone(h.app_state.upstream_client()),
    };

    // First boot: cursor is 0, so the origin is the block carrying the oldest
    // unsettled order's `created_at`, found by the bounded timestamp search.
    let report = service.tick().await.expect("bootstrap tick");
    assert_eq!(
        report.credited, 1,
        "an on-time payment for an expired order must be credited: {report:?}"
    );
    assert!(
        (70_880..=70_940).contains(&report.from_block),
        "origin anchored on the order's created_at, not the window: {report:?}"
    );
    assert!(
        report.to_block >= 71_000 && report.backlog_blocks > 100_000,
        "one bounded chunk per iteration, never an unbounded replay: {report:?}"
    );

    // The rest of the ~3-day gap is replayed in bounded chunks: several ticks,
    // still exactly one credit, cursor ending at the finality head.
    let mut credited = report.credited;
    let mut backlog = report.backlog_blocks;
    let mut ticks = 0;
    while backlog > 0 {
        ticks += 1;
        assert!(ticks <= 12, "bootstrap stalled with {backlog} blocks left");
        let next = service.tick().await.expect("catch-up tick");
        credited += next.credited;
        backlog = next.backlog_blocks;
    }
    assert_eq!(credited, 1, "the same payment is credited exactly once");
    assert!(ticks >= 2, "a days-long gap cannot fit in one tick");

    let (units, last, order_status) = h
        .db_pool
        .spawn_read(move |r| {
            Ok((
                openproxy_db::credits_ledger::balance_units(r, kid)?,
                openproxy_db::credits_ledger::last_scanned_block(r)?,
                r.query_row(
                    "SELECT status FROM credit_orders WHERE amount_units = ?1",
                    [amount_units],
                    |row| row.get::<_, String>(0),
                )
                .map_err(openproxy_db::error::map_db_error)?,
            ))
        })
        .await
        .expect("read balance/cursor/order");
    assert_eq!(units, 5_000_000, "base amount credited exactly once");
    assert_eq!(last, 200_000, "cursor caught up to the finality head");
    assert_eq!(order_status, "paid", "expired -> paid via the block window");

    let audit = h
        .db_pool
        .spawn_write(move |w| openproxy_db::credits_ledger::reconcile(w))
        .await
        .expect("reconcile");
    assert!(audit.is_consistent(), "audit: {audit:?}");
}

mod recovery;
