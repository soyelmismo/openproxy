use super::*;

fn job() -> BackgroundJob {
    BackgroundJob::MarkClientResponse {
        request_id: "restart-test".into(),
        attempt: 1,
        target_id: ComboTargetId(42),
    }
}

#[tokio::test]
async fn startup_replays_admitted_jobs_without_a_notification() {
    let conn = Arc::new(parking_lot::Mutex::new(
        openproxy_db::testing::open_in_memory(),
    ));
    let journal_conn = Arc::clone(&conn);
    tokio::task::spawn_blocking(move || admit_job(&journal_conn, &job()))
        .await
        .unwrap()
        .unwrap();
    let (_sender, receiver) = mpsc::channel(1);
    let worker = spawn_worker(conn, receiver);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if worker.stats().await.unwrap().pending == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn replay_failure_is_retained_and_retried_after_repair() {
    let conn = Arc::new(parking_lot::Mutex::new(
        openproxy_db::testing::open_in_memory(),
    ));
    let setup = Arc::clone(&conn);
    tokio::task::spawn_blocking(move || {
        let payload = serde_json::to_string(&job()).unwrap();
        let mut db = setup.lock();
        openproxy_db::usage_journal::append(&mut db, &payload).unwrap();
        db.execute_batch("CREATE TRIGGER fail_ack BEFORE DELETE ON usage_journal BEGIN SELECT RAISE(ABORT, 'failed'); END;").unwrap();
    }).await.unwrap();
    let (_sender, receiver) = mpsc::channel(1);
    let worker = spawn_worker(Arc::clone(&conn), receiver);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if worker.stats().await.unwrap().failed_batches > 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(worker.stats().await.unwrap().pending, 1);
    tokio::task::spawn_blocking(move || conn.lock().execute_batch("DROP TRIGGER fail_ack"))
        .await
        .unwrap()
        .unwrap();
    worker.shutdown().await.unwrap();
    assert_eq!(worker.stats().await.unwrap().pending, 0);
}

#[test]
fn payload_round_trip_preserves_winner_identity() {
    let payload = serde_json::to_string(&job()).unwrap();
    let decoded = serde_json::from_str(&payload).unwrap();
    let BackgroundJob::MarkClientResponse {
        request_id,
        attempt,
        target_id,
    } = decoded
    else {
        panic!("wrong job")
    };
    assert_eq!(request_id, "restart-test");
    assert_eq!(attempt, 1);
    assert_eq!(target_id, ComboTargetId(42));
}

async fn fill_journal_to_capacity(conn: &Arc<parking_lot::Mutex<Connection>>) {
    let conn = Arc::clone(conn);
    let payload = serde_json::to_string(&job()).unwrap();
    tokio::task::spawn_blocking(move || {
        conn.lock()
            .execute(
                "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<100000)
                INSERT INTO usage_journal(payload) SELECT ?1 FROM n;",
                [&payload],
            )
            .unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn coordinator_subscribe_before_notify_drained() {
    let coord = JournalCoordinator::new();
    let wait = coord.subscribe();
    coord.notify_drained(1);
    let res = tokio::time::timeout(std::time::Duration::from_secs(5), wait).await;
    assert!(res.is_ok());
    assert_eq!(coord.acked_total(), 1);
}

#[tokio::test]
async fn coordinator_multiple_waiters_all_wake() {
    let coord = Arc::new(JournalCoordinator::new());
    let mut handles = Vec::new();
    for _ in 0..3 {
        let c = Arc::clone(&coord);
        handles.push(tokio::spawn(async move {
            c.subscribe().await;
        }));
    }
    tokio::task::yield_now().await;
    coord.notify_drained(5);
    for h in handles {
        let res = tokio::time::timeout(std::time::Duration::from_secs(5), h).await;
        assert!(res.is_ok());
    }
    assert_eq!(coord.acked_total(), 5);
}

#[tokio::test]
async fn coordinator_notify_closed_does_not_change_ack_count() {
    let coord = JournalCoordinator::new();
    let wait = coord.subscribe();
    coord.notify_closed();
    let res = tokio::time::timeout(std::time::Duration::from_secs(5), wait).await;
    assert!(res.is_ok());
    assert_eq!(coord.acked_total(), 0);
}

#[tokio::test]
async fn backpressure_full_waits_for_drain_and_admits() {
    let conn = Arc::new(parking_lot::Mutex::new(
        openproxy_db::testing::open_in_memory(),
    ));
    let coord = coordinator_for(&conn);
    let (sender, receiver) = mpsc::channel(1024);

    // 1. Fill SQLite journal to 100,000 capacity in spawn_blocking
    fill_journal_to_capacity(&conn).await;

    // 2. Producer will block because journal is at MAX_PENDING_JOBS (100,000)
    let conn_clone = Arc::clone(&conn);
    let sender_clone = sender.clone();
    let coord_clone = Arc::clone(&coord);
    let producer_task = tokio::spawn(async move {
        enqueue_with_backpressure(
            &conn_clone,
            &sender_clone,
            BackgroundJob::MarkClientResponse {
                request_id: "admitted-after-drain".into(),
                attempt: 1,
                target_id: ComboTargetId(43),
            },
            Some(&coord_clone),
        )
        .await
    });

    // 3. Await deterministically until producer hits capacity and enters wait state
    coord.await_waiter().await;
    assert!(!producer_task.is_finished());

    // 4. Deterministic drain without 100k worker replay cost:
    // Acknowledge exactly ONE entry in spawn_blocking and notify coordinator of 1 ACK
    let drain_conn = Arc::clone(&conn);
    tokio::task::spawn_blocking(move || {
        let db = drain_conn.lock();
        let id: i64 = db
            .query_row("SELECT MIN(id) FROM usage_journal", [], |r| r.get(0))
            .unwrap();
        openproxy_db::usage_journal::acknowledge(&db, id).unwrap();
    })
    .await
    .unwrap();
    coord.notify_drained(1);

    // 5. Producer wakes up from drain notification, appends to the freed slot, and succeeds
    let producer_result = tokio::time::timeout(std::time::Duration::from_secs(5), producer_task)
        .await
        .expect("producer did not hang")
        .expect("join handle ok");
    assert!(producer_result.is_ok());

    // 6. Verify depth is back to 100,000 in spawn_blocking
    let check_conn = Arc::clone(&conn);
    let depth = tokio::task::spawn_blocking(move || {
        openproxy_db::usage_journal::depth(&check_conn.lock()).unwrap()
    })
    .await
    .unwrap();
    assert_eq!(depth, 100_000);

    // 7. Drop receiver to close channel; zero 100k replay cost
    drop(receiver);
}

/// Tests cancellation of a producer waiting for journal capacity.
///
/// Note on cancellation semantics:
/// When a producer is awaiting `drain_wait` (capacity full), cancelling the future
/// aborts cleanly before any SQLite insert occurs.
/// However, if cancellation occurs after `spawn_blocking` has been scheduled or while
/// SQLite `append` is committing, the transaction will complete in the blocking pool
/// making the record durable in SQLite `usage_journal`. In that case, durability is
/// preserved and the worker will eventually replay the admitted job.
#[tokio::test]
async fn backpressure_cancel_aborts_wait_cleanly() {
    let conn = Arc::new(parking_lot::Mutex::new(
        openproxy_db::testing::open_in_memory(),
    ));
    let coord = coordinator_for(&conn);
    let (sender, _receiver) = mpsc::channel(1024);

    // Fill to 100,000 capacity in spawn_blocking
    fill_journal_to_capacity(&conn).await;

    let check_conn = Arc::clone(&conn);
    let depth = tokio::task::spawn_blocking(move || {
        openproxy_db::usage_journal::depth(&check_conn.lock()).unwrap()
    })
    .await
    .unwrap();
    assert_eq!(depth, 100_000);

    // Spawn producer task and explicitly abort it via JoinHandle while waiting on drain
    let conn_clone = Arc::clone(&conn);
    let sender_clone = sender.clone();
    let coord_clone = Arc::clone(&coord);
    let producer_task = tokio::spawn(async move {
        enqueue_with_backpressure(&conn_clone, &sender_clone, job(), Some(&coord_clone)).await
    });

    // Await deterministically until producer hits capacity and enters wait state
    coord.await_waiter().await;
    assert!(!producer_task.is_finished());

    // Abort the waiting producer task
    producer_task.abort();
    let join_res = producer_task.await;
    assert!(join_res.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn backpressure_shutdown_wakes_waiting_producer_with_error() {
    let conn = Arc::new(parking_lot::Mutex::new(
        openproxy_db::testing::open_in_memory(),
    ));
    let coord = coordinator_for(&conn);
    let (sender, mut receiver) = mpsc::channel(1024);

    // Fill to 100,000 capacity in spawn_blocking
    fill_journal_to_capacity(&conn).await;

    // Producer blocks waiting for capacity
    let conn_clone = Arc::clone(&conn);
    let sender_clone = sender.clone();
    let coord_clone = Arc::clone(&coord);
    let producer_task = tokio::spawn(async move {
        enqueue_with_backpressure(&conn_clone, &sender_clone, job(), Some(&coord_clone)).await
    });

    // Await deterministically until producer hits capacity and enters wait state
    coord.await_waiter().await;
    assert!(!producer_task.is_finished());

    // Close receiver directly without spawning a worker or draining 100k entries
    receiver.close();
    drop(receiver);

    // Producer must wake up with error rather than hanging
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), producer_task)
        .await
        .expect("producer unblocked on shutdown")
        .expect("join handle ok");

    assert!(result.is_err());
}

#[tokio::test]
async fn db_full_real_returns_explicit_error_without_retry() {
    let conn = Arc::new(parking_lot::Mutex::new(
        openproxy_db::testing::open_in_memory(),
    ));
    // Trigger simulating real database/disk full failure in spawn_blocking
    let trig_conn = Arc::clone(&conn);
    tokio::task::spawn_blocking(move || {
        trig_conn
            .lock()
            .execute_batch(
                "CREATE TRIGGER fail_disk BEFORE INSERT ON usage_journal BEGIN SELECT RAISE(ABORT, 'database or disk is full'); END;",
            )
            .unwrap();
    })
    .await
    .unwrap();

    let (sender, _receiver) = mpsc::channel(1024);
    let result = enqueue_with_backpressure(&conn, &sender, job(), None).await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(!err.is_journal_capacity_exhausted());

    let drop_conn = Arc::clone(&conn);
    tokio::task::spawn_blocking(move || {
        drop_conn
            .lock()
            .execute_batch("DROP TRIGGER fail_disk")
            .unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn closed_channel_returns_error_immediately() {
    let conn = Arc::new(parking_lot::Mutex::new(
        openproxy_db::testing::open_in_memory(),
    ));
    let (sender, receiver) = mpsc::channel::<BackgroundJob>(1);
    drop(receiver);

    let result = enqueue_with_backpressure(&conn, &sender, job(), None).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn worker_shutdown_fails_when_pending_jobs_remain() {
    let conn = Arc::new(parking_lot::Mutex::new(
        openproxy_db::testing::open_in_memory(),
    ));
    let setup = Arc::clone(&conn);
    tokio::task::spawn_blocking(move || {
        let payload = serde_json::to_string(&job()).unwrap();
        let mut db = setup.lock();
        openproxy_db::usage_journal::append(&mut db, &payload).unwrap();
        db.execute_batch("CREATE TRIGGER block_ack_always BEFORE DELETE ON usage_journal BEGIN SELECT RAISE(ABORT, 'permanent failure'); END;").unwrap();
    }).await.unwrap();

    let (_sender, receiver) = mpsc::channel(1);
    let worker = spawn_worker(Arc::clone(&conn), receiver);

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if worker.stats().await.unwrap().failed_batches > 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    // Shutdown must fail because pending jobs / failed batches remain
    let shutdown_result = worker.shutdown().await;
    assert!(
        shutdown_result.is_err(),
        "shutdown must not return success when pending jobs / failed batches exist"
    );
}
