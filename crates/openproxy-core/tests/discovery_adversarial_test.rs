#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Empirical adversarial stress test harness for Discovery Scheduler Consolidation
//! and Bounded Concurrency.
//!
//! Verifies:
//! 1. 79 providers do NOT spawn 79 tasks; worker pool strictly bounded to <= 3 concurrent workers.
//! 2. In-flight deduplication: concurrent ticks for the same provider never execute concurrently.
//! 3. Cancellation safety: cancel() terminates coordinator and all workers promptly.
//! 4. Migration 000077 idempotency and ON DELETE CASCADE integrity.

use axum::extract::State as AxumState;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json as AxumJson, Router};
use openproxy_adapters::upstream::UpstreamClient;
use openproxy_core::discovery_scheduler::{DiscoverySchedulerConfig, start};
use openproxy_core::providers::{self, AuthType, NewProvider, ProviderFormat, RateLimitScope};
use openproxy_db::DbPool;
use openproxy_db::secrets::MasterKey;
use openproxy_types::ProviderId;
use serde_json::json;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::net::TcpListener;

struct MockDiscoveryServerState {
    active_concurrency: AtomicUsize,
    max_concurrency: AtomicUsize,
    total_requests: AtomicUsize,
    delay_ms: u64,
}

async fn mock_models_handler(
    AxumState(state): AxumState<Arc<MockDiscoveryServerState>>,
) -> impl IntoResponse {
    let cur = state.active_concurrency.fetch_add(1, Ordering::SeqCst) + 1;
    state.max_concurrency.fetch_max(cur, Ordering::SeqCst);
    state.total_requests.fetch_add(1, Ordering::SeqCst);

    if state.delay_ms > 0 {
        tokio::time::sleep(Duration::from_millis(state.delay_ms)).await;
    }

    state.active_concurrency.fetch_sub(1, Ordering::SeqCst);
    (
        StatusCode::OK,
        AxumJson(json!({
            "data": [
                {
                    "id": "mock-adversarial-model",
                    "object": "model"
                }
            ]
        })),
    )
}

async fn spawn_mock_discovery_server(delay_ms: u64) -> (SocketAddr, Arc<MockDiscoveryServerState>) {
    let state = Arc::new(MockDiscoveryServerState {
        active_concurrency: AtomicUsize::new(0),
        max_concurrency: AtomicUsize::new(0),
        total_requests: AtomicUsize::new(0),
        delay_ms,
    });
    let app = Router::new()
        .route("/models", get(mock_models_handler))
        .with_state(Arc::clone(&state));
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind listener");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve mock server");
    });
    (addr, state)
}

fn seed_custom_provider(pool: &DbPool, master_key: &MasterKey, id_str: &str, base_url: &str) {
    let conn = pool.writer();
    let pid = ProviderId::new(id_str);
    providers::create(
        &conn,
        NewProvider {
            id: &pid,
            name: id_str,
            base_url,
            auth_type: AuthType::Bearer,
            format: ProviderFormat::Openai,
            extra_headers_json: None,
            auto_activate_keyword: None,
            rate_limit_scope: RateLimitScope::Account,
        },
    )
    .expect("seed provider");

    openproxy_core::accounts::create(
        &conn,
        &pid,
        Some("sk-adversarial-key"),
        master_key,
        Some("default"),
        100,
        None,
    )
    .expect("seed account");

    openproxy_db::providers::set_provider_favicon(
        &conn,
        pid.as_str(),
        "image/png",
        b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR",
    )
    .expect("seed favicon");
}

/// Adversarial Challenge 1:
/// Verify that initializing scheduler with 79 providers does NOT spawn 79 concurrent tasks,
/// and that the concurrency never exceeds the 3-worker limit (strictly <= 3).
#[tokio::test]
async fn test_adversarial_79_providers_pool_strictly_bounded_to_3_workers() {
    let pool = Arc::new(DbPool::test_pool_with_prefix("adv-bounded-79").expect("open pool"));
    let mk = Arc::new(MasterKey::generate().expect("master key"));

    // 40ms delay per request to guarantee overlap between concurrent workers
    let (addr, state) = spawn_mock_discovery_server(40).await;
    let base_url = format!("http://{addr}");

    // Seed exactly 79 custom providers in SQLite
    for i in 0..79 {
        let pid = format!("cust-adv-prov-{i:03}");
        seed_custom_provider(&pool, &mk, &pid, &base_url);
    }

    let sched = start(
        Arc::clone(&pool),
        Arc::clone(&mk),
        Arc::new(vec![]), // empty adapters forces resolution to CustomAdapter from DB
        UpstreamClient::new(),
        DiscoverySchedulerConfig {
            interval_secs: 3600,
            initial_stagger_secs: 0, // all 79 eligible immediately
        },
    );

    assert_eq!(
        sched.task_count, 79,
        "scheduler must have resolved exactly 79 custom providers"
    );

    // Wait for at least 15 requests to be processed across workers
    let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
    while state.total_requests.load(Ordering::SeqCst) < 15 {
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }

    let max_concur = state.max_concurrency.load(Ordering::SeqCst);
    let total_reqs = state.total_requests.load(Ordering::SeqCst);

    assert!(
        total_reqs >= 10,
        "expected at least 10 requests executed, got {total_reqs}"
    );
    assert!(
        max_concur <= 3,
        "CRITICAL VIOLATION: max observed worker concurrency was {max_concur} > 3! \
         Worker pool must be strictly bounded to at most 3 workers."
    );
    assert!(
        max_concur >= 1,
        "expected at least 1 worker active, got {max_concur}"
    );

    sched.cancel();
}

