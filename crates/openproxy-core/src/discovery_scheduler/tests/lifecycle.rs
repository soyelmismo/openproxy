use super::{
    fast_config, fresh_pool, models_with_provider, seed_provider_with_account, three_models,
};
use crate::discovery_scheduler::*;
use crate::ids::ProviderId as CoreProviderId;
use crate::models;
use crate::providers::{self, AuthType};
use openproxy_adapters::upstream::UpstreamClient;
use openproxy_db::secrets::MasterKey;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn scheduler_upserts_models_after_a_few_ticks() {
    let (pool, _path) = fresh_pool();
    let mk = MasterKey::generate().unwrap();
    seed_provider_with_account(&pool, &mk, "openrouter");

    let (adapter, counter) =
        openproxy_adapters::adapters::MockAdapter::with_discovery("openrouter", three_models());
    let adapters: Arc<Vec<openproxy_adapters::adapters::ProviderAdapterEnum>> = Arc::new(vec![
        openproxy_adapters::adapters::ProviderAdapterEnum::Mock(Box::new(adapter)),
    ]);

    let sched = start(
        Arc::clone(&pool),
        Arc::new(mk),
        adapters,
        UpstreamClient::new(),
        DiscoverySchedulerConfig {
            interval_secs: 2,
            initial_stagger_secs: 1,
        },
    );

    for _ in 0..100 {
        tokio::time::advance(Duration::from_millis(100)).await;
        tokio::time::sleep(Duration::from_millis(10)).await;
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        if counter.load(Ordering::SeqCst) >= 2 {
            break;
        }
    }

    let calls = counter.load(Ordering::SeqCst);
    assert!(
        calls >= 2,
        "scheduler should have ticked at least twice in 2 virtual seconds, got {calls}",
    );

    let active = models::list_active(&pool.reader(), &crate::ids::ProviderId::new("openrouter"))
        .expect("list_active");
    assert_eq!(active.len(), 3, "all 3 discovered models should be in DB");

    sched.cancel();
    let calls_at_cancel = counter.load(Ordering::SeqCst);
    for _ in 0..20 {
        tokio::time::advance(Duration::from_millis(50)).await;
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
    }
    assert_eq!(
        counter.load(Ordering::SeqCst),
        calls_at_cancel,
        "no further calls after cancel",
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn scheduler_skips_provider_with_no_accounts() {
    let (pool, _path) = fresh_pool();
    let mk = MasterKey::generate().unwrap();
    {
        let conn = pool.writer();
        let provider_id = CoreProviderId::new("openrouter");
        providers::create(
            &conn,
            providers::NewProvider {
                id: &provider_id,
                name: "openrouter",
                base_url: "https://example.invalid",
                auth_type: AuthType::Bearer,
                format: providers::ProviderFormat::Openai,
                extra_headers_json: None,
                auto_activate_keyword: None,
                rate_limit_scope: crate::providers::RateLimitScope::Account,
            },
        )
        .expect("seed provider");
    }

    let (adapter, counter) =
        openproxy_adapters::adapters::MockAdapter::with_discovery("openrouter", three_models());
    let adapters: Arc<Vec<openproxy_adapters::adapters::ProviderAdapterEnum>> = Arc::new(vec![
        openproxy_adapters::adapters::ProviderAdapterEnum::Mock(Box::new(adapter)),
    ]);

    let sched = start(
        Arc::clone(&pool),
        Arc::new(mk),
        adapters,
        UpstreamClient::new(),
        DiscoverySchedulerConfig {
            interval_secs: 1,
            initial_stagger_secs: 0,
        },
    );

    tokio::time::advance(Duration::from_secs(3)).await;
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }

    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "mock adapter must not be called when the provider has no accounts"
    );

    let rows = models_with_provider(&pool, "openrouter");
    assert!(
        rows.is_empty(),
        "no models should have been written; got {rows:?}"
    );

    sched.cancel();
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn cancelled_scheduler_stops_within_one_tick() {
    let provider_ids = [
        "openrouter",
        "minimax",
        "opencode-zen",
        "opencode-go",
        "ollama-cloud",
    ];

    let (pool, _path) = fresh_pool();
    let mk = MasterKey::generate().unwrap();
    for pid in provider_ids {
        seed_provider_with_account(&pool, &mk, pid);
    }

    let (adapters, counters): (
        Vec<openproxy_adapters::adapters::ProviderAdapterEnum>,
        Vec<Arc<AtomicUsize>>,
    ) = provider_ids
        .iter()
        .map(|pid| {
            let (a, c) =
                openproxy_adapters::adapters::MockAdapter::with_discovery(pid, three_models());
            (
                openproxy_adapters::adapters::ProviderAdapterEnum::Mock(Box::new(a)),
                c,
            )
        })
        .unzip();
    let adapters = Arc::new(adapters);

    let sched = start(
        Arc::clone(&pool),
        Arc::new(mk),
        adapters,
        UpstreamClient::new(),
        DiscoverySchedulerConfig {
            interval_secs: 3_600,
            initial_stagger_secs: 0,
        },
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while counters.iter().any(|c| c.load(Ordering::SeqCst) == 0) {
        if std::time::Instant::now() >= deadline {
            break;
        }
        tokio::task::yield_now().await;
    }
    for (pid, c) in provider_ids.iter().zip(counters.iter()) {
        let n = c.load(Ordering::SeqCst);
        assert_eq!(
            n, 1,
            "adapter for {pid} should have been called exactly once after the first tick, got {n}",
        );
    }

    let cancel_started_at = std::time::Instant::now();

    sched.cancel();
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
    for _ in 0..=3_600 {
        tokio::time::advance(Duration::from_secs(1)).await;
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
    }

    let elapsed = cancel_started_at.elapsed();
    assert!(
        elapsed < Duration::from_millis(500),
        "cancel + 1h+1s virtual advance should be near-instant; took {elapsed:?}",
    );

    for (pid, c) in provider_ids.iter().zip(counters.iter()) {
        let n = c.load(Ordering::SeqCst);
        assert_eq!(
            n, 1,
            "broadcast cancel failed: adapter for {pid} received {n} calls (expected 1) \
             — the cancel primitive did not wake every per-provider task",
        );
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn scheduler_skips_providers_without_an_adapter() {
    let (pool, _path) = fresh_pool();
    let mk = MasterKey::generate().unwrap();
    let adapters: Arc<Vec<openproxy_adapters::adapters::ProviderAdapterEnum>> = Arc::new(vec![]);

    let sched = start(
        Arc::clone(&pool),
        Arc::new(mk),
        adapters,
        UpstreamClient::new(),
        fast_config(),
    );
    assert_eq!(sched.task_count, 0, "no providers had an adapter");
}
