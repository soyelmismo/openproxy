//! Tests for the in-memory adapter registry hot-reload path.
//!
//! Regression coverage for the frozen-registry bug: the list used to be built
//! once at startup, so a `POST /admin/providers` on a running server inserted the
//! row but left the registry stale and the first chat attempt failed with
//! `CoreError::ProviderNotFound`. The registry is now an `Arc<RwLock<Vec<…>>>`
//! with a `rebuild_adapters()` the admin handlers call after mutations.

use super::*;
use crate::state::AppState;
use openproxy_adapters::adapters;
use openproxy_core::{AppConfig, providers};
use openproxy_db as core_db;
use openproxy_db::MasterKey;
use openproxy_pipeline::worker::BackgroundJob;
use openproxy_types::endpoint::EndpointKind;
use openproxy_types::ids::{ProviderId, RequestId};
use openproxy_types::usage::UsageInput;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Build an in-process pool: temp dir on disk, migrations applied.
fn fresh_pool() -> (core_db::DbPool, PathBuf) {
    let pool = core_db::DbPool::test_pool_with_prefix("openproxy-state-test").expect("open pool");
    let path = pool.path().to_path_buf();
    (pool, path)
}

/// Build a minimal `AppState` mirroring the test helper in
/// `crates/openproxy-server/src/handlers/admin.rs::tests::make_state_with_key`.
async fn make_state() -> AppState {
    let (pool, _path) = fresh_pool();
    let db_pool = Arc::new(pool);
    // Fresh 32-byte key: safe for tests that decrypt no real secrets.
    let master_key = Arc::new(MasterKey::generate().unwrap());
    // Empty registry: `rebuild_adapters` fills in built-ins and custom rows.
    let adapters = Arc::new(RwLock::new(Arc::new(
        Vec::<adapters::ProviderAdapterEnum>::new(),
    )));
    AppState::for_test(AppConfig::default(), db_pool, master_key, adapters)
}

/// Regression test for the frozen-registry bug.
#[tokio::test]
async fn rebuild_adapters_registers_custom_provider() {
    let state = make_state().await;

    // 1. Empty DB → registry should contain only built-ins.
    state.rebuild_adapters().await.expect("first rebuild");
    let initial_ids: Vec<String> = state
        .adapters()
        .iter()
        .map(|a| a.id().as_str().to_string())
        .collect();
    assert!(
        initial_ids.iter().any(|id| id == "openrouter"),
        "openrouter built-in must be present after first rebuild: {initial_ids:?}"
    );

    // 2. Insert a custom provider via the same helper the admin handler uses.
    let custom_id = ProviderId::new("hot-reload-test");
    {
        let w = state.db_pool().writer();
        providers::create(
            &w,
            providers::NewProvider {
                id: &custom_id,
                name: "Hot Reload Test",
                base_url: "https://example.test/v1",
                auth_type: providers::AuthType::Bearer,
                format: providers::ProviderFormat::Openai,
                extra_headers_json: None,
                auto_activate_keyword: None,
                rate_limit_scope: providers::RateLimitScope::Account,
            },
        )
        .expect("create custom provider");
    }

    // 3. The registry must NOT know about it yet.
    let pre_reload_ids: Vec<String> = state
        .adapters()
        .iter()
        .map(|a| a.id().as_str().to_string())
        .collect();
    assert!(
        !pre_reload_ids.iter().any(|id| id == "hot-reload-test"),
        "registry must NOT contain the new provider before rebuild: {pre_reload_ids:?}"
    );

    // 4. Hot-reload.
    state.rebuild_adapters().await.expect("second rebuild");
    let post_reload_ids: Vec<String> = state
        .adapters()
        .iter()
        .map(|a| a.id().as_str().to_string())
        .collect();
    assert!(
        post_reload_ids.iter().any(|id| id == "hot-reload-test"),
        "registry MUST contain the new provider after rebuild: {post_reload_ids:?}"
    );
    assert!(
        post_reload_ids.iter().any(|id| id == "openrouter"),
        "openrouter built-in must remain after rebuild: {post_reload_ids:?}"
    );
}

