use super::*;

/// Regression: the bootstrap origin has NO fixed age horizon. An order that
/// has been `expired` for more than 90 days still owes the on-time payment made
/// inside its window, so the first boot must start at the block carrying that
/// order's `created_at` — not at `head - 90 days`. The old clamp
/// (`start.max(head - 3_888_000)`, 90 days at 2 s/block) moved the origin past
/// the payment on a fresh cursor and silently stranded the deposit while the
/// ledger stayed consistent (nothing credited, nothing flagged).
///
/// The catch-up stays safe without the clamp: the tick scans at most
/// `MAX_CHUNKS_PER_TICK * MAX_BLOCK_CHUNK` blocks and the timestamp search is
/// bounded to `MAX_BLOCK_TIME_SEARCH_STEPS` probes, so an ancient origin costs
/// extra ticks, never an unbounded request.
#[tokio::test]
async fn watcher_first_boot_credits_a_payment_older_than_ninety_days_e2e() {
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

    // Head 4_000_000 at 2 s/block, payment at block 5_000: the transfer is
    // (4_000_000 - 5_000) * 2 = 7_990_000 s ≈ 92.5 days old, past the old
    // 90-day horizon expressed in 2 s blocks (3_888_000 blocks).
    *chain.safe_block.lock() = Some(4_000_000);
    *chain.logs.lock() = vec![transfer_log_at(amount_units, "0x9d0a01", 0, 5_000)];
    let payment_age_secs = (4_000_000 - 5_000) * chain.block_secs;
    backdate_order(&h, amount_units, payment_age_secs + 60, "expired").await;

    let service = openproxy_server::background::credits_watcher::CreditsWatcherService {
        db_pool: Arc::clone(&h.db_pool),
        config: cfg,
        master_key: Arc::clone(&h.master_key),
        upstream_client: Arc::clone(h.app_state.upstream_client()),
    };

    // The mock `eth_getLogs` filters by the requested range, so a clamped
    // origin would return no log at all for the payment block.
    let report = service.tick().await.expect("bootstrap tick");
    assert_eq!(
        report.credited, 1,
        "a >90-day-old on-time payment must still be credited: {report:?}"
    );
    assert!(
        (4_800..=5_000).contains(&report.from_block),
        "origin anchored on the order's created_at, not head - 90 days: {report:?}"
    );
    assert!(
        report.backlog_blocks > 3_000_000,
        "the remaining history is caught up chunk by chunk, not replayed in one request: {report:?}"
    );

    let (units, order_status) = h
        .db_pool
        .spawn_read(move |r| {
            Ok((
                openproxy_db::credits_ledger::balance_units(r, kid)?,
                r.query_row(
                    "SELECT status FROM credit_orders WHERE amount_units = ?1",
                    [amount_units],
                    |row| row.get::<_, String>(0),
                )
                .map_err(openproxy_db::error::map_db_error)?,
            ))
        })
        .await
        .expect("read balance/order");
    assert_eq!(units, 5_000_000, "base amount credited exactly once");
    assert_eq!(order_status, "paid", "expired -> paid via the block window");
}

/// Finality: prefer `safe`, accept `finalized`, and REFUSE to scan when the
/// node offers neither. A `latest - N` heuristic is never accepted.
#[tokio::test]
async fn watcher_requires_safe_or_finalized_head_e2e() {
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

    let service = openproxy_server::background::credits_watcher::CreditsWatcherService {
        db_pool: Arc::clone(&h.db_pool),
        config: cfg,
        master_key: Arc::clone(&h.master_key),
        upstream_client: Arc::clone(h.app_state.upstream_client()),
    };

    // Seed the cursor at 100 through the real first tick.
    *chain.safe_block.lock() = Some(100);
    service.tick().await.expect("init tick");

    // No `safe`, but `finalized` is available: scan to it, not to latest.
    *chain.safe_block.lock() = None;
    *chain.finalized_block.lock() = Some(150);
    let report = service.tick().await.expect("finalized tick");
    assert_eq!(report.head_tag, "finalized", "{report:?}");
    assert_eq!(report.from_block, 101, "{report:?}");
    assert_eq!(report.to_block, 150, "{report:?}");

    // Neither tag: fail closed and hold the cursor.
    *chain.finalized_block.lock() = None;
    let err = service.tick().await.expect_err("must fail closed");
    assert!(
        err.contains("neither `safe` nor `finalized`"),
        "unexpected error: {err}"
    );
    let last = h
        .db_pool
        .spawn_read(openproxy_db::credits_ledger::last_scanned_block)
        .await
        .expect("read cursor");
    assert_eq!(last, 150, "failed tick must not advance the cursor");
}

