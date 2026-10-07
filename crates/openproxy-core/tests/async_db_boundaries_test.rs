//! Current-thread runtime regressions for core database boundaries.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use openproxy_core::oauth::DbRef;
use openproxy_core::unary::{is_target_available_async, resolve_api_key, resolve_api_key_async};
use openproxy_core::{accounts, providers};
use openproxy_db::DbPool;
use openproxy_db::secrets::MasterKey;
use openproxy_pipeline::circuit_breaker::CircuitBreakerRegistry;
use openproxy_types::config::CircuitBreakerConfig;
use openproxy_types::ids::{ComboTargetId, ProviderId};

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

#[tokio::test(flavor = "current_thread")]
async fn oauth_pool_read_and_write_support_current_thread_runtime() {
    let pool = DbPool::test_pool().expect("pool");
    let db = DbRef::Pool(&pool);
    db.with_conn_async(|conn| {
        conn.execute_batch(
            "CREATE TABLE async_boundary (value INTEGER); INSERT INTO async_boundary VALUES (7)",
        )
        .map_err(openproxy_db::error::map_db_error)
    })
    .await
    .expect("write");
    let value = db
        .with_read_conn_async(|conn| {
            conn.query_row("SELECT value FROM async_boundary", [], |row| {
                row.get::<_, i64>(0)
            })
            .map_err(openproxy_db::error::map_db_error)
        })
        .await
        .expect("read");
    assert_eq!(value, 7);
}

#[tokio::test(flavor = "current_thread")]
async fn oauth_pool_write_wait_does_not_block_runtime() {
    let pool = DbPool::test_pool().expect("pool");
    let writer = pool.writer_arc();
    let (locked_tx, locked_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let _guard = writer.lock();
        locked_tx.send(()).expect("signal lock");
        release_rx.recv_timeout(TEST_TIMEOUT).expect("release lock");
    });
    locked_rx.await.expect("writer locked");
    let release = tokio::spawn(async move {
        tokio::task::yield_now().await;
        release_tx.send(()).expect("release writer");
    });
    tokio::time::timeout(TEST_TIMEOUT, DbRef::Pool(&pool).with_conn_async(|_| Ok(())))
        .await
        .expect("runtime progressed")
        .expect("write completed");
    release.await.expect("release task");
    holder.join().expect("holder thread");
}

#[tokio::test(flavor = "current_thread")]
async fn unary_async_credentials_preserve_error_contract() {
    let pool = DbPool::test_pool().expect("pool");
    let master_key = MasterKey::generate().expect("master key");
    let provider = ProviderId::new("missing-provider");
    let sync_error = resolve_api_key(&pool, &master_key, None, &provider).expect_err("sync error");
    let async_error = resolve_api_key_async(&pool, &master_key, None, &provider)
        .await
        .expect_err("async error");
    assert_eq!(sync_error.to_string(), async_error.to_string());
}

#[tokio::test(flavor = "current_thread")]
async fn unary_reader_contention_does_not_block_runtime() {
    let pool = DbPool::test_pool().expect("pool");
    let lock_pool = pool.clone();
    let (locked_tx, locked_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let _guards: Vec<_> = (0..lock_pool.reader_count())
            .map(|_| lock_pool.reader_guard())
            .collect();
        locked_tx.send(()).expect("signal readers locked");
        release_rx
            .recv_timeout(TEST_TIMEOUT)
            .expect("release readers");
    });
    locked_rx.await.expect("readers locked");
    let release = tokio::spawn(async move {
        tokio::task::yield_now().await;
        release_tx.send(()).expect("release readers");
    });
    let circuit_breaker = CircuitBreakerRegistry::new(&CircuitBreakerConfig::default());
    let available = tokio::time::timeout(
        TEST_TIMEOUT,
        is_target_available_async(&pool, &circuit_breaker, None, Some(ComboTargetId(999))),
    )
    .await
    .expect("runtime progressed");
    assert!(available);
    release.await.expect("release task");
    holder.join().expect("holder thread");
}