/// Companion: deleting a custom provider drops its `CustomAdapter` on the next
/// rebuild.
#[tokio::test]
async fn rebuild_adapters_unregisters_deleted_custom_provider() {
    let state = make_state().await;

    // Seed a custom provider and rebuild.
    let custom_id = ProviderId::new("will-be-deleted");
    {
        let w = state.db_pool().writer();
        providers::create(
            &w,
            providers::NewProvider {
                id: &custom_id,
                name: "Will Be Deleted",
                base_url: "https://example.test/v1",
                auth_type: providers::AuthType::Bearer,
                format: providers::ProviderFormat::Openai,
                extra_headers_json: None,
                auto_activate_keyword: None,
                rate_limit_scope: providers::RateLimitScope::Account,
            },
        )
        .expect("create custom provider");
    }
    state
        .rebuild_adapters()
        .await
        .expect("rebuild after create");
    let ids_after_create: Vec<String> = state
        .adapters()
        .iter()
        .map(|a| a.id().as_str().to_string())
        .collect();
    assert!(
        ids_after_create.iter().any(|id| id == "will-be-deleted"),
        "sanity: id present after create+rebuild"
    );

    // Delete the row and rebuild.
    {
        let w = state.db_pool().writer();
        providers::delete(&w, &custom_id).expect("delete custom provider");
    }
    state
        .rebuild_adapters()
        .await
        .expect("rebuild after delete");
    let ids_after_delete: Vec<String> = state
        .adapters()
        .iter()
        .map(|a| a.id().as_str().to_string())
        .collect();
    assert!(
        !ids_after_delete.iter().any(|id| id == "will-be-deleted"),
        "deleted provider must be gone from registry after rebuild: {ids_after_delete:?}"
    );
    assert!(
        ids_after_delete.iter().any(|id| id == "openrouter"),
        "built-ins must survive a rebuild that removed a custom adapter"
    );
}

fn dummy_key(id: i64, hash: &str) -> openproxy_core::api_keys::ApiKey {
    openproxy_core::api_keys::ApiKey {
        id: openproxy_types::ApiKeyId(id),
        key_hash: hash.to_string(),
        key_prefix: Some("op_live_test".into()),
        label: Some("test".into()),
        scopes: vec!["chat".into()],
        allowed_models: None,
        allowed_combos: None,
        blacklisted_providers: None,
        blacklisted_models: None,
        is_active: true,
        revoked_at: None,
        expires_at: None,
        last_used_at: None,
        created_at: "2024-01-01".into(),
        created_by: None,
    }
}

#[tokio::test]
async fn test_api_key_cache_saturation_6000_keys() {
    let state = make_state().await;

    // Insert 6,000 distinct API keys
    for i in 0..6000 {
        let key = Arc::new(dummy_key(i, &format!("key_hash_{i}")));
        state.cache_api_key(key);
        assert!(
            state.api_key_cache.len() <= AppState::MAX_API_KEY_CACHE_ENTRIES,
            "api_key_cache capacity exceeded at {i}: len is {}",
            state.api_key_cache.len()
        );
    }

    assert_eq!(
        state.api_key_cache.len(),
        AppState::MAX_API_KEY_CACHE_ENTRIES
    );

    // Oldest 1,000 (key_hash_0..999) evicted.
    assert!(state.get_cached_api_key("key_hash_0").is_none());
    assert!(state.get_cached_api_key("key_hash_500").is_none());
    assert!(state.get_cached_api_key("key_hash_999").is_none());

    // Newest 5,000 (key_hash_1000..5999) retained.
    assert!(state.get_cached_api_key("key_hash_1000").is_some());
    assert!(state.get_cached_api_key("key_hash_3000").is_some());
    assert!(state.get_cached_api_key("key_hash_5999").is_some());
}

