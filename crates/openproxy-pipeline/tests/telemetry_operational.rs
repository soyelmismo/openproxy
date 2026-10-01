#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Operational integration tests for `openproxy-pipeline` journal durability,
//! backpressure under concurrency, real subprocess SIGKILL crash recovery,
//! and native SQLite DiskFull and OS ENOSPC error handling.

#[path = "telemetry_operational/helpers.rs"]
mod helpers;

use helpers::{
    ChildGuard, create_test_db, generate_deterministic_request_ids, sample_large_record_usage_job,
    sample_record_usage_job, verify_path_is_on_tmpfs, verify_usage_records,
};
use openproxy_db::conn::DbPool;
use openproxy_pipeline::worker::{
    admit_job, coordinator_for, enqueue_with_backpressure, replay_pending, spawn_worker,
};
use openproxy_types::error::CoreError;
use openproxy_types::ids::RequestId;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

/// Helper entry point invoked in a dedicated child subprocess.
///
/// In standard test execution without the specific environment variable,
/// this test returns immediately as a no-op.
#[test]
fn helper_crash_subprocess() {
    let Ok(db_path) = std::env::var("OPENPROXY_CRASH_TEST_DB_PATH") else {
        return;
    };

    let pool = DbPool::open(Path::new(&db_path)).expect("child open db pool");
    let conn = pool.writer_arc();

    let count = 128;
    let expected_ids = generate_deterministic_request_ids(count, 20_000);
    for (i, req_id) in expected_ids.into_iter().enumerate() {
        let job = sample_record_usage_job(req_id, i as u64);
        admit_job(&conn, &job).expect("child admit job into journal");
    }

    let depth = openproxy_db::usage_journal::depth(&conn.lock()).expect("child journal depth");
    assert_eq!(depth, count as u64);

    // Notify parent via piped stdout marker
    println!("CRASH_CHILD_COMMITTED_AND_READY");
    std::io::stdout().flush().expect("flush child stdout");

    // Park / block on stdin so the child does not exit or replay until parent kills it
    let mut byte = [0u8; 1];
    let _ = std::io::stdin().read_exact(&mut byte);
    std::thread::park();
}

