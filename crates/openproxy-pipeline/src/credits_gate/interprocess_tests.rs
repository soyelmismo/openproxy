//! Cross-process regression tests for the prepaid-credit gate.
//!
//! Every test here exercises the parts the in-process lane cannot cover: the
//! per-key file lock next to the database, its release on cancellation and on a
//! real process crash (`SIGKILL`), and the durable intent row that survives the
//! process and is reconciled from `usage` evidence by the next admission.
//!
//! All waits are bounded: a regression in the lock or in the recovery path must
//! fail here, never hang the suite.

use super::CreditsAdmissionGate;
use super::interprocess;
use openproxy_types::ids::{ApiKeyId, RequestId};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

const CHILD_ENV: &str = "OPENPROXY_CREDIT_LOCK_CHILD_TEST";
const CHILD_TEST: &str = "credits_gate::interprocess_tests::child_process_holds_the_key_lock";

fn temp_db() -> (openproxy_db::testing::TempDir, PathBuf) {
    let dir = openproxy_db::testing::TempDir::new("credit-gate-lock").expect("temp dir");
    let path = dir.path().join("openproxy.db");
    // An empty file is enough for the lock path: real callers always have the
    // database file open before they ever admit a request.
    std::fs::File::create(&path).expect("database file");
    (dir, path)
}

/// A migrated pool with one prepaid key `1` holding `grant` micro-USDC units.
fn credit_pool(grant: i64) -> openproxy_db::DbPool {
    let pool = openproxy_db::DbPool::test_pool().expect("pool");
    {
        let conn = pool.writer();
        conn.execute(
            "INSERT INTO api_keys(key_hash, label) VALUES ('credit-gate-lock', 'lock fixture')",
            [],
        )
        .expect("api key");
        conn.execute(
            "INSERT INTO credit_transactions(api_key_id, kind, amount_units, remaining_units) \
             VALUES (1, 'grant', ?1, ?1)",
            [grant],
        )
        .expect("grant");
    }
    pool
}

fn balance(pool: &openproxy_db::DbPool) -> i64 {
    let conn = pool.writer();
    openproxy_db::credits_ledger::balance_units(&conn, 1).expect("balance")
}