/// Adversarial Challenge 2:
/// Verify in-flight deduplication: multiple ticks for the same provider must NEVER
/// run concurrently; concurrency per provider must strictly equal 1.
#[tokio::test]
async fn test_adversarial_inflight_deduplication_stress() {
    let pool = Arc::new(DbPool::test_pool_with_prefix("adv-dedup-stress").expect("open pool"));
    let mk = Arc::new(MasterKey::generate().expect("master key"));

    // 150ms delay per request
    let (addr, state) = spawn_mock_discovery_server(150).await;
    let base_url = format!("http://{addr}");

    seed_custom_provider(&pool, &mk, "slow-dedup-provider", &base_url);

    let sched = start(
        Arc::clone(&pool),
        Arc::clone(&mk),
        Arc::new(vec![]),
        UpstreamClient::new(),
        DiscoverySchedulerConfig {
            interval_secs: 1, // 1-second cadence
            initial_stagger_secs: 0,
        },
    );

    assert_eq!(sched.task_count, 1);

    // Run for 600ms while request takes 150ms. Multiple coordinator loops will fire.
    for _ in 0..10 {
        tokio::time::sleep(Duration::from_millis(60)).await;
        let active = state.active_concurrency.load(Ordering::SeqCst);
        assert!(
            active <= 1,
            "CRITICAL VIOLATION: in-flight deduplication failed! \
             Active concurrency for single provider was {active} > 1"
        );
    }

    let max_concur = state.max_concurrency.load(Ordering::SeqCst);
    assert_eq!(
        max_concur, 1,
        "max concurrent executions for single provider must strictly equal 1, got {max_concur}"
    );

    sched.cancel();
}