/// A malformed log (bad word size / non-hex / native JSON number) must abort
/// the tick and HOLD the cursor: silently skipping it would lose a payment.
#[tokio::test]
async fn watcher_malformed_log_holds_cursor_e2e() {
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

    let service = openproxy_server::background::credits_watcher::CreditsWatcherService {
        db_pool: Arc::clone(&h.db_pool),
        config: cfg,
        master_key: Arc::clone(&h.master_key),
        upstream_client: Arc::clone(h.app_state.upstream_client()),
    };
    *chain.safe_block.lock() = Some(100);
    service.tick().await.expect("init tick");

    // A structurally broken log inside the next range.
    let mut bad = transfer_log_at(1_000_000, "0x0bad01", 0, 150);
    bad["data"] = json!("0x1234");
    *chain.safe_block.lock() = Some(200);
    *chain.logs.lock() = vec![bad];
    let err = service.tick().await.expect_err("malformed log must abort");
    assert!(err.contains("malformed log"), "unexpected error: {err}");
    let last = h
        .db_pool
        .spawn_read(openproxy_db::credits_ledger::last_scanned_block)
        .await
        .expect("read cursor");
    assert_eq!(last, 100, "cursor must not advance past a malformed log");

    // Once the RPC stops returning the broken entry, the same range is
    // scanned and a legitimate transfer in it is processed.
    *chain.logs.lock() = Vec::new();
    let report = service.tick().await.expect("retry after bad log");
    assert_eq!(report.from_block, 101, "{report:?}");
    assert_eq!(report.to_block, 200, "{report:?}");
    let last = h
        .db_pool
        .spawn_read(openproxy_db::credits_ledger::last_scanned_block)
        .await
        .expect("read cursor");
    assert_eq!(last, 200);
}

/// Root-fix regression (audit A2-06): a reorg that DROPS an already-credited
/// transfer is detected instead of persisting silently. The polling scanner
/// never re-reads scanned blocks, so the detection path is the post-scan
/// receipt audit over the reorgable `(finalized, safe]` zone — the credit
/// stands (operator decides the reversal) but the tick reports reorg_alerts
/// and a CRITICAL is logged.
#[tokio::test]
async fn watcher_reorg_audit_flags_dropped_credit_e2e() {
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
    assert_eq!(status, axum::http::StatusCode::OK, "plan order: {order}");
    let amount_units = order["amount_units"].as_i64().expect("amount_units");

    let service = openproxy_server::background::credits_watcher::CreditsWatcherService {
        db_pool: Arc::clone(&h.db_pool),
        config: cfg.clone(),
        master_key: Arc::clone(&h.master_key),
        upstream_client: Arc::clone(h.app_state.upstream_client()),
    };

    // Tick 1: head safe=100, finalized=100 — the transfer at block 100 is
    // already finalized, credits normally, the audit zone is empty.
    *chain.safe_block.lock() = Some(100);
    *chain.finalized_block.lock() = Some(100);
    *chain.logs.lock() = vec![transfer_log_at(amount_units, "0xfeedd00d", 0, 100)];
    let t1 = service.tick().await.expect("tick 1");
    assert_eq!(t1.credited, 1, "tick 1: {t1:?}");
    assert_eq!(t1.reorg_alerts, 0, "nothing reorged yet: {t1:?}");

    // Tick 2: safe advanced to 110 but finalized LAGS at 90 — block 100 now
    // sits in the reorgable zone — and the chain no longer carries the tx
    // (the reorg dropped it). The audit re-fetches the receipt and CRITICALs.
    *chain.safe_block.lock() = Some(110);
    *chain.finalized_block.lock() = Some(90);
    chain.logs.lock().clear();
    let t2 = service.tick().await.expect("tick 2");
    assert_eq!(t2.reorg_alerts, 1, "dropped credit must be flagged: {t2:?}");

    // The credit itself still stands (operator decides the reversal).
    let balance = h
        .db_pool
        .spawn_read(move |r| openproxy_db::credits_ledger::balance_units(r, kid))
        .await
        .expect("balance");
    assert_eq!(balance, 5_000_000, "credit stands; the flag is the fix");

    // Tick 3: finalized catches up past the payment block — the payment left
    // the audit zone, no repeated alarms.
    *chain.safe_block.lock() = Some(120);
    *chain.finalized_block.lock() = Some(120);
    let t3 = service.tick().await.expect("tick 3");
    assert_eq!(
        t3.reorg_alerts, 0,
        "verified payments leave the zone: {t3:?}"
    );
}