#[tokio::test]
async fn test_api_key_cache_eviction_order_expired_first_oldest_next() {
    let state = make_state().await;

    // Fill to capacity (5,000 keys)
    for i in 0..AppState::MAX_API_KEY_CACHE_ENTRIES {
        let key = Arc::new(dummy_key(i as i64, &format!("key_hash_{i}")));
        state.cache_api_key(key);
    }
    assert_eq!(
        state.api_key_cache.len(),
        AppState::MAX_API_KEY_CACHE_ENTRIES
    );

    // Artificially expire key_hash_2500 by setting its expiration into the past
    let now = std::time::Instant::now();
    if let Some(mut entry) = state.api_key_cache.get_mut("key_hash_2500") {
        entry.1 = now.checked_sub(std::time::Duration::from_secs(10)).unwrap();
    }

    // Insert key_hash_5000: the capacity check prunes first…
    let new_key = Arc::new(dummy_key(5000, "key_hash_5000"));
    state.cache_api_key(new_key);

    assert_eq!(
        state.api_key_cache.len(),
        AppState::MAX_API_KEY_CACHE_ENTRIES
    );

    // …key_hash_2500 was expired, so it is evicted first.
    assert!(
        !state.api_key_cache.contains_key("key_hash_2500"),
        "expired key_hash_2500 must be evicted first"
    );

    // key_hash_0 (oldest non-expired) must still be present.
    assert!(
        state.api_key_cache.contains_key("key_hash_0"),
        "oldest non-expired key_hash_0 must NOT be evicted when an expired key exists"
    );

    // key_hash_5001 with nothing expired: the oldest key is evicted.
    let next_key = Arc::new(dummy_key(5001, "key_hash_5001"));
    state.cache_api_key(next_key);

    assert_eq!(
        state.api_key_cache.len(),
        AppState::MAX_API_KEY_CACHE_ENTRIES
    );
    assert!(
        !state.api_key_cache.contains_key("key_hash_0"),
        "oldest key_hash_0 must now be evicted upon capacity saturation"
    );
    assert!(
        state.api_key_cache.contains_key("key_hash_1"),
        "key_hash_1 should be retained"
    );
    assert!(
        state.api_key_cache.contains_key("key_hash_5001"),
        "new key_hash_5001 must be present"
    );
}

#[tokio::test]
async fn test_api_key_cache_refresh_prevents_eviction() {
    let state = make_state().await;

    // Fill to capacity (5,000 keys)
    for i in 0..AppState::MAX_API_KEY_CACHE_ENTRIES {
        let key = Arc::new(dummy_key(i as i64, &format!("key_hash_{i}")));
        state.cache_api_key(key);
    }

    // Refresh key_hash_0: expiration renewed to now + 60s.
    let refreshed_key = Arc::new(dummy_key(0, "key_hash_0"));
    state.cache_api_key(refreshed_key);

    // Capacity remains exactly 5000 (duplicate update does not inflate)
    assert_eq!(
        state.api_key_cache.len(),
        AppState::MAX_API_KEY_CACHE_ENTRIES
    );

    // key_hash_5000 now: key_hash_0 was refreshed, so the oldest is key_hash_1.
    let new_key = Arc::new(dummy_key(5000, "key_hash_5000"));
    state.cache_api_key(new_key);

    assert_eq!(
        state.api_key_cache.len(),
        AppState::MAX_API_KEY_CACHE_ENTRIES
    );
    assert!(
        state.api_key_cache.contains_key("key_hash_0"),
        "refreshed key_hash_0 must NOT be evicted"
    );
    assert!(
        !state.api_key_cache.contains_key("key_hash_1"),
        "un-refreshed oldest key_hash_1 must be evicted"
    );
}