/// Test 1: Concurrently load usage journal with async producers (8 tasks x 64 jobs = 512 total)
/// across a bounded MPSC channel of capacity 8, retaining the receiver initially to create a real
/// channel barrier. Verifies bounded capacity fill, suspended producers under backpressure,
/// drain upon worker spawn, clean shutdown, and exact set parity.
#[tokio::test]
async fn test_backpressure_and_concurrent_load() {
    let (_pool, conn, _path) = create_test_db("op-backpressure");
    let coord = coordinator_for(&conn);
    let (sender, receiver) = tokio::sync::mpsc::channel(8);
    assert_eq!(sender.max_capacity(), 8);

    let total_producers = 8;
    let jobs_per_producer = 64;
    let total_jobs = total_producers * jobs_per_producer;
    let expected_ids = generate_deterministic_request_ids(total_jobs, 10_000);

    let chunks: Vec<Vec<RequestId>> = expected_ids
        .chunks_exact(jobs_per_producer)
        .map(|chunk| chunk.to_vec())
        .collect();

    let admitted_counter = Arc::new(AtomicUsize::new(0));
    let start_time = Instant::now();
    let mut handles = Vec::with_capacity(total_producers);

    // 1. Spawn producers while receiver is NOT yet consuming, creating a real channel barrier
    for (producer_idx, task_ids) in chunks.into_iter().enumerate() {
        let task_conn = Arc::clone(&conn);
        let task_sender = sender.clone();
        let task_coord = Arc::clone(&coord);
        let task_admitted = Arc::clone(&admitted_counter);

        handles.push(tokio::spawn(async move {
            for (j, req_id) in task_ids.into_iter().enumerate() {
                let global_idx = (producer_idx * jobs_per_producer + j) as u64;
                let job = sample_record_usage_job(req_id, global_idx);
                enqueue_with_backpressure(&task_conn, &task_sender, job, Some(&task_coord))
                    .await
                    .expect("producer admission with backpressure");
                task_admitted.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    // 2. Wait until channel capacity is filled to 8 and producers are stalled by backpressure
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let depth = tokio::task::spawn_blocking({
                let conn = Arc::clone(&conn);
                move || openproxy_db::usage_journal::depth(&conn.lock()).unwrap()
            })
            .await
            .unwrap();

            let admitted = admitted_counter.load(Ordering::Relaxed);
            if admitted == 8 && depth == 8 && sender.capacity() == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("bounded channel filled to 8 and remaining producers stalled by backpressure");

    assert_eq!(
        sender.capacity(),
        0,
        "bounded mpsc channel capacity must be 0"
    );
    assert_eq!(
        admitted_counter.load(Ordering::Relaxed),
        8,
        "exactly 8 jobs admitted while receiver held"
    );

    // 3. Spawn worker to begin draining the channel and unblock the backpressured producers
    let worker = spawn_worker(Arc::clone(&conn), receiver);

    // Keep original sender alive until worker drains, then drop to allow channel EOF
    drop(sender);

    // 4. Await all producer tasks to completion
    for handle in handles {
        handle.await.expect("producer task joined");
    }
    assert_eq!(admitted_counter.load(Ordering::Relaxed), total_jobs);

    // 5. Shutdown worker: drains all remaining pending jobs
    worker.shutdown().await.expect("worker shutdown and drain");
    let elapsed = start_time.elapsed();

    // 6. Verify exact set parity in spawn_blocking (no blocking Tokio thread)
    tokio::task::spawn_blocking({
        let conn = Arc::clone(&conn);
        let expected_ids = expected_ids.clone();
        move || verify_usage_records(&conn, &expected_ids)
    })
    .await
    .expect("verify usage records in spawn_blocking");

    let throughput = (total_jobs as f64) / elapsed.as_secs_f64();
    tracing::info!(
        total_jobs,
        elapsed_ms = elapsed.as_millis(),
        throughput_per_sec = throughput,
        "load and backpressure test verified successfully"
    );
}

/// Test 2: Real crash of a child subprocess via SIGKILL.
///
/// Spawns the test binary with `--exact helper_crash_subprocess --nocapture` and dedicated env vars.
/// The child admits 128 unique usage records to SQLite WAL journal and emits a piped stdout marker.
/// The parent reads the marker with a bounded timeout, kills the child via SIGKILL (`child.kill()`),
/// and waits for the process (RAII guard ensures cleanup).
/// The parent reopens the database, verifies startup replay without relying on shutdown final replay,
/// and validates idempotency via a second replay.
#[tokio::test]
async fn test_crash_subprocess_real_sigkill_recovery() {
    let temp_dir =
        openproxy_db::testing::TempDir::new("op-crash-subprocess").expect("create temp dir");
    let db_path = temp_dir.path().join("crash_test.db");

    // 1. Initialize DB file and migrations
    {
        let pool = DbPool::open(&db_path).expect("open initial db pool");
        let mut writer = pool.writer();
        openproxy_db::migrations::run(&mut writer).expect("run migrations");
    }

    // 2. Spawn child subprocess running `helper_crash_subprocess`
    let current_exe = std::env::current_exe().expect("resolve current_exe");
    let mut cmd = Command::new(current_exe);
    cmd.arg("--exact")
        .arg("helper_crash_subprocess")
        .arg("--nocapture")
        .env(
            "OPENPROXY_CRASH_TEST_DB_PATH",
            db_path.to_str().expect("utf8 path"),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    let child = cmd.spawn().expect("spawn child crash subprocess");
    let mut guard = ChildGuard::new(child);

    // 3. Await stdout marker with bounded timeout; if timeout, cleanup child before unwinding
    let stdout = guard
        .as_mut()
        .expect("child handle")
        .stdout
        .take()
        .expect("stdout piped");

    let mut marker_reader_task = tokio::task::spawn_blocking(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap_or(0) > 0 {
            if line.contains("CRASH_CHILD_COMMITTED_AND_READY") {
                return true;
            }
            line.clear();
        }
        false
    });

    let marker_res =
        tokio::time::timeout(std::time::Duration::from_secs(15), &mut marker_reader_task).await;
    let marker_found = match marker_res {
        Ok(join_res) => join_res.expect("marker reader task panicked"),
        Err(_) => {
            let _ = guard.kill_and_wait();
            let _ = marker_reader_task.await;
            panic!("child process did not emit ready marker within 15s");
        }
    };
    assert!(marker_found, "expected ready marker from child subprocess");

    // 4. Kill child with real SIGKILL (child.kill() sends SIGKILL on Linux/Unix)
    let status = guard.kill_and_wait().expect("kill and wait child");
    assert!(
        !status.success(),
        "child killed with SIGKILL must not exit with success"
    );
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            status.signal(),
            Some(9),
            "child must be killed by SIGKILL (signal 9)"
        );
    }

    // 5. Reopen the database from the same file
    let pool = DbPool::open(&db_path).expect("reopen db pool after crash");
    let conn = pool.writer_arc();

    let count = 128;
    let expected_ids = generate_deterministic_request_ids(count, 20_000);

    // Verify all 128 jobs persisted in SQLite journal despite sudden SIGKILL (in spawn_blocking)
    let depth = tokio::task::spawn_blocking({
        let conn = Arc::clone(&conn);
        move || openproxy_db::usage_journal::depth(&conn.lock()).unwrap()
    })
    .await
    .expect("journal depth after crash");
    assert_eq!(
        depth, count as u64,
        "all 128 committed journal records must survive SIGKILL crash"
    );

    // 6. Spawn worker without sending any wakes
    let (_sender, receiver) = tokio::sync::mpsc::channel(1);
    let worker = spawn_worker(Arc::clone(&conn), receiver);

    // 7. Verify startup replay drains pending jobs BEFORE shutdown is called
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let stats = worker.stats().await.expect("worker stats");
            if stats.pending == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("startup replay drained all 128 pending records before shutdown");

    worker.shutdown().await.expect("worker recovery shutdown");

    // 8. Verify all records persisted to `usage` table with COUNT exactly 1 and pending == 0 in spawn_blocking
    tokio::task::spawn_blocking({
        let conn = Arc::clone(&conn);
        let expected_ids = expected_ids.clone();
        move || verify_usage_records(&conn, &expected_ids)
    })
    .await
    .expect("verify usage records after startup replay");

    // 9. Second replay: verify idempotency (no duplicates) in spawn_blocking
    tokio::task::spawn_blocking({
        let conn = Arc::clone(&conn);
        let expected_ids = expected_ids.clone();
        move || {
            replay_pending(&conn).expect("second replay call");
            verify_usage_records(&conn, &expected_ids);
        }
    })
    .await
    .expect("second replay idempotency check");
}

/// Test 3: Native SQLite DiskFull error handling and journal recovery.
/// Sets `PRAGMA max_page_count` to current page count, attempts admission of an oversized payload,
/// verifies explicit SQLite DiskFull error downcast without corruption or phantom rows, restores `max_page_count`,
/// admits a subsequent job, and verifies replay persists all admitted jobs exactly once.
#[test]
fn test_sqlite_full_native_recovery() {
    let (_pool, conn, _path) = create_test_db("op-sqlite-full");

    // 1. Admit N initial jobs before triggering disk full
    let initial_count = 5;
    let mut admitted_ids = generate_deterministic_request_ids(initial_count, 30_000);
    for (i, req_id) in admitted_ids.iter().enumerate() {
        let job = sample_record_usage_job(*req_id, i as u64);
        admit_job(&conn, &job).expect("admit pre-full job");
    }

    let depth_before = openproxy_db::usage_journal::depth(&conn.lock()).expect("depth before");
    assert_eq!(depth_before, initial_count as u64);

    // 2. Set PRAGMA max_page_count to current page count (no additional pages allowed)
    let current_pages: i64 = conn
        .lock()
        .query_row("PRAGMA page_count", [], |r| r.get(0))
        .expect("query current page_count");
    conn.lock()
        .execute_batch(&format!("PRAGMA max_page_count = {current_pages};"))
        .expect("set max_page_count to limit");

    // 3. Attempt to admit an oversized payload (1MB payload requiring multiple overflow pages)
    let failed_req_id = RequestId(openproxy_types::ids::u64_to_v4_uuid(39_999));
    let oversized_job = sample_large_record_usage_job(failed_req_id, 1024 * 1024);
    let result = admit_job(&conn, &oversized_job);

    // 4. Verify explicit Database error indicating disk full without fake capacity exhaustion
    assert!(
        result.is_err(),
        "admission must fail when SQLite cannot allocate new pages"
    );
    let err = result.expect_err("expected error on disk full");
    assert!(
        !err.is_journal_capacity_exhausted(),
        "error must be a real DB disk full error, not journal capacity exhaustion"
    );

    let mut is_disk_full = false;
    if let CoreError::Database {
        source: Some(ref src),
        ..
    } = err
    {
        if let Some(rusqlite::Error::SqliteFailure(ffi, _)) = src.downcast_ref::<rusqlite::Error>()
        {
            if ffi.code == rusqlite::ErrorCode::DiskFull
                || ffi.code == rusqlite::ErrorCode::SystemIoFailure
            {
                is_disk_full = true;
            }
        }
    }
    assert!(
        is_disk_full,
        "error source must downcast to rusqlite::ErrorCode::DiskFull or SystemIoFailure, actual: {err:?}"
    );

    // 5. Verify no phantom rows and depth remains unchanged
    let depth_after = openproxy_db::usage_journal::depth(&conn.lock()).expect("depth after");
    assert_eq!(
        depth_after, depth_before,
        "journal depth must remain unchanged at {depth_before} after failed admission"
    );

    let phantom_count: i64 = conn
        .lock()
        .query_row(
            "SELECT count(*) FROM usage_journal WHERE payload LIKE ?1",
            [format!("%{failed_req_id}%")],
            |r| r.get(0),
        )
        .expect("query phantom rows");
    assert_eq!(
        phantom_count, 0,
        "failed job must not leave a phantom row in usage_journal"
    );

    // 6. Restore max_page_count to SQLite default max (1073741823) and admit a new valid job
    conn.lock()
        .execute_batch("PRAGMA max_page_count = 1073741823;")
        .expect("restore max_page_count");

    let new_req_id = RequestId(openproxy_types::ids::u64_to_v4_uuid(30_005));
    let new_job = sample_record_usage_job(new_req_id, 5);
    admit_job(&conn, &new_job).expect("admit job after restoring max_page_count");
    admitted_ids.push(new_req_id);

    assert_eq!(
        openproxy_db::usage_journal::depth(&conn.lock()).expect("depth after recovery"),
        (initial_count + 1) as u64
    );

    // 7. Synchronous replay: verify all admitted jobs applied exactly once
    replay_pending(&conn).expect("replay admitted jobs after max_page_count restored");
    verify_usage_records(&conn, &admitted_ids);

    // Failed job must not exist in `usage`
    let failed_in_usage: i64 = conn
        .lock()
        .query_row(
            "SELECT count(*) FROM usage WHERE request_id = ?1",
            [failed_req_id.to_string()],
            |r| r.get(0),
        )
        .expect("query failed job in usage");
    assert_eq!(
        failed_in_usage, 0,
        "failed job must never be persisted in usage table"
    );

    // Second replay: verify idempotency
    replay_pending(&conn).expect("second replay call");
    verify_usage_records(&conn, &admitted_ids);
}

/// Test 4: Injected ACK failure during the apply transaction demonstrates atomic rollback:
/// the usage insertion rolls back, the record is safely retained in `usage_journal`,
/// and upon dropping the trigger, replay succeeds with COUNT exactly 1.
#[test]
fn injected_ack_failure_rolls_back_usage_and_retries() {
    let (_pool, conn, _path) = create_test_db("op-ack-failure");

    // 1. Admit a job into the durable journal
    let apply_req_id = RequestId(openproxy_types::ids::u64_to_v4_uuid(40_001));
    let job = sample_record_usage_job(apply_req_id, 40_001);
    admit_job(&conn, &job).expect("admit job");
    assert_eq!(
        openproxy_db::usage_journal::depth(&conn.lock()).expect("depth"),
        1
    );

    // 2. Install trigger to simulate an atomic ACK failure during acknowledge()
    conn.lock()
        .execute_batch(
            "CREATE TRIGGER fail_ack BEFORE DELETE ON usage_journal \
             BEGIN SELECT RAISE(ABORT, 'injected ACK failure'); END;",
        )
        .expect("install ack failure trigger");

    // 3. Synchronous replay must fail due to trigger
    let replay_result = replay_pending(&conn);
    assert!(
        replay_result.is_err(),
        "replay_pending must fail when acknowledge() is aborted"
    );

    // 4. Verify transaction rollback: no row in `usage`, job preserved in `usage_journal`
    let usage_count: i64 = conn
        .lock()
        .query_row(
            "SELECT count(*) FROM usage WHERE request_id = ?1",
            [apply_req_id.to_string()],
            |r| r.get(0),
        )
        .expect("count usage");
    assert_eq!(
        usage_count, 0,
        "atomic rollback must ensure no row in usage table"
    );

    let depth = openproxy_db::usage_journal::depth(&conn.lock()).expect("journal depth");
    assert_eq!(
        depth, 1,
        "job must remain safely retained in journal on ACK failure"
    );

    // 5. Repair: drop failure trigger
    conn.lock()
        .execute_batch("DROP TRIGGER fail_ack;")
        .expect("drop ack failure trigger");

    // 6. Replay succeeds and persists record
    replay_pending(&conn).expect("replay succeeds after dropping trigger");
    verify_usage_records(&conn, &[apply_req_id]);

    // Second replay: verify idempotency
    replay_pending(&conn).expect("second replay call");
    verify_usage_records(&conn, &[apply_req_id]);
}

/// Test 5: Real OS ENOSPC admission failure and recovery on a dedicated tmpfs filesystem.
///
/// Activated only when `OPENPROXY_ENOSPC_TEST_ROOT` environment variable is defined.
/// Performs safety checks against `/proc/mounts` to guarantee the path is a `tmpfs`
/// mount within `/tmp/opencode/`, writes a dedicated filler file until `raw_os_error == 28`,
/// verifies real SQLite Database error on admission, removes the filler, and verifies recovery.
#[test]
#[ignore = "requires a dedicated <=64 MiB tmpfs in a private mount namespace"]
fn test_os_enospc_admission_and_recovery() {
    let root_str =
        std::env::var("OPENPROXY_ENOSPC_TEST_ROOT").expect("dedicated tmpfs path required");

    assert!(
        root_str.starts_with("/tmp/opencode/"),
        "OPENPROXY_ENOSPC_TEST_ROOT must reside within /tmp/opencode/ for safety"
    );

    let root_path = PathBuf::from(&root_str);
    assert!(
        root_path.exists(),
        "OPENPROXY_ENOSPC_TEST_ROOT path must exist: {root_str}"
    );
    assert!(
        verify_path_is_on_tmpfs(&root_path),
        "OPENPROXY_ENOSPC_TEST_ROOT must reside on a tmpfs filesystem mount to prevent host disk impact"
    );

    let test_dir = root_path.join(format!("enospc-test-{}", RequestId::new()));
    std::fs::create_dir(&test_dir).expect("create isolated enospc test dir");

    let db_path = test_dir.join("test_enospc.db");
    let pool = DbPool::open(&db_path).expect("open db pool on dedicated fs");
    let conn = pool.writer_arc();
    {
        let mut w = pool.writer();
        openproxy_db::migrations::run(&mut w).expect("run migrations");
    }

    // 1. Admit 5 initial jobs
    let initial_count = 5;
    let mut admitted_ids = generate_deterministic_request_ids(initial_count, 50_000);
    for (i, req_id) in admitted_ids.iter().enumerate() {
        let job = sample_record_usage_job(*req_id, i as u64);
        admit_job(&conn, &job).expect("admit pre-enospc job");
    }
    let depth_before = openproxy_db::usage_journal::depth(&conn.lock()).expect("depth");
    assert_eq!(depth_before, initial_count as u64);

    // 2. Fill the tmpfs filesystem using a dedicated filler file until real OS ENOSPC (errno 28)
    let filler_path = test_dir.join("filler_space.bin");
    let mut filler = std::fs::File::create(&filler_path).expect("create filler file");
    let chunk = vec![0x55u8; 64 * 1024]; // 64 KiB
    let mut total_written = 0usize;
    let max_defensive_bytes = 40 * 1024 * 1024; // 40 MiB ceiling
    let mut hit_enospc = false;

    while total_written < max_defensive_bytes {
        match filler.write_all(&chunk) {
            Ok(()) => {
                total_written += chunk.len();
            }
            Err(e) => {
                if e.raw_os_error() == Some(28) || e.kind() == std::io::ErrorKind::StorageFull {
                    hit_enospc = true;
                    break;
                }
                panic!("unexpected IO error while filling tmpfs: {e}");
            }
        }
    }
    drop(filler);
    assert!(
        hit_enospc,
        "filler file must encounter real OS ENOSPC (raw_os_error 28)"
    );

    // 3. Admission of oversized payload must fail with real database IO / disk full error
    let failed_req_id = RequestId(openproxy_types::ids::u64_to_v4_uuid(59_999));
    let large_job = sample_large_record_usage_job(failed_req_id, 256 * 1024);
    let result = admit_job(&conn, &large_job);

    assert!(result.is_err(), "admission must fail under real OS ENOSPC");
    let err = result.expect_err("expected error on enospc");
    assert!(
        !err.is_journal_capacity_exhausted(),
        "must not be fake journal capacity error"
    );

    let mut is_disk_full_or_io = false;
    if let CoreError::Database {
        source: Some(ref src),
        ..
    } = err
    {
        if let Some(rusqlite::Error::SqliteFailure(ffi, _)) = src.downcast_ref::<rusqlite::Error>()
        {
            is_disk_full_or_io = matches!(
                ffi.code,
                rusqlite::ErrorCode::DiskFull | rusqlite::ErrorCode::SystemIoFailure
            );
        }
    }
    assert!(
        is_disk_full_or_io,
        "error must indicate SQLite DiskFull or SystemIoFailure, got: {err:?}"
    );

    let depth_after = openproxy_db::usage_journal::depth(&conn.lock()).expect("depth");
    assert_eq!(
        depth_after, depth_before,
        "depth must not increment on failed admission"
    );

    // 4. Remove ONLY own filler file to restore disk space
    std::fs::remove_file(&filler_path).expect("remove own filler file");

    // 5. Admit a new job after space recovery
    let post_recovery_id = RequestId(openproxy_types::ids::u64_to_v4_uuid(50_005));
    let post_job = sample_record_usage_job(post_recovery_id, 5);
    admit_job(&conn, &post_job).expect("admit succeeds after removing filler file");
    admitted_ids.push(post_recovery_id);

    // 6. Synchronous replay persists all admitted jobs exactly once
    replay_pending(&conn).expect("replay after enospc recovery");
    verify_usage_records(&conn, &admitted_ids);

    // 7. Reopen DB and second replay to verify idempotency
    drop(conn);
    drop(pool);

    let pool = DbPool::open(&db_path).expect("reopen db pool");
    let conn = pool.writer_arc();
    replay_pending(&conn).expect("second replay after reopen");
    verify_usage_records(&conn, &admitted_ids);

    // 8. Cleanup test directory
    let _ = std::fs::remove_dir_all(&test_dir);
}
