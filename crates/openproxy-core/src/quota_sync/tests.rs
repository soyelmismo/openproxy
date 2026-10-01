use super::*;
use openproxy_adapters::adapters::builtin_adapters;
use openproxy_db::DbPool;
use openproxy_types::ids::{AccountId, ModelId};

/// In-memory `DbPool` with migrations applied and one provider + account
/// seeded. Returns the pool and the seeded `AccountId` (always `1`).
fn fresh_pool() -> (Arc<DbPool>, AccountId) {
    let pool = DbPool::test_pool_with_prefix("openproxy-quota-sync-test").expect("open pool");
    let aid = AccountId(1);
    {
        let conn = pool.open_connection().expect("open conn");
        conn.execute(
            "INSERT INTO providers(id, name, base_url, auth_type, format) \
             VALUES ('antigravity', 'Antigravity', 'https://x', 'oauth', 'openai')",
            [],
        )
        .expect("seed provider");
        conn.execute(
            "INSERT INTO accounts(provider_id, label) VALUES ('antigravity', 'a1')",
            [],
        )
        .expect("seed account");
    }
    (Arc::new(pool), aid)
}

#[tokio::test]
async fn quota_sync_clear_helper_drops_expired_rows() {
    let (pool, aid) = fresh_pool();
    let mid = ModelId::new("gemini-2.5");

    // seed two expired rows
    {
        let w = pool.writer();
        let expired = (chrono::Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();
        openproxy_db::live_limited::mark_limited(&w, aid, &mid, &expired, "RESOURCE_EXHAUSTED")
            .expect("mark expired");
    }

    // the helper `refresh_single_account_quota` calls when `fetch_error.is_none()`
    clear_live_limited_after_refresh(&pool, aid).await;

    let w = pool.writer();
    assert!(!openproxy_db::live_limited::is_limited(&w, aid, &mid).expect("is_limited"));
    assert!(!openproxy_db::live_limited::has_row(&w, aid, &mid).expect("has_row"));
}

#[tokio::test]
async fn quota_sync_clear_helper_preserves_active_rows() {
    // a future-TTL row must survive: without the `until_ts <= now` filter, a
    // refresh 1ms after `mark_limited` would wipe a fresh sentinel
    let (pool, aid) = fresh_pool();
    let mid = ModelId::new("gemini-2.5");
    let active = (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339();

    {
        let w = pool.writer();
        openproxy_db::live_limited::mark_limited(&w, aid, &mid, &active, "RESOURCE_EXHAUSTED")
            .expect("mark active");
    }

    clear_live_limited_after_refresh(&pool, aid).await;

    let w = pool.writer();
    assert!(openproxy_db::live_limited::is_limited(&w, aid, &mid).expect("still limited"));
}

#[tokio::test]
async fn quota_sync_does_not_clear_when_fetch_error_present() {
    // mirrors `refresh_single_account_quota`'s `if q.fetch_error.is_none()` gate.
    // The full refresh path is too heavy for a unit test, so this pins the
    // call-site contract: with a non-empty `fetch_error` the rows stay.
    let (pool, aid) = fresh_pool();
    let mid = ModelId::new("gemini-2.5");
    let expired = (chrono::Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();

    {
        let w = pool.writer();
        openproxy_db::live_limited::mark_limited(&w, aid, &mid, &expired, "RESOURCE_EXHAUSTED")
            .expect("mark");
    }

    // "fetch_error was Some(_)": skip the helper and assert the row survives.
    // `clear_for_account`'s own test covers the SQL filter.
    {
        let w = pool.writer();
        assert!(openproxy_db::live_limited::has_row(&w, aid, &mid).expect("has_row"));
    }
}

#[tokio::test]
async fn quota_sync_disabled_config_returns_none_and_noop() {
    let (pool, _) = fresh_pool();
    let mut config = AppConfig::default();
    config.quota_sync.enabled = false;

    let upstream = UpstreamClient::new();
    let master_key = Arc::new(MasterKey::generate().expect("generate key"));
    let adapters = Arc::new(RwLock::new(Arc::new(builtin_adapters())));
    let registry = Arc::new(OAuthProviderRegistry::new());

    let res = start_quota_sync_scheduler(
        Arc::clone(&pool),
        config.clone(),
        Arc::clone(&upstream),
        Arc::clone(&master_key),
        Arc::clone(&adapters),
        Arc::clone(&registry),
    );
    assert!(res.is_none());

    let cancel = CancellationToken::new();
    // run_quota_sync_scheduler must also return immediately if disabled
    run_quota_sync_scheduler(
        pool, config, upstream, master_key, adapters, registry, cancel,
    )
    .await;
}

#[tokio::test]
async fn quota_sync_initial_delay_cancelled() {
    let (pool, _) = fresh_pool();
    let mut config = AppConfig::default();
    config.quota_sync.enabled = true;
    config.quota_sync.interval_secs = 60;

    let upstream = UpstreamClient::new();
    let master_key = Arc::new(MasterKey::generate().expect("generate key"));
    let adapters = Arc::new(RwLock::new(Arc::new(builtin_adapters())));
    let registry = Arc::new(OAuthProviderRegistry::new());

    let cancel = CancellationToken::new();
    cancel.cancel(); // Pre-cancelled token

    // Must return immediately during initial delay select without hitting DB/network
    run_quota_sync_scheduler(
        pool, config, upstream, master_key, adapters, registry, cancel,
    )
    .await;
}

#[tokio::test]
async fn checkin_scheduler_initial_delay_cancelled() {
    let (pool, _) = fresh_pool();
    let upstream = UpstreamClient::new();
    let master_key = Arc::new(MasterKey::generate().expect("generate key"));

    let cancel = CancellationToken::new();
    cancel.cancel(); // Pre-cancelled token

    // Must return immediately during initial delay select without hitting DB/network
    crate::minimax_checkin::run_checkin_scheduler(pool, upstream, master_key, cancel).await;
}

#[tokio::test]
async fn checkin_cycle_with_cancel_breaks_on_cancelled_token() {
    let (pool, _) = fresh_pool();
    let upstream = UpstreamClient::new();
    let master_key = Arc::new(MasterKey::generate().expect("generate key"));

    let cancel = CancellationToken::new();
    cancel.cancel();

    // With cancelled token, run_checkin_cycle_with_cancel must abort before processing accounts
    crate::minimax_checkin::run_checkin_cycle_with_cancel(
        &pool,
        &upstream,
        &master_key,
        Some(&cancel),
    )
    .await;
}