#[tokio::test]
async fn test_enqueue_usage_when_closed_returns_error() {
    let state = make_state().await;

    state
        .shutdown_usage_worker()
        .await
        .expect("shutdown usage worker");

    let job = openproxy_pipeline::worker::BackgroundJob::MarkClientResponse {
        request_id: "req_closed_test".into(),
        attempt: 1,
        target_id: openproxy_types::ids::ComboTargetId(1),
    };

    let result = state.enqueue_usage(job).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        err.to_string().contains("closed"),
        "error must indicate usage worker is closed, got: {err}"
    );
}

#[tokio::test]
async fn test_enqueue_usage_db_failure_returns_explicit_error() {
    let state = make_state().await;

    state
        .db_pool()
        .spawn_write(|conn| {
            conn.execute_batch(
                "CREATE TRIGGER test_fail_disk BEFORE INSERT ON usage_journal \
                 BEGIN SELECT RAISE(ABORT, 'database or disk is full'); END;",
            )
            .map_err(openproxy_db::error::map_db_error)
        })
        .await
        .expect("install failure trigger");

    let job = openproxy_pipeline::worker::BackgroundJob::MarkClientResponse {
        request_id: "req_disk_full_test".into(),
        attempt: 1,
        target_id: openproxy_types::ids::ComboTargetId(1),
    };

    let result = state.enqueue_usage(job).await;
    assert!(
        result.is_err(),
        "enqueue_usage must fail when DB write fails"
    );
    let err = result.unwrap_err();
    assert!(
        matches!(err, openproxy_types::CoreError::Database { .. }),
        "expected CoreError::Database, got: {err:?}"
    );

    state
        .db_pool()
        .spawn_write(|conn| {
            conn.execute_batch("DROP TRIGGER test_fail_disk")
                .map_err(openproxy_db::error::map_db_error)
        })
        .await
        .expect("drop failure trigger");
}

fn make_test_usage_input(request_id: RequestId) -> UsageInput {
    UsageInput {
        request_id,
        trace_id: format!("trace_{request_id}"),
        attempt: 1,
        provider_id: ProviderId::new("test-provider"),
        account_id: None,
        combo_id: None,
        combo_target_id: None,
        model_row_id: None,
        upstream_model_id: "test-model".to_string(),
        prompt_tokens: Some(12),
        completion_tokens: Some(24),
        cached_tokens: None,
        connect_ms: Some(4),
        ttft_ms: Some(15),
        total_ms: 60,
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
        flags: 0,
    }
}

