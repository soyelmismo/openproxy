use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

struct TestCounterService {
    count: Arc<AtomicUsize>,
    started: Arc<tokio::sync::Notify>,
}

impl BackgroundService for TestCounterService {
    fn name(&self) -> &'static str {
        "test_counter"
    }

    async fn run(&self, cancel: CancellationToken) {
        let mut tick = tokio::time::interval(Duration::from_millis(5));
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                _ = tick.tick() => {
                    self.count.fetch_add(1, Ordering::Relaxed);
                    self.started.notify_one();
                }
            }
        }
    }
}

#[tokio::test]
async fn supervisor_spawns_and_shuts_down_service() {
    let supervisor = BackgroundSupervisor::new();
    let count = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(tokio::sync::Notify::new());

    let spawned = supervisor.spawn(TestCounterService {
        count: Arc::clone(&count),
        started: Arc::clone(&started),
    });
    assert!(spawned);

    // Wait deterministically for the first tick
    started.notified().await;
    assert!(count.load(Ordering::Relaxed) > 0);

    // Signal graceful shutdown and drain
    supervisor.shutdown_and_wait().await;
    let stopped_at = count.load(Ordering::Relaxed);

    tokio::time::sleep(Duration::from_millis(15)).await;
    assert_eq!(count.load(Ordering::Relaxed), stopped_at);
}

/// Smoke test for [`BackfillService`]: a fresh empty DB should
/// still complete one backfill pass and populate `last_run` /
/// `last_result` so the admin UI can clear the "warming up" banner.
/// We use a 60s interval so the service only runs one pass during
/// the test and exits cleanly on shutdown.
#[tokio::test]
async fn backfill_service_completes_initial_pass_on_empty_db() {
    use openproxy_db::DbPool;

    let pool =
        Arc::new(DbPool::test_pool_with_prefix("openproxy-backfill-test").expect("open pool"));

    let status = Arc::new(parking_lot::RwLock::new(
        crate::state::BackfillStatus::default(),
    ));
    let supervisor = BackgroundSupervisor::new();
    let spawned = supervisor.spawn(BackfillService {
        db_pool: Arc::clone(&pool),
        backfill_status: Arc::clone(&status),
        interval: Duration::from_secs(60),
    });
    assert!(spawned);

    // Poll the status for up to 5s waiting for the first pass to
    // finish. On an empty DB the backfill is fast (just seeding
    // built-in providers + the bootstrap key).
    let mut completed = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if status.read().last_run.is_some() {
            completed = true;
            break;
        }
    }
    supervisor.shutdown_and_wait().await;

    let s = status.read().clone();
    assert!(completed, "backfill pass did not complete; status={s:?}");
    assert_eq!(s.last_result.as_deref(), Some("ok"));
    assert!(!s.in_progress, "status still in_progress after pass");
}

#[tokio::test]
async fn memory_cleanup_service_prunes_abandoned_inflight_and_trims() {
    use openproxy_db::DbPool;
    use openproxy_types::usage::InflightAttempt;

    let pool =
        Arc::new(DbPool::test_pool_with_prefix("openproxy-mem-cleanup-test").expect("open pool"));
    let selection_registry = Arc::new(openproxy_types::SelectionRegistry::new());
    let circuit_breaker = openproxy_pipeline::circuit_breaker::CircuitBreakerRegistry::new(
        &openproxy_types::config::CircuitBreakerConfig {
            failure_threshold: 5,
            unhealthy_duration_ms: 60_000,
        },
    );
    let predictive_limiter = Arc::new(openproxy_pipeline::PredictiveRateLimiter::new());
    let api_key_cache = Arc::new(dashmap::DashMap::new());

    let service = MemoryCleanupService {
        db_pool: Arc::clone(&pool),
        selection_registry,
        circuit_breaker,
        predictive_limiter,
        api_key_cache,
    };

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let fresh_key = "test-fresh-attempt-key".to_string();
    let stale_key = "test-stale-attempt-key".to_string();

    let make_attempt = |key: &str, updated_at_ms: u64| InflightAttempt {
        attempt_key: key.to_string(),
        request_id: "req".to_string(),
        trace_id: "trace".to_string(),
        provider_id: "p".to_string(),
        upstream_model_id: "m".to_string(),
        started_at_ms: updated_at_ms,
        updated_at_ms,
        stage: "started".to_string(),
        stage_seq: 1,
        stage_rank: 0,
        elapsed_ms_at_event: 0,
        connect_ms: None,
        ttft_ms: None,
        status_code: None,
        terminal: false,
        terminal_kind: None,
        error: None,
        row_id: None,
        source: "proxy".to_string(),
        endpoint_kind: None,
    };

    openproxy_core::usage::INFLIGHT_REGISTRY
        .insert(fresh_key.clone(), make_attempt(&fresh_key, now_ms));
    openproxy_core::usage::INFLIGHT_REGISTRY.insert(
        stale_key.clone(),
        make_attempt(&stale_key, now_ms.saturating_sub(400_000)),
    );

    service.run_cleanup_pass().await;

    assert!(
        !openproxy_core::usage::INFLIGHT_REGISTRY.contains_key(&stale_key),
        "stale inflight attempt (>300s) should have been pruned"
    );
    assert!(
        openproxy_core::usage::INFLIGHT_REGISTRY.contains_key(&fresh_key),
        "fresh inflight attempt should have been retained"
    );

    // Clean up
    openproxy_core::usage::INFLIGHT_REGISTRY.remove(&fresh_key);
}

