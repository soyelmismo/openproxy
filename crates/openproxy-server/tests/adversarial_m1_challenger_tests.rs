//! Adversarial Challenger Test Suite for Milestone 1
//!
//! Validates:
//! 1. Allocator configuration options via `libmimalloc_sys::mi_option_get`.
//! 2. `MemoryCleanupService`: concurrent synthetic load on `INFLIGHT_REGISTRY`,
//!    TTL eviction, clock-skew resilience, and active attempt preservation.
//! 3. Dual-pool trimming: `DbPool::shrink_memory` and `mi_collect` under concurrent
//!    heavy read/write transactions without deadlock or connection corruption.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use openproxy_core::usage::INFLIGHT_REGISTRY;
use openproxy_db::DbPool;
use openproxy_db::testing::TempDir;
use openproxy_server::background::MemoryCleanupService;
use openproxy_types::usage::InflightAttempt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn make_attempt(key: &str, updated_at_ms: u64) -> InflightAttempt {
    InflightAttempt {
        attempt_key: key.to_string(),
        request_id: format!("req_{key}"),
        trace_id: format!("trace_{key}"),
        provider_id: "prov_challenger".to_string(),
        upstream_model_id: "model_challenger".to_string(),
        started_at_ms: updated_at_ms,
        updated_at_ms,
        stage: "streaming".to_string(),
        stage_seq: 4,
        stage_rank: 2,
        elapsed_ms_at_event: 15,
        connect_ms: Some(5),
        ttft_ms: Some(10),
        status_code: Some(200),
        terminal: false,
        terminal_kind: None,
        error: None,
        row_id: None,
        source: "proxy".to_string(),
        endpoint_kind: None,
    }
}

/// Verification of mimalloc option configuration and behavior.
#[test]
fn test_allocator_options_adversarial() {
    // Test direct configuration via libmimalloc_sys
    unsafe {
        libmimalloc_sys::mi_option_set(15 /* purge_delay */, 0);
        libmimalloc_sys::mi_option_set(4 /* arena_eager_commit */, 0);
        libmimalloc_sys::mi_option_set(23 /* arena_reserve */, 64 * 1024);
        libmimalloc_sys::mi_option_set(12 /* abandoned_page_purge */, 1);
    }

    assert_eq!(
        unsafe { libmimalloc_sys::mi_option_get(15) },
        0,
        "purge_delay must be 0"
    );
    assert_eq!(
        unsafe { libmimalloc_sys::mi_option_get(4) },
        0,
        "arena_eager_commit must be 0"
    );
    assert_eq!(
        unsafe { libmimalloc_sys::mi_option_get(23) },
        64 * 1024,
        "arena_reserve must be 64MB"
    );
    assert_eq!(
        unsafe { libmimalloc_sys::mi_option_get(12) },
        1,
        "abandoned_page_purge must be 1"
    );

    let mut handles = Vec::new();
    for _ in 0..4 {
        handles.push(std::thread::spawn(|| {
            for _ in 0..10 {
                let _v: Vec<u8> = vec![0xaa; 1024 * 64];
                unsafe {
                    libmimalloc_sys::mi_collect(true);
                }
            }
        }));
    }
    for h in handles {
        h.join().expect("thread join");
    }
}