#[tokio::test(flavor = "current_thread")]
async fn unary_cooldown_queries_remain_fail_open() {
    let pool = DbPool::test_pool().expect("pool");
    let circuit_breaker = Arc::new(CircuitBreakerRegistry::new(&CircuitBreakerConfig::default()));
    assert!(
        is_target_available_async(&pool, &circuit_breaker, None, Some(ComboTargetId(999))).await
    );
    pool.spawn_write(|conn| {
        conn.execute_batch("DROP TABLE target_cooldowns")
            .map_err(openproxy_db::error::map_db_error)
    })
    .await
    .expect("drop cooldown table");
    assert!(
        is_target_available_async(&pool, &circuit_breaker, None, Some(ComboTargetId(999))).await
    );
}

async fn seed_oauth_account(pool: &DbPool, master_key: &MasterKey) -> openproxy_types::AccountId {
    let master_key = master_key.clone();
    pool.spawn_write(move |conn| {
        let provider_id = ProviderId::new("cline");
        providers::create(
            conn,
            providers::NewProvider {
                id: &provider_id,
                name: "Cline",
                base_url: "https://example.invalid",
                auth_type: providers::AuthType::Bearer,
                format: providers::ProviderFormat::Openai,
                extra_headers_json: None,
                auto_activate_keyword: None,
                rate_limit_scope: providers::RateLimitScope::Account,
            },
        )?;
        let account_id = accounts::create(conn, &provider_id, None, &master_key, None, 10, None)?;
        accounts::store_oauth_tokens(
            conn,
            account_id,
            &master_key,
            accounts::StoreOAuthTokensParams {
                access_token: "stored-access-token",
                refresh_token: Some("latest-refresh-token"),
                token_type: "Bearer",
                expires_at: Some("2099-01-01T00:00:00Z"),
                ..Default::default()
            },
        )?;
        Ok(account_id)
    })
    .await
    .expect("seed oauth account")
}

#[tokio::test(flavor = "current_thread")]
async fn oauth_coordinator_reuses_stored_token_on_current_thread_runtime() {
    use openproxy_core::oauth::{
        OAuthProviderRegistry, OAuthRefreshParams, TokenRefreshCoordinator,
    };

    let pool = DbPool::test_pool().expect("pool");
    let master_key = MasterKey::generate().expect("master key");
    let account_id = seed_oauth_account(&pool, &master_key).await;
    let registry = OAuthProviderRegistry::builtin();
    let client = Arc::new(openproxy_adapters::upstream::UpstreamClient::new());
    let token = TokenRefreshCoordinator::new()
        .refresh_and_store(OAuthRefreshParams {
            provider_id: "cline",
            provider: registry.get("cline").expect("provider"),
            refresh_token: "stale-caller-token",
            upstream_client: &client,
            account_id,
            db: DbRef::Pool(&pool),
            master_key: &master_key,
            force: false,
        })
        .await
        .expect("reuse existing token without network");
    assert_eq!(token.access_token, "stored-access-token");
    assert_eq!(token.refresh_token.as_deref(), Some("latest-refresh-token"));
}

#[tokio::test(flavor = "current_thread")]
async fn discovery_async_start_preserves_initial_task_count() {
    use openproxy_core::discovery_scheduler::{DiscoverySchedulerConfig, start_async};

    let pool = Arc::new(DbPool::test_pool().expect("pool"));
    pool.spawn_write(|conn| {
        providers::create(
            conn,
            providers::NewProvider {
                id: &ProviderId::new("custom-discovery-test"),
                name: "Custom discovery test",
                base_url: "https://example.invalid",
                auth_type: providers::AuthType::None,
                format: providers::ProviderFormat::Openai,
                extra_headers_json: None,
                auto_activate_keyword: None,
                rate_limit_scope: providers::RateLimitScope::Account,
            },
        )
    })
    .await
    .expect("seed custom provider");
    let scheduler = start_async(
        pool,
        Arc::new(MasterKey::generate().expect("master key")),
        Arc::new(Vec::new()),
        openproxy_adapters::upstream::UpstreamClient::new(),
        DiscoverySchedulerConfig::default(),
    )
    .await
    .expect("scheduler startup");
    assert_eq!(scheduler.task_count, 1);
    scheduler.cancel();
}
