//! Tests for the in-memory adapter registry hot-reload path.
//!
//! The regression test exercises the bug fixed by
//! `rebuild_adapters`: prior to the fix, the registry was built
//! once at startup and never refreshed, so a `POST
//! /admin/providers` made AFTER the server was already
//! running inserted the row but left the in-memory adapter list
//! stale, causing `CoreError::ProviderNotFound(<id>)` on the
//! first chat attempt against the new provider. The fix wraps
//! the registry in an `Arc<RwLock<Vec<...>>>` and exposes
//! `rebuild_adapters()` so the admin handlers can refresh it.

use super::*;
use crate::state::AppState;
use openproxy_adapters::adapters;
use openproxy_core::{AppConfig, providers};
use openproxy_db as core_db;
use openproxy_db::MasterKey;
use openproxy_types::ids::ProviderId;
use std::path::PathBuf;
use std::sync::Arc;

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
    // MasterKey::generate().unwrap() returns a fresh 32-byte key — safe
    // for tests that don't decrypt any real secrets.
    let master_key = Arc::new(MasterKey::generate().unwrap());
    // Start with an empty adapter registry; `rebuild_adapters`
    // is responsible for filling in both the built-ins and any
    // custom rows.
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

/// Companion test: deleting a custom provider removes its
/// `CustomAdapter` from the registry on the next rebuild.
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