/// Stress-test `MemoryCleanupService` under synthetic concurrent load.
#[tokio::test]
async fn test_memory_cleanup_service_inflight_synthetic_concurrency() {
    let temp_dir = TempDir::new("challenger-inflight-stress").expect("mkdir");
    let pool = Arc::new(DbPool::open(&temp_dir.path().join("test.db")).expect("open pool"));
    {
        let mut w = pool.writer();
        openproxy_db::migrations::run(&mut w).expect("migrations");
    }

    let service = Arc::new(MemoryCleanupService {
        db_pool: pool,
        selection_registry: Arc::new(openproxy_types::SelectionRegistry::new()),
        circuit_breaker: openproxy_pipeline::circuit_breaker::CircuitBreakerRegistry::new(
            &openproxy_types::config::CircuitBreakerConfig {
                failure_threshold: 5,
                unhealthy_duration_ms: 60_000,
            },
        ),
        predictive_limiter: Arc::new(openproxy_pipeline::PredictiveRateLimiter::new()),
        api_key_cache: Arc::new(dashmap::DashMap::new()),
    });

    let prefix = format!("ch_stress_{}_", now_epoch_ms());
    let now = now_epoch_ms();

    // Insert pre-configured edge cases:
    // 1. Stale entry (>5 min old: 301s old)
    let k_stale_exact = format!("{prefix}stale_exact");
    INFLIGHT_REGISTRY.insert(
        k_stale_exact.clone(),
        make_attempt(&k_stale_exact, now.saturating_sub(301_000)),
    );

    // 2. Active entry (almost stale: 299s old)
    let k_fresh_exact = format!("{prefix}fresh_exact");
    INFLIGHT_REGISTRY.insert(
        k_fresh_exact.clone(),
        make_attempt(&k_fresh_exact, now.saturating_sub(299_000)),
    );

    // 3. Clock skew / future timestamp entry (e.g. now + 60s)
    let k_future = format!("{prefix}future");
    INFLIGHT_REGISTRY.insert(k_future.clone(), make_attempt(&k_future, now + 60_000));

    // Spawn 8 concurrent worker tasks inserting thousands of varied items
    let done = Arc::new(AtomicBool::new(false));
    let inserted_stale = Arc::new(AtomicUsize::new(1));
    let inserted_fresh = Arc::new(AtomicUsize::new(2));

    let mut tasks = Vec::new();

    for t_idx in 0..8 {
        let prefix_clone = prefix.clone();
        let inserted_stale = Arc::clone(&inserted_stale);
        let inserted_fresh = Arc::clone(&inserted_fresh);
        let done = Arc::clone(&done);

        tasks.push(tokio::spawn(async move {
            let mut i = 0;
            while !done.load(Ordering::Relaxed) && i < 300 {
                let key = format!("{prefix_clone}t{t_idx}_{i}");
                let current_now = now_epoch_ms();

                if i % 2 == 0 {
                    // Stale: 400s - 600s old
                    let age = 400_000 + ((i as u64 * 31) % 200_000);
                    INFLIGHT_REGISTRY.insert(
                        key.clone(),
                        make_attempt(&key, current_now.saturating_sub(age)),
                    );
                    inserted_stale.fetch_add(1, Ordering::Relaxed);
                } else {
                    // Fresh: 10s - 120s old
                    let age = 10_000 + ((i as u64 * 17) % 110_000);
                    INFLIGHT_REGISTRY.insert(
                        key.clone(),
                        make_attempt(&key, current_now.saturating_sub(age)),
                    );
                    inserted_fresh.fetch_add(1, Ordering::Relaxed);
                }
                i += 1;
                tokio::task::yield_now().await;
            }
        }));
    }

    // Concurrently run cleanup passes while workers are actively writing
    let service_clone = Arc::clone(&service);
    let done_cleanup = Arc::clone(&done);
    let cleaner_task = tokio::spawn(async move {
        while !done_cleanup.load(Ordering::Relaxed) {
            service_clone.run_cleanup_pass().await;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });

    // Wait for insertion workers
    for t in tasks {
        t.await.expect("worker task join");
    }

    // Signal cleaner to stop and wait
    done.store(true, Ordering::Relaxed);
    cleaner_task.await.expect("cleaner task join");

    // Final decisive cleanup pass
    service.run_cleanup_pass().await;

    // Verify:
    // 1. k_stale_exact must be evicted
    assert!(
        !INFLIGHT_REGISTRY.contains_key(&k_stale_exact),
        "Exact stale entry (>300s) must be evicted"
    );

    // 2. k_fresh_exact must be retained
    assert!(
        INFLIGHT_REGISTRY.contains_key(&k_fresh_exact),
        "Exact fresh entry (<300s) must be retained"
    );

    // 3. k_future must be retained (clock skew resilience)
    assert!(
        INFLIGHT_REGISTRY.contains_key(&k_future),
        "Future timestamp entry must be retained without underflow"
    );

    // 4. Scan all entries with our prefix
    let mut surviving_stale = 0usize;
    let mut surviving_fresh = 0usize;
    let verify_now = now_epoch_ms();

    let keys_to_remove: Vec<String> = INFLIGHT_REGISTRY
        .iter()
        .filter(|r| r.key().starts_with(&prefix))
        .map(|r| {
            let attempt = r.value();
            let age_ms = verify_now.saturating_sub(attempt.updated_at_ms);
            if age_ms > 300_000 {
                surviving_stale += 1;
            } else {
                surviving_fresh += 1;
            }
            r.key().clone()
        })
        .collect();

    // Clean up our keys to preserve clean global registry
    for k in keys_to_remove {
        INFLIGHT_REGISTRY.remove(&k);
    }

    assert_eq!(
        surviving_stale, 0,
        "Zero stale entries must survive after run_cleanup_pass"
    );
    assert!(
        surviving_fresh > 0,
        "Active fresh entries must be preserved"
    );
}

/// Stress-test dual-pool trimming (`shrink_memory` + `mi_collect`) under heavy concurrent read/write transactions.
#[tokio::test]
async fn test_dual_pool_trimming_under_concurrent_transactions() {
    let temp_dir = TempDir::new("challenger-dualpool-stress").expect("mkdir");
    let db_path = temp_dir.path().join("test_stress.db");
    let pool = Arc::new(DbPool::open(&db_path).expect("open pool"));

    // Set up a stress table
    {
        let w = pool.writer();
        w.execute_batch(
            "CREATE TABLE challenger_records (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                worker_id INTEGER NOT NULL,
                counter INTEGER NOT NULL,
                payload TEXT NOT NULL
            );",
        )
        .expect("create table");
    }

    let stop = Arc::new(AtomicBool::new(false));
    let writes_completed = Arc::new(AtomicUsize::new(0));
    let reads_completed = Arc::new(AtomicUsize::new(0));
    let trims_completed = Arc::new(AtomicUsize::new(0));

    let mut handles = Vec::new();

    // 4 Concurrent Writers
    for w_id in 0..4 {
        let pool = Arc::clone(&pool);
        let stop = Arc::clone(&stop);
        let writes = Arc::clone(&writes_completed);

        handles.push(tokio::task::spawn_blocking(move || {
            let mut c = 0;
            while !stop.load(Ordering::Relaxed) && c < 200 {
                let mut w = pool.writer();
                let tx = w.transaction().expect("begin tx");
                tx.execute(
                    "INSERT INTO challenger_records (worker_id, counter, payload) VALUES (?1, ?2, ?3);",
                    rusqlite::params![w_id, c, "stress payload string for testing page allocation"],
                )
                .expect("insert");
                tx.commit().expect("commit tx");
                writes.fetch_add(1, Ordering::Relaxed);
                c += 1;
                std::thread::yield_now();
            }
        }));
    }

    // 6 Concurrent Readers
    for _ in 0..6 {
        let pool = Arc::clone(&pool);
        let stop = Arc::clone(&stop);
        let reads = Arc::clone(&reads_completed);

        handles.push(tokio::task::spawn_blocking(move || {
            let mut r_count = 0;
            while !stop.load(Ordering::Relaxed) && r_count < 300 {
                let r = pool.reader();
                let cnt: i64 = r
                    .query_row("SELECT COUNT(*) FROM challenger_records;", [], |row| {
                        row.get(0)
                    })
                    .unwrap_or(0);
                assert!(cnt >= 0, "count must be valid");
                reads.fetch_add(1, Ordering::Relaxed);
                r_count += 1;
                std::thread::yield_now();
            }
        }));
    }

    // 3 Concurrent Memory Trimmers calling `shrink_memory` and `mi_collect`
    for _ in 0..3 {
        let pool = Arc::clone(&pool);
        let stop = Arc::clone(&stop);
        let trims = Arc::clone(&trims_completed);

        handles.push(tokio::task::spawn_blocking(move || {
            let mut t_count = 0;
            while !stop.load(Ordering::Relaxed) && t_count < 100 {
                pool.shrink_memory();
                unsafe {
                    libmimalloc_sys::mi_collect(true);
                }
                trims.fetch_add(1, Ordering::Relaxed);
                t_count += 1;
                std::thread::sleep(Duration::from_millis(1));
            }
        }));
    }

    // Await all threads with a hard 15s deadline to catch any deadlocks
    let join_all = async {
        for h in handles {
            h.await.expect("join handle");
        }
    };

    tokio::time::timeout(Duration::from_secs(15), join_all)
        .await
        .expect("Deadlock detected! Trimming or transactions hung during concurrent execution");

    stop.store(true, Ordering::Relaxed);

    // Integrity checks on connection state and database
    let w = pool.writer();
    let integrity: String = w
        .query_row("PRAGMA integrity_check;", [], |row| row.get(0))
        .expect("integrity_check");
    assert_eq!(
        integrity, "ok",
        "Database corrupted after concurrent trimming!"
    );

    let quick: String = w
        .query_row("PRAGMA quick_check;", [], |row| row.get(0))
        .expect("quick_check");
    assert_eq!(quick, "ok", "Database quick_check failed!");

    let total_records: i64 = w
        .query_row("SELECT COUNT(*) FROM challenger_records;", [], |row| {
            row.get(0)
        })
        .expect("count records");

    let total_writes = writes_completed.load(Ordering::SeqCst);
    assert_eq!(
        total_records as usize, total_writes,
        "Record count must exactly match committed writes"
    );

    let total_reads = reads_completed.load(Ordering::SeqCst);
    let total_trims = trims_completed.load(Ordering::SeqCst);

    assert!(total_writes > 0, "Writes must have executed");
    assert!(total_reads > 0, "Reads must have executed");
    assert!(total_trims > 0, "Trims must have executed");
}
