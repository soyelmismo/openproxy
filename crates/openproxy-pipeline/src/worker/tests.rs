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
