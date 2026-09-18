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

    // Oldest 1,000 keys (key_hash_0 .. key_hash_999) must have been evicted
    assert!(state.get_cached_api_key("key_hash_0").is_none());
    assert!(state.get_cached_api_key("key_hash_500").is_none());
    assert!(state.get_cached_api_key("key_hash_999").is_none());

    // Newest 5,000 keys (key_hash_1000 .. key_hash_5999) must be retained
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

    // Insert key_hash_5000: capacity check triggers pruning first
    let new_key = Arc::new(dummy_key(5000, "key_hash_5000"));
    state.cache_api_key(new_key);

    assert_eq!(
        state.api_key_cache.len(),
        AppState::MAX_API_KEY_CACHE_ENTRIES
    );

    // key_hash_2500 was expired, so it must have been evicted first!
    assert!(
        !state.api_key_cache.contains_key("key_hash_2500"),
        "expired key_hash_2500 must be evicted first"
    );

    // key_hash_0 (oldest non-expired) must STILL be present
    assert!(
        state.api_key_cache.contains_key("key_hash_0"),
        "oldest non-expired key_hash_0 must NOT be evicted when an expired key exists"
    );

    // Now insert key_hash_5001 when NO keys are expired:
    // Oldest key (key_hash_0) must be evicted!
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

    // Refresh key_hash_0: its expiration is renewed to now + 60s
    let refreshed_key = Arc::new(dummy_key(0, "key_hash_0"));
    state.cache_api_key(refreshed_key);

    // Capacity remains exactly 5000 (duplicate update does not inflate)
    assert_eq!(
        state.api_key_cache.len(),
        AppState::MAX_API_KEY_CACHE_ENTRIES
    );

    // Now insert new key_hash_5000:
    // Since key_hash_0 was refreshed, the oldest key is now key_hash_1!
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