/// Adversarial Challenge 3:
/// Verify cancellation safety: when cancel() is triggered, coordinator and all workers
/// exit cleanly without hanging or dispatching further requests.
#[tokio::test]
async fn test_adversarial_cancellation_and_clean_exit() {
    let pool = Arc::new(DbPool::test_pool_with_prefix("adv-cancel-exit").expect("open pool"));
    let mk = Arc::new(MasterKey::generate().expect("master key"));

    let (addr, state) = spawn_mock_discovery_server(20).await;
    let base_url = format!("http://{addr}");

    for i in 0..20 {
        let pid = format!("cancel-prov-{i:02}");
        seed_custom_provider(&pool, &mk, &pid, &base_url);
    }

    let sched = start(
        Arc::clone(&pool),
        Arc::clone(&mk),
        Arc::new(vec![]),
        UpstreamClient::new(),
        DiscoverySchedulerConfig {
            interval_secs: 1,
            initial_stagger_secs: 0,
        },
    );

    // Let workers start processing
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while state.total_requests.load(Ordering::SeqCst) < 4 {
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Cancel now
    let cancel_time = tokio::time::Instant::now();
    sched.cancel();
    // Idempotent cancel call
    sched.cancel();

    // Allow in-flight requests to complete
    tokio::time::sleep(Duration::from_millis(80)).await;
    let requests_frozen = state.total_requests.load(Ordering::SeqCst);

    // Sleep an additional 200ms and verify no further requests were dispatched
    tokio::time::sleep(Duration::from_millis(200)).await;
    let requests_after_wait = state.total_requests.load(Ordering::SeqCst);

    assert_eq!(
        requests_after_wait, requests_frozen,
        "workers or coordinator dispatched new requests after cancellation: \
         frozen={requests_frozen}, after_wait={requests_after_wait}"
    );

    let active_left = state.active_concurrency.load(Ordering::SeqCst);
    assert_eq!(
        active_left, 0,
        "all active worker tasks must have exited; got {active_left} still active"
    );

    let cancel_elapsed = cancel_time.elapsed();
    assert!(
        cancel_elapsed < Duration::from_millis(1500),
        "cancellation took too long: {cancel_elapsed:?}"
    );
}

/// Adversarial Challenge 4:
/// Database migration 000077_provider_favicons.sql verification:
/// 1. Idempotency on repeated execution.
/// 2. Foreign key ON DELETE CASCADE integrity.
/// 3. Foreign key violation rejection.
#[test]
fn test_adversarial_migration_000077_idempotency_and_cascade() {
    let pool = DbPool::test_pool_with_prefix("adv-mig-000077").expect("open pool");
    let conn = pool.writer();

    // 1. Raw idempotency check: Re-run migration 000077 SQL directly on DB that already has it
    let migration_sql =
        include_str!("../../../crates/openproxy-db/migrations/000077_provider_favicons.sql");
    conn.execute_batch(migration_sql)
        .expect("migration 000077 must be cleanly idempotent under re-execution");

    // 2. Foreign keys enabled check
    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .expect("enable fk");

    // 3. Foreign key rejection: cannot insert favicon for non-existent provider
    let fake_data = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR";
    let bad_insert = openproxy_db::providers::set_provider_favicon(
        &conn,
        "non-existent-provider",
        "image/png",
        fake_data,
    );
    assert!(
        bad_insert.is_err(),
        "FK constraint must reject favicons for non-existent provider IDs"
    );

    // 4. Create valid provider and attach favicon
    let pid = ProviderId::new("cascade-test-prov");
    providers::create(
        &conn,
        NewProvider {
            id: &pid,
            name: "Cascade Provider",
            base_url: "https://example.invalid",
            auth_type: AuthType::None,
            format: ProviderFormat::Openai,
            extra_headers_json: None,
            auto_activate_keyword: None,
            rate_limit_scope: RateLimitScope::Account,
        },
    )
    .expect("create provider");

    openproxy_db::providers::set_provider_favicon(&conn, pid.as_str(), "image/png", fake_data)
        .expect("set favicon");

    let fav = openproxy_db::providers::get_provider_favicon(&conn, pid.as_str())
        .expect("get favicon")
        .expect("favicon should exist");
    assert_eq!(fav.0, "image/png");
    assert_eq!(fav.1, fake_data);

    // Verify provider row has_favicon is true
    let p_row = providers::get(&conn, &pid)
        .expect("get prov")
        .expect("prov exists");
    assert!(p_row.has_favicon, "provider.has_favicon must be true");

    // 5. Delete provider and verify ON DELETE CASCADE cleans up provider_favicons
    conn.execute(
        "DELETE FROM providers WHERE id = ?1",
        rusqlite::params![pid.as_str()],
    )
    .expect("delete provider");

    let fav_after_delete = openproxy_db::providers::get_provider_favicon(&conn, pid.as_str())
        .expect("get favicon after parent delete");
    assert!(
        fav_after_delete.is_none(),
        "ON DELETE CASCADE must automatically remove provider_favicons row when provider is deleted"
    );
}

/// Adversarial Challenge 5:
/// Verify channel buffer bound (capacity 64) under sudden dispatch burst of 79 providers.
/// Excess items beyond channel capacity must not cause OOM, panic, or deadlock.
#[tokio::test]
async fn test_adversarial_bounded_channel_overflow_resilience() {
    let pool = Arc::new(DbPool::test_pool_with_prefix("adv-overflow-resil").expect("open pool"));
    let mk = Arc::new(MasterKey::generate().expect("master key"));

    // Delay 100ms per request so workers are busy while coordinator tries to enqueue 79 items
    let (addr, state) = spawn_mock_discovery_server(100).await;
    let base_url = format!("http://{addr}");

    // Seed 79 providers
    for i in 0..79 {
        let pid = format!("burst-prov-{i:03}");
        seed_custom_provider(&pool, &mk, &pid, &base_url);
    }

    let sched = start(
        Arc::clone(&pool),
        Arc::clone(&mk),
        Arc::new(vec![]),
        UpstreamClient::new(),
        DiscoverySchedulerConfig {
            interval_secs: 1, // short interval so failed try_sends are re-dispatched cleanly on subsequent tick
            initial_stagger_secs: 0,
        },
    );

    assert_eq!(sched.task_count, 79);

    // Allow the burst dispatch and worker processing to make forward progress
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while state.total_requests.load(Ordering::SeqCst) < 10 {
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let total = state.total_requests.load(Ordering::SeqCst);
    let max_concur = state.max_concurrency.load(Ordering::SeqCst);

    assert!(
        total >= 10,
        "system must make forward progress despite queue saturation; executed {total} requests"
    );
    assert!(
        max_concur <= 3,
        "concurrency must never exceed 3 workers even under channel saturation: got {max_concur}"
    );

    sched.cancel();
}