#[tokio::test]
async fn test_admin_shutdown_operational_drain_inflight_and_admitted_telemetry() {
    let state = make_state().await;

    let audit_id = "audit_shutdown_operational_1".to_string();
    let unique_req_id = RequestId::new();
    let req_id_str = unique_req_id.to_string();

    let (oneshot_started_tx, oneshot_started_rx) = tokio::sync::oneshot::channel();
    let (oneshot_latch_tx, oneshot_latch_rx) = tokio::sync::oneshot::channel();
    let (oneshot_done_tx, oneshot_done_rx) = tokio::sync::oneshot::channel();
    let (write_queued_tx, write_queued_rx) = tokio::sync::oneshot::channel();

    let state_for_task = state.clone();
    let audit_id_for_task = audit_id.clone();
    let usage_for_task = make_test_usage_input(unique_req_id);

    // 1. Admitted one-shot starts under supervisor before shutdown signal
    let admitted = state.supervisor().spawn_one_shot("operational_admin_op", async move {
        let _ = oneshot_started_tx.send(());
        let _ = oneshot_latch_rx.await;

        // Perform dedicated DB write in spawn_write
        write_queued_tx.send(()).expect("signal queued write");
        state_for_task
            .db_pool()
            .spawn_write({
                let aid = audit_id_for_task.clone();
                move |conn| {
                    conn.execute_batch(
                        "CREATE TABLE IF NOT EXISTS admin_audit_events (id TEXT PRIMARY KEY, note TEXT);",
                    )
                    .map_err(openproxy_db::error::map_db_error)?;
                    conn.execute(
                        "INSERT INTO admin_audit_events (id, note) VALUES (?1, ?2);",
                        rusqlite::params![aid, "operational_drain_write"],
                    )
                    .map_err(openproxy_db::error::map_db_error)?;
                    Ok(())
                }
            })
            .await
            .expect("admin operation write must succeed");

        // Admit RecordUsage into usage journal with backpressure
        let job = BackgroundJob::RecordUsage(Box::new(usage_for_task));
        state_for_task
            .enqueue_usage(job)
            .await
            .expect("telemetry admission must succeed");

        let _ = oneshot_done_tx.send(());
    });
    assert!(admitted, "one-shot must be admitted before shutdown");

    // Wait deterministically for task start
    oneshot_started_rx.await.expect("one-shot started");

    // 2. Start shutdown_usage_worker pinned directly; verify it is pending
    let shutdown_fut = state.shutdown_usage_worker();
    tokio::pin!(shutdown_fut);
    assert!(
        futures::poll!(&mut shutdown_fut).is_pending(),
        "shutdown must remain pending while in-flight task is unreleased"
    );

    // Verify new spawn_one_shot calls are rejected and future is never polled
    let rejected_ran = Arc::new(AtomicBool::new(false));
    let r_clone = Arc::clone(&rejected_ran);
    let rejected_admitted =
        state
            .supervisor()
            .spawn_one_shot("rejected_during_shutdown", async move {
                r_clone.store(true, Ordering::SeqCst);
            });
    assert!(
        !rejected_admitted,
        "supervisor must reject new tasks after shutdown initiated"
    );
    assert!(
        !rejected_ran.load(Ordering::SeqCst),
        "rejected task future must never be executed"
    );

    // 3. Keep DB writer locked on a dedicated spawn_blocking thread to verify Tokio is not blocked
    // and shutdown drain waits for DB operation completion
    let (writer_locked_tx, writer_locked_rx) = tokio::sync::oneshot::channel();
    let (writer_release_tx, writer_release_rx) = tokio::sync::oneshot::channel();
    let pool_for_block = Arc::clone(state.db_pool());
    let blocker_handle = tokio::task::spawn_blocking(move || {
        let _guard = pool_for_block.writer();
        let _ = writer_locked_tx.send(());
        let _ = writer_release_rx.blocking_recv();
    });
    writer_locked_rx
        .await
        .expect("writer lock acquired on dedicated blocking thread");

    // Release task latch: task proceeds to spawn_write, where it waits for the locked writer
    oneshot_latch_tx.send(()).expect("release one-shot latch");
    write_queued_rx
        .await
        .expect("one-shot reached database write");

    // Poll shutdown_fut again; Tokio async thread remains responsive and shutdown stays pending
    assert!(
        futures::poll!(&mut shutdown_fut).is_pending(),
        "shutdown must remain pending while DB writer lock is held"
    );

    // 4. Release resources in deterministic order:
    // First release DB writer lock
    let _ = writer_release_tx.send(());
    blocker_handle.await.expect("blocker task finished");

    // Wait for the one-shot task to finish its DB write and telemetry enqueue
    oneshot_done_rx.await.expect("one-shot completed work");

    // Now shutdown completes cleanly (draining supervisor and flushing usage journal to usage table)
    shutdown_fut
        .await
        .expect("shutdown must complete successfully");

    // 5. Verify row IDs are persisted exactly once
    let audit_count: i64 = state
        .db_pool()
        .spawn_read({
            let aid = audit_id.clone();
            move |conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM admin_audit_events WHERE id = ?1",
                    rusqlite::params![aid],
                    |row| row.get(0),
                )
                .map_err(openproxy_db::error::map_db_error)
            }
        })
        .await
        .expect("read audit row count");
    assert_eq!(audit_count, 1, "admin audit record must exist exactly once");

    let usage_count: i64 = state
        .db_pool()
        .spawn_read({
            let rid = req_id_str.clone();
            move |conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM usage WHERE request_id = ?1",
                    rusqlite::params![rid],
                    |row| row.get(0),
                )
                .map_err(openproxy_db::error::map_db_error)
            }
        })
        .await
        .expect("read usage row count");
    assert_eq!(
        usage_count, 1,
        "telemetry usage row must exist exactly once"
    );

    // 6. Verify usage_worker_stats shows pending 0 and no failed batches
    let stats = state
        .usage_worker_stats()
        .await
        .expect("fetch usage worker stats");
    assert_eq!(
        stats.pending, 0,
        "usage journal depth must be 0 after drain"
    );
    assert_eq!(stats.failed_batches, 0, "failed batches must be 0");
}