/// SECURITY REGRESSION (audit round 2, V-03/A4-09): `eth_getLogs` served by a
/// lagging node behind an RPC load balancer silently misses the range's logs
/// while the head probe was answered by a healthy node. The old tick marked
/// the chunk as scanned (cursor advanced) and the payment inside it was lost
/// with no error, no unmatched row — nothing. The range-reachability probe
/// must fail that tick and hold the cursor, so the next healthy tick rescan
/// the same range and the payment still lands.
#[tokio::test]
async fn lagging_getlogs_node_holds_the_cursor_instead_of_losing_payments() {
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
    assert_eq!(status, axum::http::StatusCode::OK, "plan order: {order}");
    let amount_units = order["amount_units"].as_i64().expect("amount_units");

    let service = openproxy_server::background::credits_watcher::CreditsWatcherService {
        db_pool: Arc::clone(&h.db_pool),
        config: cfg.clone(),
        master_key: Arc::clone(&h.master_key),
        upstream_client: Arc::clone(h.app_state.upstream_client()),
    };

    // Tick 1 at head 100, healthy node, no logs yet: the cursor lands at 100.
    *chain.safe_block.lock() = Some(100);
    service.tick().await.expect("init tick");
    let cursor = h
        .db_pool
        .spawn_read(openproxy_db::credits_ledger::last_scanned_block)
        .await
        .expect("cursor after init");
    assert_eq!(cursor, 100);

    // The buyer pays in block 120, but the node that serves getLogs (and hex
    // block lookups) only knows up to block 110: the log is invisible to it.
    // The `safe` tag still reports the TRUE head 120 — exactly the split-brain
    // an RPC load balancer produces.
    *chain.safe_block.lock() = Some(120);
    *chain.hide_blocks_above.lock() = Some(110);
    *chain.logs.lock() = vec![transfer_log_at(amount_units, "0x1a99b0", 0, 120)];

    let lagging = service.tick().await;
    assert!(
        lagging.is_err(),
        "a range the serving node cannot produce must fail the tick, got {lagging:?}"
    );

    // The payment was not credited and the cursor was NOT advanced past the
    // invisible range: nothing was lost, the tick is retryable.
    let cursor_after = h
        .db_pool
        .spawn_read(openproxy_db::credits_ledger::last_scanned_block)
        .await
        .expect("cursor after lagging tick");
    assert_eq!(
        cursor_after, 100,
        "the cursor must hold while the range end is unreachable"
    );
    let order_status = {
        let amount = amount_units;
        h.db_pool
            .spawn_read(move |r| {
                use rusqlite::OptionalExtension;
                r.query_row(
                    "SELECT status FROM credit_orders WHERE amount_units = ?1",
                    [amount],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(openproxy_db::error::map_db_error)
            })
            .await
            .expect("order status")
    };
    assert_eq!(order_status.as_deref(), Some("pending"));

    // The node catches up: the SAME range is rescanned and the payment lands.
    *chain.hide_blocks_above.lock() = None;
    let report = service.tick().await.expect("healthy tick after catch-up");
    assert_eq!(
        report.credited, 1,
        "rescan must credit the payment: {report:?}"
    );
    assert_eq!(
        report.logs_seen, 1,
        "the log was invisible before: {report:?}"
    );
    let balance = h
        .db_pool
        .spawn_read(move |r| openproxy_db::credits_ledger::balance_units(r, kid))
        .await
        .expect("balance");
    assert_eq!(balance, 5_000_000, "the payment finally credited");
}