fn intent_row(
    pool: &openproxy_db::DbPool,
    request_id: &str,
) -> Option<(String, Option<String>, i64)> {
    use rusqlite::OptionalExtension;
    let conn = pool.writer();
    conn.query_row(
        "SELECT state, outcome, charged_units FROM credit_reservations \
         WHERE request_id = ?1 ORDER BY id DESC LIMIT 1",
        [request_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .optional()
    .expect("intent row")
}

/// Whether this exact request still has an open intent. A global count would
/// also see the intent the admission under test just created for itself.
fn held(pool: &openproxy_db::DbPool, request_id: &str) -> bool {
    intent_row(pool, request_id).map(|row| row.0) == Some("held".into())
}

fn seed_delivered_usage(pool: &openproxy_db::DbPool, request_id: &str, cost_usd: f64) {
    let conn = pool.writer();
    conn.execute(
        "INSERT INTO usage(request_id, trace_id, provider_id, upstream_model_id, attempt, \
         combo_target_id, race_total, race_lost, total_ms, status_code, cost_usd, api_key_id) \
         VALUES (?1, 'lock-fixture', 'fixture', 'fixture', 1, 1, 1, 0, 1, 200, ?2, 1)",
        rusqlite::params![request_id, cost_usd],
    )
    .expect("usage row");
}

fn usage_debited(pool: &openproxy_db::DbPool, request_id: &str) -> Option<i64> {
    use rusqlite::OptionalExtension;
    let conn = pool.writer();
    conn.query_row(
        "SELECT credits_debited FROM usage WHERE request_id = ?1 ORDER BY id DESC LIMIT 1",
        [request_id],
        |row| row.get(0),
    )
    .optional()
    .expect("usage lookup")
}

// ── the file lock ───────────────────────────────────────────────────

#[tokio::test]
async fn a_second_owner_of_the_same_key_waits_for_the_file_lock() {
    let (_dir, path) = temp_db();
    let gate = Arc::new(CreditsAdmissionGate::new());
    // Different `db_id`s model different processes: they share no lane map.
    let first = gate.acquire_leased(1, &path, 7).await.expect("first turn");
    assert!(
        first.has_interprocess_lease(),
        "admission must hold the cross-process lock, not only the lane"
    );

    let waiter_gate = Arc::clone(&gate);
    let waiter_path = path.clone();
    let waiter = tokio::spawn(async move {
        waiter_gate
            .acquire_leased(2, &waiter_path, 7)
            .await
            .map(|guard| guard.key_id())
    });

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !waiter.is_finished(),
        "another process must not take the same key while the lock is held"
    );

    drop(first);
    let acquired = timeout(Duration::from_secs(5), waiter)
        .await
        .expect("the released file lock must wake the waiter")
        .expect("waiter task")
        .expect("second turn");
    assert_eq!(acquired, 7);
}

#[tokio::test]
async fn different_keys_never_wait_on_each_other_across_owners() {
    let (_dir, path) = temp_db();
    let gate = Arc::new(CreditsAdmissionGate::new());
    let one = gate.acquire_leased(1, &path, 1).await.expect("key 1");
    let other = timeout(Duration::from_secs(2), gate.acquire_leased(2, &path, 2))
        .await
        .expect("a different key must not wait for another key's lock")
        .expect("key 2");
    drop(one);
    drop(other);
}

#[tokio::test]
async fn a_cancelled_waiter_does_not_leave_the_lock_held() {
    let (_dir, path) = temp_db();
    let gate = Arc::new(CreditsAdmissionGate::new());
    let first = gate.acquire_leased(1, &path, 9).await.expect("first turn");

    let waiter_gate = Arc::clone(&gate);
    let waiter_path = path.clone();
    let waiter = tokio::spawn(async move { waiter_gate.acquire_leased(2, &waiter_path, 9).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    waiter.abort();
    match waiter.await {
        Err(error) => assert!(error.is_cancelled(), "the waiter was aborted, not dropped"),
        Ok(_) => panic!("the waiter must not have acquired the lock"),
    }

    drop(first);
    // If the aborted wait had left an orphaned blocking task holding the file
    // lock, this would time out.
    let regained = timeout(Duration::from_secs(5), gate.acquire_leased(2, &path, 9))
        .await
        .expect("a cancelled waiter must not keep the key locked")
        .expect("regained");
    drop(regained);
}

#[tokio::test]
async fn the_lock_file_lives_next_to_the_database() {
    let (_dir, path) = temp_db();
    openproxy_db::DbPool::open(&path).expect("create database file");
    let lease = interprocess::acquire_blocking_for_test(&path, 42).expect("take the lock");
    let lock = lease.path().to_path_buf();
    let canonical_db = path.canonicalize().expect("canonical db");
    assert_eq!(
        lock.parent().and_then(Path::parent),
        canonical_db.parent(),
        "the lock directory must sit beside the database file"
    );
    assert_eq!(
        lock.file_name().and_then(|name| name.to_str()),
        Some("key-42.lock")
    );
    assert!(lock.exists(), "the lock file must be stable on disk");
    assert_eq!(
        interprocess::lock_file_path_for_test(&path, 42).expect("path again"),
        lock,
        "the path is derived deterministically from the database file"
    );
    drop(lease);
}

/// Child process body: grabs the key lock, reports readiness, then holds it
/// until the parent kills the process.
#[test]
fn child_process_holds_the_key_lock() {
    let Some(spec) = std::env::var_os(CHILD_ENV) else {
        return;
    };
    let spec = spec.to_string_lossy().to_string();
    let mut parts = spec.split('|');
    let db_path = PathBuf::from(parts.next().expect("db path"));
    let key: i64 = parts.next().expect("key").parse().expect("key number");
    let ready = PathBuf::from(parts.next().expect("ready path"));

    let lease = interprocess::acquire_blocking_for_test(&db_path, key).expect("child lock");
    std::fs::write(&ready, lease.path().to_string_lossy().as_bytes()).expect("ready marker");
    println!("LOCKED {}", lease.path().display());
    // Hold until the parent kills us; a real crash is the point of the test.
    std::thread::sleep(Duration::from_secs(30));
}

#[tokio::test]
async fn a_crashed_process_releases_its_key_lock() {
    let (_dir, path) = temp_db();
    openproxy_db::DbPool::open(&path).expect("create database file");
    let ready = path.with_extension("child-ready");
    let _ = std::fs::remove_file(&ready);

    let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", CHILD_TEST, "--nocapture", "--test-threads=1"])
        .env(
            CHILD_ENV,
            format!("{}|11|{}", path.display(), ready.display()),
        )
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn child process");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !std::fs::exists(&ready).unwrap_or(false) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the child process never acquired the key lock"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let gate = CreditsAdmissionGate::new();
    let blocked = timeout(
        Duration::from_millis(500),
        gate.acquire_leased(2, &path, 11),
    )
    .await;
    assert!(
        blocked.is_err(),
        "a live child process must keep this key locked"
    );

    child.kill().expect("kill child");
    child.wait().expect("reap child");

    let acquired = timeout(Duration::from_secs(10), gate.acquire_leased(2, &path, 11))
        .await
        .expect("the OS must release the lock when the process dies")
        .expect("acquire after crash");
    drop(acquired);
}

// ── durable intent ──────────────────────────────────────────────────

#[tokio::test]
async fn a_crashed_delivered_request_is_billed_once_by_the_next_admission() {
    let pool = credit_pool(1_000);
    seed_delivered_usage(&pool, "crashed-delivered", 0.0004);
    {
        let conn = pool.writer();
        openproxy_db::reservations::hold(&conn, 1, "crashed-delivered", 1, 1_000, false)
            .expect("crashed intent");
    }
    assert_eq!(
        intent_row(&pool, "crashed-delivered").map(|row| row.0),
        Some("held".into())
    );

    let guard = super::admit_unary_credit(&pool, Some(ApiKeyId(1)))
        .await
        .expect("admission")
        .expect("credit key is governed");

    assert_eq!(
        usage_debited(&pool, "crashed-delivered"),
        Some(1),
        "the delivered response must be debited by the recovery, not lost"
    );
    assert_eq!(
        balance(&pool),
        600,
        "only the real cost of the delivered response is charged"
    );
    let (state, outcome, charged) = intent_row(&pool, "crashed-delivered").expect("intent row");
    assert_eq!(state, "settled");
    assert_eq!(outcome.as_deref(), Some("recovered_billed"));
    assert_eq!(charged, 400);
    assert!(
        !held(&pool, "crashed-delivered"),
        "the recovered intent is closed"
    );

    // The second admission does not charge the same usage row again.
    drop(guard);
    let again = super::admit_unary_credit(&pool, Some(ApiKeyId(1)))
        .await
        .expect("second admission")
        .expect("still governed");
    assert_eq!(balance(&pool), 600);
    drop(again);
}

#[tokio::test]
async fn a_crashed_intent_without_delivery_is_released_not_charged() {
    let pool = credit_pool(1_000);
    {
        let conn = pool.writer();
        openproxy_db::reservations::hold(&conn, 1, "crashed-undelivered", 1, 1_000, false)
            .expect("crashed intent");
    }

    let guard = super::admit_unary_credit(&pool, Some(ApiKeyId(1)))
        .await
        .expect("admission")
        .expect("credit key is governed");
    assert_eq!(
        guard.budget_units(),
        1_000,
        "the whole balance is the budget"
    );
    assert_eq!(
        balance(&pool),
        1_000,
        "nothing was delivered, so nothing may be charged"
    );
    let (state, outcome, charged) = intent_row(&pool, "crashed-undelivered").expect("intent row");
    assert_eq!(state, "released");
    assert_eq!(outcome.as_deref(), Some("recovered_unbilled"));
    assert_eq!(charged, 0);
    assert!(
        !held(&pool, "crashed-undelivered"),
        "the key is not blocked by a stale intent"
    );
}

#[tokio::test]
async fn a_started_stream_is_charged_only_for_what_was_checkpointed() {
    // A worker that checkpointed output before delivering it leaves verifiable
    // evidence: exactly that amount is billed, never the admitted maximum.
    let checked = credit_pool(1_000);
    {
        let conn = checked.writer();
        openproxy_db::reservations::hold(&conn, 1, "stream-checked", 1, 900, true)
            .expect("quoted intent");
        openproxy_db::reservations::checkpoint(&conn, 1, "stream-checked", 1, 300)
            .expect("durable checkpoint");
    }
    let guard = super::admit_unary_credit(&checked, Some(ApiKeyId(1)))
        .await
        .expect("admission")
        .expect("governed");
    assert_eq!(
        balance(&checked),
        700,
        "only the checkpointed output is charged"
    );
    let (state, outcome, charged) = intent_row(&checked, "stream-checked").expect("intent row");
    assert_eq!(state, "settled");
    assert_eq!(outcome.as_deref(), Some("recovered_partial"));
    assert_eq!(charged, 300);
    drop(guard);

    // A stream that started but produced no verifiable output is not billable:
    // the admitted/quote maximum is never charged on its own.
    let unchecked = credit_pool(1_000);
    {
        let conn = unchecked.writer();
        openproxy_db::reservations::hold(&conn, 1, "stream-unchecked", 1, 900, true)
            .expect("quoted intent");
        openproxy_db::reservations::mark_delivery_started(&conn, "stream-unchecked", 1)
            .expect("delivery flag");
    }
    let guard = super::admit_unary_credit(&unchecked, Some(ApiKeyId(1)))
        .await
        .expect("admission")
        .expect("governed");
    assert_eq!(
        balance(&unchecked),
        1_000,
        "no checkpoint means no evidence of produced output, so nothing is invented"
    );
    let (state, outcome, charged) = intent_row(&unchecked, "stream-unchecked").expect("intent row");
    assert_eq!(state, "released");
    assert_eq!(outcome.as_deref(), Some("recovered_unbilled"));
    assert_eq!(charged, 0);
    drop(guard);
}

#[tokio::test]
async fn admission_records_the_intent_before_handing_the_guard_out() {
    let pool = credit_pool(1_000);
    let request_id = RequestId::new();
    let guard =
        super::admit_unary_credit_with_request(&pool, Some(ApiKeyId(1)), request_id, Some(250))
            .await
            .expect("admission")
            .expect("governed");

    assert_eq!(
        guard.budget_units(),
        250,
        "an explicit quote becomes the dispatch budget"
    );
    let reservation = guard.reservation_id().expect("durable intent id");
    {
        let conn = pool.writer();
        let state: String = conn
            .query_row(
                "SELECT state FROM credit_reservations WHERE id = ?1",
                [reservation],
                |row| row.get(0),
            )
            .expect("intent state");
        assert_eq!(
            state, "held",
            "the intent is durable and open before the caller can dispatch"
        );
    }

    // Dropping without settling (a cancelled or crashed request) leaves the
    // intent behind: that is the durable evidence the next admission resolves.
    drop(guard);
    assert!(held(&pool, &request_id.to_string()));
    let next = super::admit_unary_credit(&pool, Some(ApiKeyId(1)))
        .await
        .expect("next admission")
        .expect("governed");
    assert!(
        !held(&pool, &request_id.to_string()),
        "the abandoned intent is recovered"
    );
    assert_eq!(balance(&pool), 1_000, "an abandoned request bills nothing");
    assert_eq!(next.budget_units(), 1_000);
}