#[tokio::test]
async fn test_admin_shutdown_caller_dropped_mid_wait_next_caller_drains_same_task() {
    let state = make_state().await;

    let audit_id = "audit_caller_dropped_test".to_string();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (latch_tx, latch_rx) = tokio::sync::oneshot::channel();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();

    let state_clone = state.clone();
    let audit_id_clone = audit_id.clone();

    let admitted = state.supervisor().spawn_one_shot("caller_drop_one_shot", async move {
        let _ = started_tx.send(());
        let _ = latch_rx.await;
        state_clone
            .db_pool()
            .spawn_write(move |conn| {
                conn.execute_batch(
                    "CREATE TABLE IF NOT EXISTS admin_audit_events (id TEXT PRIMARY KEY, note TEXT);",
                )
                .map_err(openproxy_db::error::map_db_error)?;
                conn.execute(
                    "INSERT INTO admin_audit_events (id, note) VALUES (?1, ?2);",
                    rusqlite::params![audit_id_clone, "caller_dropped_execution"],
                )
                .map_err(openproxy_db::error::map_db_error)?;
                Ok(())
            })
            .await
            .expect("spawn_write in task must succeed");
        let _ = done_tx.send(());
    });
    assert!(admitted, "one-shot must be admitted");
    started_rx.await.expect("task started");

    // Caller 1 starts shutdown_usage_worker, transfers task to drain_lock, and is dropped mid-wait
    {
        let caller1_fut = state.shutdown_usage_worker();
        tokio::pin!(caller1_fut);
        assert!(
            futures::poll!(&mut caller1_fut).is_pending(),
            "caller 1 shutdown must be pending because task is unreleased"
        );
        // caller1_fut is dropped at end of scope
    }

    // Caller 2 initiates shutdown_usage_worker: must acquire drain_lock and wait for the same retained task
    let caller2_fut = state.shutdown_usage_worker();
    tokio::pin!(caller2_fut);
    assert!(
        futures::poll!(&mut caller2_fut).is_pending(),
        "caller 2 shutdown must still be pending on the retained task"
    );

    // Release task latch and confirm task completed
    latch_tx.send(()).expect("release latch");
    done_rx.await.expect("task completed execution");

    // Caller 2 cleanly completes the shutdown
    caller2_fut.await.expect("caller 2 shutdown must succeed");

    // Verify task DB write executed exactly once
    let count: i64 = state
        .db_pool()
        .spawn_read({
            let aid = audit_id.clone();
            move |conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM admin_audit_events WHERE id = ?1",
                    rusqlite::params![aid],
                    |row| row.get(0),
                )
                .map_err(openproxy_db::error::map_db_error)
            }
        })
        .await
        .expect("read audit count");
    assert_eq!(count, 1, "task must execute and persist exactly once");

    // Verification: subsequent shutdown call is idempotent
    state
        .shutdown_usage_worker()
        .await
        .expect("subsequent shutdown must be idempotent");
}
