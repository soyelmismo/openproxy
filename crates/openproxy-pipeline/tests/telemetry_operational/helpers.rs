//! Operational test helpers for `openproxy-pipeline` journal telemetry testing.
//!
//! Provides isolated database instantiation, sample payload construction,
//! deterministic identifier generation, verification routines, mount safety validations,
//! and RAII subprocess lifecycle guards.

use openproxy_db::conn::DbPool;
use openproxy_pipeline::worker::BackgroundJob;
use openproxy_types::endpoint::EndpointKind;
use openproxy_types::ids::{ProviderId, RequestId, u64_to_v4_uuid};
use openproxy_types::usage::{USAGE_FLAG_CLIENT_RESPONSE, USAGE_FLAG_STREAM_COMPLETE, UsageInput};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::Arc;

/// RAII Guard for managing spawned child processes.
///
/// Ensures `kill()` and `wait()` are ALWAYS executed upon dropping,
/// preventing zombie child processes or leaked process sessions.
pub struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    pub fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    pub fn as_mut(&mut self) -> Option<&mut Child> {
        self.child.as_mut()
    }

    /// Kills the child process and waits for it without taking the handle beforehand.
    /// The inner child is only removed if `wait()` successfully completes.
    pub fn kill_and_wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        let child = self
            .child
            .as_mut()
            .ok_or_else(|| std::io::Error::other("child already reaped"))?;
        child.kill()?;
        let status = child.wait()?;
        self.child = None;
        Ok(status)
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Creates a fresh isolated test database in a temporary directory
/// with migrations applied and WAL `PRAGMA synchronous = FULL`.
pub fn create_test_db(prefix: &str) -> (DbPool, Arc<parking_lot::Mutex<Connection>>, PathBuf) {
    let pool = DbPool::test_pool_with_prefix(prefix).expect("create test db pool");
    let conn = pool.writer_arc();
    let path = pool.path().to_path_buf();
    (pool, conn, path)
}

/// Constructs a realistic `UsageInput` instance with unique identifiers.
pub fn sample_usage_input(request_id: RequestId, seed: u64) -> UsageInput {
    UsageInput {
        request_id,
        trace_id: format!("trace-{seed}"),
        attempt: 1,
        provider_id: ProviderId::new("openai"),
        account_id: None,
        combo_id: None,
        combo_target_id: None,
        model_row_id: None,
        upstream_model_id: "gpt-4o-mini".to_string(),
        prompt_tokens: Some(100 + (seed % 50) as u32),
        completion_tokens: Some(20 + (seed % 30) as u32),
        cached_tokens: Some(5),
        connect_ms: Some(15),
        ttft_ms: Some(60),
        total_ms: 180 + (seed % 100),
        status_code: 200,
        error_msg: None,
        race_total: 1,
        api_key_id: None,
        request_body_json: None,
        response_body_json: None,
        request_headers: None,
        response_headers: None,
        error_message: None,
        race_attempts: 1,
        stop_reason: Some("stop".to_string()),
        compression_savings_pct: None,
        compression_techniques: None,
        pii_redacted: None,
        endpoint_kind: EndpointKind::Chat,
        proxy_url: None,
        proxy_status: None,
        flags: USAGE_FLAG_CLIENT_RESPONSE | USAGE_FLAG_STREAM_COMPLETE,
    }
}

/// Creates a standard `BackgroundJob::RecordUsage` job with unique `RequestId`.
pub fn sample_record_usage_job(request_id: RequestId, seed: u64) -> BackgroundJob {
    BackgroundJob::RecordUsage(Box::new(sample_usage_input(request_id, seed)))
}

/// Creates a `BackgroundJob::RecordUsage` with an oversized `request_body_json` payload
/// to trigger SQLite page allocations (for native `SQLITE_FULL` and OS `ENOSPC` testing).
pub fn sample_large_record_usage_job(request_id: RequestId, payload_bytes: usize) -> BackgroundJob {
    let mut input = sample_usage_input(request_id, 9999);
    input.request_body_json = Some(bytes::Bytes::from(vec![b'x'; payload_bytes]));
    BackgroundJob::RecordUsage(Box::new(input))
}

/// Generates `n` deterministic `RequestId`s starting from `offset`.
pub fn generate_deterministic_request_ids(count: usize, offset: u64) -> Vec<RequestId> {
    (0..count)
        .map(|i| RequestId(u64_to_v4_uuid(offset + i as u64)))
        .collect()
}

/// Verifies that:
/// 1. `usage_journal` depth is 0 (all pending jobs acknowledged).
/// 2. Total rows in `usage` table matches `expected_ids.len()`.
/// 3. Every individual `request_id` has COUNT exactly 1 (no duplicates, no omissions).
pub fn verify_usage_records(conn: &parking_lot::Mutex<Connection>, expected_ids: &[RequestId]) {
    let db = conn.lock();
    let pending = openproxy_db::usage_journal::depth(&db).expect("get journal depth");
    assert_eq!(pending, 0, "usage journal depth must be 0 (got {pending})");

    let total_in_usage: i64 = db
        .query_row("SELECT count(*) FROM usage", [], |r| r.get(0))
        .expect("count usage rows");
    assert_eq!(
        total_in_usage as usize,
        expected_ids.len(),
        "total rows in usage ({total_in_usage}) must equal expected count ({})",
        expected_ids.len()
    );

    let mut stmt = db
        .prepare("SELECT count(*) FROM usage WHERE request_id = ?1")
        .expect("prepare count stmt");
    for req_id in expected_ids {
        let count: i64 = stmt
            .query_row([req_id.to_string()], |r| r.get(0))
            .expect("query row count for request_id");
        assert_eq!(
            count, 1,
            "request_id {req_id} must have COUNT exactly 1 in usage table, got {count}"
        );
    }
}

/// Validates that a path resides on a `tmpfs` filesystem mount point according to `/proc/mounts`.
/// Prevents accidental filling of host or non-tmpfs physical partitions during OS ENOSPC tests.
pub fn verify_path_is_on_tmpfs(path: &Path) -> bool {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let Ok(content) = std::fs::read_to_string("/proc/mounts") else {
        return false;
    };
    let mut best_match: Option<(PathBuf, String)> = None;
    for line in content.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 3 {
            let mount_point = PathBuf::from(parts[1]);
            let fstype = parts[2].to_string();
            if canonical.starts_with(&mount_point) {
                match &best_match {
                    None => best_match = Some((mount_point, fstype)),
                    Some((prev_point, _)) => {
                        if mount_point.as_os_str().len() > prev_point.as_os_str().len() {
                            best_match = Some((mount_point, fstype));
                        }
                    }
                }
            }
        }
    }
    best_match.is_some_and(|(_, fstype)| fstype == "tmpfs")
}
