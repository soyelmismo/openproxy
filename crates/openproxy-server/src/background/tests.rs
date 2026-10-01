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
            () = s1.shutdown_and_wait() => {
                panic!("caller 1 should have been cancelled before drain completion");
            }
            () = trigger_clone.notified() => {}
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

#[tokio::test]
async fn supervisor_supervises_quota_sync_service() {
    let pool = Arc::new(
        openproxy_db::DbPool::test_pool_with_prefix("quota-sync-sup-test").expect("open pool"),
    );
    let mut config = openproxy_core::AppConfig::default();
    config.quota_sync.enabled = false;
    let upstream_client = openproxy_adapters::upstream::UpstreamClient::new();
    let master_key = Arc::new(openproxy_db::secrets::MasterKey::generate().expect("generate key"));
    let adapters = Arc::new(parking_lot::RwLock::new(Arc::new(
        openproxy_adapters::adapters::builtin_adapters(),
    )));
    let oauth_provider_registry = Arc::new(openproxy_core::oauth::OAuthProviderRegistry::new());

    let supervisor = BackgroundSupervisor::new();
    let service = QuotaSyncService {
        db_pool: pool,
        config,
        upstream_client,
        master_key,
        adapters,
        oauth_provider_registry,
    };
    assert_eq!(service.name(), "quota_sync");
    let spawned = supervisor.spawn(service);
    assert!(spawned);

    supervisor.shutdown_and_wait().await;
}

#[tokio::test]
async fn supervisor_supervises_minimax_checkin_service() {
    let pool = Arc::new(
        openproxy_db::DbPool::test_pool_with_prefix("minimax-checkin-sup-test").expect("open pool"),
    );
    let upstream_client = openproxy_adapters::upstream::UpstreamClient::new();
    let master_key = Arc::new(openproxy_db::secrets::MasterKey::generate().expect("generate key"));

    let supervisor = BackgroundSupervisor::new();
    let service = MiniMaxCheckinService {
        db_pool: pool,
        upstream_client,
        master_key,
    };
    assert_eq!(service.name(), "minimax_checkin");
    let spawned = supervisor.spawn(service);
    assert!(spawned);

    supervisor.shutdown_and_wait().await;
}

#[tokio::test]
async fn cancellation_bridge_retains_inflight_work_until_released() {
    let cancel = CancellationToken::new();
    let runner_cancel = cancel.clone();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (cancelled_tx, cancelled_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let mut handle = tokio::spawn(async move {
        scheduler_services::with_core_cancellation(runner_cancel, |core_cancel| async move {
            started_tx.send(()).expect("signal started");
            core_cancel.cancelled().await;
            cancelled_tx.send(()).expect("signal cancellation observed");
            release_rx.await.expect("release inflight work");
        })
        .await;
    });
    started_rx.await.expect("runner started");
    cancel.cancel();
    cancelled_rx.await.expect("cancellation mirrored");
    assert!(futures::poll!(&mut handle).is_pending());
    release_tx.send(()).expect("release runner");
    handle.await.expect("runner joined");
}

#[tokio::test]
async fn cancellation_bridge_passes_precancelled_token_to_runner() {
    let cancel = CancellationToken::new();
    cancel.cancel();
    scheduler_services::with_core_cancellation(cancel, |core_cancel| async move {
        assert!(core_cancel.is_cancelled());
    })
    .await;
}

#[tokio::test]
async fn spawn_one_shot_returns_false_on_shutdown_and_future_not_executed() {
    let supervisor = BackgroundSupervisor::new();
    supervisor.shutdown();

    let executed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let executed_clone = Arc::clone(&executed);

    let spawned = supervisor.spawn_one_shot("test_rejected", async move {
        executed_clone.store(true, Ordering::SeqCst);
    });

    assert!(
        !spawned,
        "spawn_one_shot must return false when supervisor is closed"
    );
    assert!(
        !executed.load(Ordering::SeqCst),
        "rejected one-shot future must never be executed"
    );
}

#[tokio::test]
async fn spawn_one_shot_shutdown_waits_until_release() {
    let supervisor = BackgroundSupervisor::new();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();

    let spawned = supervisor.spawn_one_shot("admitted_operation", async move {
        let _ = started_tx.send(());
        let _ = release_rx.await;
    });
    assert!(spawned, "task must be admitted");

    started_rx.await.expect("task must start");

    let shutdown_fut = supervisor.shutdown_and_wait();
    tokio::pin!(shutdown_fut);

    assert!(
        futures::poll!(&mut shutdown_fut).is_pending(),
        "shutdown must remain pending while in-flight task is unreleased"
    );

    release_tx.send(()).expect("release task");
    shutdown_fut.await;
}

#[tokio::test]
async fn spawn_one_shot_cancellation_of_first_shutdown_caller_retains_handle_for_second() {
    let supervisor = BackgroundSupervisor::new();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();

    let spawned = supervisor.spawn_one_shot("cancel_drain_task", async move {
        let _ = started_tx.send(());
        let _ = release_rx.await;
    });
    assert!(spawned);
    started_rx.await.expect("task started");

    // Scope 1: caller 1 starts shutdown_and_wait, polls it, transferring tasks to drain_lock.
    // Dropping caller 1 cancels its await while keeping the handle in drain_lock.
    {
        let caller1_fut = supervisor.shutdown_and_wait();
        tokio::pin!(caller1_fut);
        assert!(
            futures::poll!(&mut caller1_fut).is_pending(),
            "caller 1 must be pending before task release"
        );
    }

    // Verify the cancelled caller 1 retained the handle in drain_lock
    {
        let drain = supervisor.drain_lock.lock().await;
        assert_eq!(drain.len(), 1, "retained handle must remain in drain_lock");
    }

    // Caller 2 initiates shutdown_and_wait directly
    let caller2_fut = supervisor.shutdown_and_wait();
    tokio::pin!(caller2_fut);

    assert!(
        futures::poll!(&mut caller2_fut).is_pending(),
        "caller 2 must still wait for the retained task"
    );

    release_tx.send(()).expect("release task");
    caller2_fut.await;
}

#[tokio::test]
async fn spawn_one_shot_many_completed_tasks_do_not_grow_unboundedly() {
    let supervisor = BackgroundSupervisor::new();

    for i in 0..50 {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let spawned = supervisor.spawn_one_shot("short_task", async move {
            let _ = tx.send(i);
        });
        assert!(spawned);
        let res = rx.await.expect("task finished");
        assert_eq!(res, i);

        // Ensure task has completed before next admission
        while {
            let reg = supervisor.registry.lock();
            reg.tasks.iter().any(|h| !h.is_finished())
        } {
            tokio::task::yield_now().await;
        }
    }

    let (final_tx, final_rx) = tokio::sync::oneshot::channel();
    assert!(supervisor.spawn_one_shot("final_task", async move {
        let _ = final_tx.send(());
    }));
    final_rx.await.expect("final task finished");

    assert!(
        supervisor.task_count() <= 1,
        "finished tasks must be pruned on admission; task_count={}",
        supervisor.task_count()
    );
}
