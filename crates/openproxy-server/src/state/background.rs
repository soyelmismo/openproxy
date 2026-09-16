//! Background task wiring and supervisor task scheduling.

use openproxy_adapters::adapters;
use openproxy_adapters::upstream::UpstreamClient;
use openproxy_core::discovery_scheduler::{self, DiscoveryScheduler};
use openproxy_db as db;
use openproxy_db::MasterKey;
use parking_lot::RwLock;
use std::sync::Arc;

pub(crate) struct SpawnBackgroundTasksArgs {
    pub(crate) db_pool: Arc<db::DbPool>,
    pub(crate) config: openproxy_core::AppConfig,
    pub(crate) recording_ttl_secs_cell: Arc<RwLock<i64>>,
    pub(crate) maintenance_cell: Arc<RwLock<openproxy_types::config::MaintenanceConfig>>,
    pub(crate) vacuum_status: Arc<RwLock<super::VacuumStatus>>,
    pub(crate) backfill_status: Arc<RwLock<super::BackfillStatus>>,
    pub(crate) master_key: Arc<MasterKey>,
    pub(crate) adapters: Arc<RwLock<Arc<Vec<adapters::ProviderAdapterEnum>>>>,
    pub(crate) upstream_client: Arc<UpstreamClient>,
    pub(crate) oauth_provider_registry: Arc<openproxy_core::oauth::OAuthProviderRegistry>,
}

pub(crate) fn spawn_background_tasks(
    supervisor: &crate::background::BackgroundSupervisor,
    args: SpawnBackgroundTasksArgs,
) {
    let SpawnBackgroundTasksArgs {
        db_pool,
        config,
        recording_ttl_secs_cell,
        maintenance_cell,
        vacuum_status,
        backfill_status,
        master_key,
        adapters,
        upstream_client,
        oauth_provider_registry,
    } = args;

    // Boot backfill: provider seed + repricing + bootstrap key. First
    // tick fires immediately so the dashboard sees a "backfilling"
    // banner rather than waiting for the interval. We use a 6h cadence
    // on subsequent passes so pricing drift after a models.dev sync is
    // picked up.
    supervisor.spawn(crate::background::BackfillService {
        db_pool: Arc::clone(&db_pool),
        backfill_status: Arc::clone(&backfill_status),
        interval: std::time::Duration::from_hours(6),
    });

    supervisor.spawn(crate::background::CooldownPrunerService {
        db_pool: Arc::clone(&db_pool),
        interval: std::time::Duration::from_mins(1),
    });

    supervisor.spawn(crate::background::RecordingTtlPrunerService {
        db_pool: Arc::clone(&db_pool),
        recording_ttl_secs_cell,
        interval: std::time::Duration::from_mins(1),
    });

    supervisor.spawn(crate::background::OAuthRefreshService {
        db_pool: Arc::clone(&db_pool),
        master_key: Arc::clone(&master_key),
        upstream_client: Arc::clone(&upstream_client),
        oauth_provider_registry: Arc::clone(&oauth_provider_registry),
    });

    let models_dev_enabled =
        std::env::var("MODELS_DEV_SYNC_ENABLED").is_ok_and(|v| v == "1" || v == "true");
    if models_dev_enabled {
        let interval_secs: u64 = std::env::var("MODELS_DEV_SYNC_INTERVAL_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(86_400);
        supervisor.spawn(crate::background::ModelsDevSyncService {
            db_pool: Arc::clone(&db_pool),
            upstream_client: Arc::clone(&upstream_client),
            interval_secs,
        });
    }

    openproxy_core::quota_sync::start_quota_sync_scheduler(
        Arc::clone(&db_pool),
        config,
        Arc::clone(&upstream_client),
        Arc::clone(&master_key),
        Arc::clone(&adapters),
        Arc::clone(&oauth_provider_registry),
    );

    supervisor.spawn(crate::background::FreeProxiesSyncService {
        db_pool: Arc::clone(&db_pool),
    });

    supervisor.spawn(crate::background::MaintenanceVacuumService {
        db_pool,
        maintenance_cell,
        vacuum_status,
    });
}

pub(crate) fn start_discovery_scheduler(
    db_pool: Arc<openproxy_db::DbPool>,
    master_key: Arc<openproxy_db::secrets::MasterKey>,
    adapters: &Arc<RwLock<Arc<Vec<openproxy_adapters::adapters::ProviderAdapterEnum>>>>,
    upstream_client: Arc<openproxy_adapters::upstream::UpstreamClient>,
) -> DiscoveryScheduler {
    let adapters_clone = Arc::clone(&adapters.read());
    discovery_scheduler::start(
        db_pool,
        master_key,
        adapters_clone,
        upstream_client,
        openproxy_core::discovery_scheduler::DiscoverySchedulerConfig::default(),
    )
}

pub(crate) fn spawn_rate_limiter_cleanup(
    supervisor: &crate::background::BackgroundSupervisor,
    rate_limiter: Arc<dyn openproxy_core::rate_limit::RateLimiter>,
) {
    supervisor.spawn(crate::background::RateLimiterCleanupService {
        rate_limiter,
        interval: std::time::Duration::from_mins(5),
    });
}

pub(crate) fn spawn_memory_cleanup(
    supervisor: &crate::background::BackgroundSupervisor,
    db_pool: Arc<openproxy_db::DbPool>,
    selection_registry: Arc<openproxy_types::SelectionRegistry>,
    circuit_breaker: openproxy_pipeline::circuit_breaker::CircuitBreakerRegistry,
    predictive_limiter: Arc<openproxy_pipeline::PredictiveRateLimiter>,
    api_key_cache: Arc<
        dashmap::DashMap<String, (Arc<openproxy_core::api_keys::ApiKey>, std::time::Instant)>,
    >,
) {
    supervisor.spawn(crate::background::MemoryCleanupService {
        db_pool,
        selection_registry,
        circuit_breaker,
        predictive_limiter,
        api_key_cache,
    });
}