struct SlowInflightService {
    started: Arc<tokio::sync::Notify>,
    finished: Arc<tokio::sync::Notify>,
}

impl BackgroundService for SlowInflightService {
    fn name(&self) -> &'static str {
        "slow_inflight"
    }

    async fn run(&self, cancel: CancellationToken) {
        self.started.notify_one();
        cancel.cancelled().await;
        // Simulate in-flight work that must be drained before task termination
        tokio::time::sleep(Duration::from_millis(20)).await;
        self.finished.notify_one();
    }
}

#[tokio::test]
async fn supervisor_shutdown_and_wait_drains_inflight_service() {
    let supervisor = BackgroundSupervisor::new();
    let started = Arc::new(tokio::sync::Notify::new());
    let finished = Arc::new(tokio::sync::Notify::new());

    let spawned = supervisor.spawn(SlowInflightService {
        started: Arc::clone(&started),
        finished: Arc::clone(&finished),
    });
    assert!(spawned);

    // Deterministically wait until service has started
    started.notified().await;

    // Concurrent shutdown_and_wait calls: both must wait until drain is complete
    let s1 = supervisor.clone();
    let s2 = supervisor.clone();
    let (res1, res2) = tokio::join!(
        tokio::spawn(async move { s1.shutdown_and_wait().await }),
        tokio::spawn(async move { s2.shutdown_and_wait().await }),
    );
    res1.expect("s1 join");
    res2.expect("s2 join");

    // After shutdown_and_wait, finished must have been notified
    assert!(
        tokio::time::timeout(Duration::from_millis(10), finished.notified())
            .await
            .is_ok(),
        "finished must be notified before shutdown_and_wait returns"
    );
}

#[tokio::test]
async fn supervisor_stops_new_spawns_after_shutdown() {
    let supervisor = BackgroundSupervisor::new();
    supervisor.shutdown();

    let started = Arc::new(tokio::sync::Notify::new());
    let spawned = supervisor.spawn(SlowInflightService {
        started: Arc::clone(&started),
        finished: Arc::new(tokio::sync::Notify::new()),
    });

    assert!(
        !spawned,
        "spawn must return false after supervisor shutdown"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(10), started.notified())
            .await
            .is_err(),
        "rejected service must never be started"
    );
}

struct CancelDrainService {
    started: Arc<tokio::sync::Notify>,
    cancelled_seen: Arc<tokio::sync::Notify>,
    step1_gate: Arc<tokio::sync::Notify>,
    completed: Arc<tokio::sync::Notify>,
}

impl BackgroundService for CancelDrainService {
    fn name(&self) -> &'static str {
        "cancel_drain"
    }

    async fn run(&self, cancel: CancellationToken) {
        self.started.notify_one();
        cancel.cancelled().await;
        self.cancelled_seen.notify_one();
        self.step1_gate.notified().await;
        self.completed.notify_one();
    }
}

#[tokio::test]
async fn supervisor_cancellation_of_one_shutdown_caller_does_not_block_next() {
    let supervisor = BackgroundSupervisor::new();
    let started = Arc::new(tokio::sync::Notify::new());
    let cancelled_seen = Arc::new(tokio::sync::Notify::new());
    let step1_gate = Arc::new(tokio::sync::Notify::new());
    let completed = Arc::new(tokio::sync::Notify::new());

    let spawned = supervisor.spawn(CancelDrainService {
        started: Arc::clone(&started),
        cancelled_seen: Arc::clone(&cancelled_seen),
        step1_gate: Arc::clone(&step1_gate),
        completed: Arc::clone(&completed),
    });
    assert!(spawned);

    // Deterministically ensure service is actively running
    started.notified().await;

    // Caller 1 starts shutdown_and_wait(), but is dropped/cancelled early while waiting for drain
    let trigger_cancel = Arc::new(tokio::sync::Notify::new());
    let s1 = supervisor.clone();
    let trigger_clone = Arc::clone(&trigger_cancel);

    let caller1 = tokio::spawn(async move {
        tokio::select! {
            _ = s1.shutdown_and_wait() => {
                panic!("caller 1 should have been cancelled before drain completion");
            }
            _ = trigger_clone.notified() => {}
        }
    });

    // Wait until supervisor has triggered cancellation and caller1 is awaiting the task handle
    cancelled_seen.notified().await;

    // Trigger cancellation of caller 1 (drops caller 1's shutdown_and_wait future & mutex guard)
    trigger_cancel.notify_one();
    caller1.await.expect("caller 1 select task joined");

    // Unblock the background service to allow it to finish
    step1_gate.notify_one();

    // Caller 2 initiates shutdown_and_wait(): must acquire lock without deadlocking and complete drain
    supervisor.shutdown_and_wait().await;

    // Ensure the service finished cleanly
    assert!(
        tokio::time::timeout(Duration::from_millis(50), completed.notified())
            .await
            .is_ok(),
        "service completed notification must be received"
    );

    // Caller 3 verifies idempotence after drain
    supervisor.shutdown_and_wait().await;
}
