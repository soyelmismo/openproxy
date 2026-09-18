//! Application state constructors (`new` and `for_test`).

use super::background::{
    SpawnBackgroundTasksArgs, spawn_background_tasks, spawn_memory_cleanup,
    spawn_rate_limiter_cleanup, start_discovery_scheduler,
};
use super::init::{init_database, run_database_maintenance};
use super::{AppState, BackfillStatus, VacuumStatus};
use openproxy_adapters::adapters;
use openproxy_adapters::upstream::UpstreamClient;
use openproxy_core::{AppConfig, discovery_scheduler, oauth, usage};
use openproxy_db as db;
use openproxy_db::MasterKey;
use parking_lot::RwLock;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

impl AppState {
    /// Build state from a fully-loaded config.
    pub fn new(config: AppConfig) -> anyhow::Result<Self> {
        let db_pool = Arc::new(init_database(&config)?);
        let mut config = config;
        let mut recording_ttl_secs = db::app_config::RECORDING_TTL_DEFAULT_SECS;
        let mut idle_chunk_retryable = db::app_config::IDLE_CHUNK_RETRYABLE_DEFAULT;
        let mut compression_mode = openproxy_compression::CompressionMode::Off;
        let mut notifications_enabled = db::app_config::NOTIFICATIONS_ENABLED_DEFAULT;

        run_database_maintenance(
            &mut db_pool.writer(),
            &mut config,
            &mut recording_ttl_secs,
            &mut idle_chunk_retryable,
            &mut compression_mode,
            &mut notifications_enabled,
        )?;

        let usage_tx = usage::init_usage_broadcast();
        let stage_tx = usage::init_stage_broadcast();
        let models_refreshed_tx = openproxy_core::models::init_models_refreshed_broadcast();
        openproxy_core::notifications::init_broadcast();

        let recording_ttl_secs_cell = Arc::new(RwLock::new(recording_ttl_secs));
        let idle_chunk_retryable_cell = Arc::new(AtomicBool::new(idle_chunk_retryable));
        let compression_mode_cell = Arc::new(RwLock::new(compression_mode));
        let quota_protection_cell = Arc::new(RwLock::new(config.quota_protection.clone()));
        let pii_config_cell = Arc::new(RwLock::new(config.pii.clone()));
        let notifications_enabled_cell = Arc::new(AtomicBool::new(notifications_enabled));
        openproxy_core::notifications::set_enabled(notifications_enabled);

        let master_key = Arc::new(MasterKey::from_env()?);
        let initial_adapters = Self::load_adapters(&db_pool)?;
        let adapters: Arc<RwLock<Arc<Vec<adapters::ProviderAdapterEnum>>>> =
            Arc::new(RwLock::new(Arc::new(initial_adapters)));

        let maintenance_cell = Arc::new(RwLock::new(config.storage.maintenance.clone()));
        let vacuum_status = Arc::new(RwLock::new(VacuumStatus::default()));
        let backfill_status = Arc::new(RwLock::new(BackfillStatus::default()));
        let upstream_client = UpstreamClient::new();
        let oauth_provider_registry = Arc::new(oauth::OAuthProviderRegistry::builtin());
        let supervisor = Arc::new(crate::background::BackgroundSupervisor::new());

        spawn_background_tasks(
            &supervisor,
            SpawnBackgroundTasksArgs {
                db_pool: Arc::clone(&db_pool),
                config: config.clone(),
                recording_ttl_secs_cell: Arc::clone(&recording_ttl_secs_cell),
                maintenance_cell: Arc::clone(&maintenance_cell),
                vacuum_status: Arc::clone(&vacuum_status),
                backfill_status: Arc::clone(&backfill_status),
                master_key: Arc::clone(&master_key),
                adapters: Arc::clone(&adapters),
                upstream_client: Arc::clone(&upstream_client),
                oauth_provider_registry: Arc::clone(&oauth_provider_registry),
            },
        );

        let discovery_scheduler = Arc::new(start_discovery_scheduler(
            Arc::clone(&db_pool),
            Arc::clone(&master_key),
            &adapters,
            Arc::clone(&upstream_client),
        ));

        openproxy_core::smart_warmup::start_smart_warmup_scheduler(
            Arc::clone(&db_pool),
            config.clone(),
            Arc::clone(&upstream_client),
            Arc::clone(&master_key),
        );

        let timeouts_initial = config.timeouts;
        let rate_limiter_config = openproxy_core::rate_limit::RateLimitConfig {
            max_requests: config.server.rate_limit_requests_per_minute,
            ..Default::default()
        };
        let rate_limiter: Arc<dyn openproxy_core::rate_limit::RateLimiter> = Arc::new(
            openproxy_core::rate_limit::SlidingWindowRateLimiter::new(rate_limiter_config),
        );
        spawn_rate_limiter_cleanup(&supervisor, Arc::clone(&rate_limiter));

        let selection_registry = Arc::new(openproxy_types::SelectionRegistry::new());
        let circuit_breaker = openproxy_pipeline::circuit_breaker::CircuitBreakerRegistry::new(
            &openproxy_types::config::CircuitBreakerConfig {
                failure_threshold: 5,
                unhealthy_duration_ms: 60_000,
            },
        );
        let predictive_limiter = Arc::new(openproxy_pipeline::PredictiveRateLimiter::new());
        let api_key_cache = Arc::new(dashmap::DashMap::new());
        spawn_memory_cleanup(
            &supervisor,
            Arc::clone(&db_pool),
            Arc::clone(&selection_registry),
            circuit_breaker.clone(),
            Arc::clone(&predictive_limiter),
            Arc::clone(&api_key_cache),
        );

        let (background_tx, background_rx) = tokio::sync::mpsc::channel(1024);
        let repo = Arc::new(openproxy_pipeline::SqlitePipelineRepository::new(
            db_pool.writer_arc(),
        ));
        openproxy_pipeline::worker::spawn_worker(
            db_pool.writer_arc(),
            repo,
            background_rx,
            Arc::clone(&selection_registry),
        );

        let services = Arc::new(crate::services::Services::new(Arc::clone(&db_pool)));

        // Immediate post-startup page trimming to release migration and seed burst memory.
        db_pool.shrink_memory();
        unsafe {
            libmimalloc_sys::mi_collect(true);
        }

        let state = Self {
            config,
            db_pool,
            master_key,
            services,
            adapters,
            rate_limiter,
            upstream_client,
            usage_tx,
            stage_tx,
            models_refreshed_tx,
            record_bodies_and_headers: Arc::new(AtomicBool::new(false)),
            timeouts_cell: Arc::new(RwLock::new(timeouts_initial)),
            compression_mode_cell,
            recording_ttl_secs_cell,
            discovery_scheduler,
            oauth_provider_registry,
            idle_chunk_retryable_cell,
            quota_protection_cell,
            pii_config_cell,
            notifications_enabled_cell,
            selection_registry,
            circuit_breaker,
            predictive_limiter,
            maintenance_cell,
            vacuum_status,
            backfill_status,
            background_tx,
            supervisor,
            api_key_cache,
        };

        Ok(state)
    }

    /// Build a minimal `AppState` suitable for tests.
    pub fn for_test(
        config: AppConfig,
        db_pool: Arc<db::DbPool>,
        master_key: Arc<MasterKey>,
        adapters: Arc<RwLock<Arc<Vec<adapters::ProviderAdapterEnum>>>>,
    ) -> Self {
        let recording_ttl_secs_cell =
            Arc::new(RwLock::new(db::app_config::RECORDING_TTL_DEFAULT_SECS));
        let maintenance_cell = Arc::new(RwLock::new(
            openproxy_types::config::MaintenanceConfig::default(),
        ));
        let vacuum_status = Arc::new(RwLock::new(VacuumStatus::default()));
        let backfill_status = Arc::new(RwLock::new(BackfillStatus::default()));
        let upstream_client = UpstreamClient::new();
        let oauth_provider_registry = Arc::new(oauth::OAuthProviderRegistry::builtin());
        let supervisor = Arc::new(crate::background::BackgroundSupervisor::new());

        spawn_background_tasks(
            &supervisor,
            SpawnBackgroundTasksArgs {
                db_pool: Arc::clone(&db_pool),
                config: config.clone(),
                recording_ttl_secs_cell: Arc::clone(&recording_ttl_secs_cell),
                maintenance_cell: Arc::clone(&maintenance_cell),
                vacuum_status: Arc::clone(&vacuum_status),
                backfill_status: Arc::clone(&backfill_status),
                master_key: Arc::clone(&master_key),
                adapters: Arc::clone(&adapters),
                upstream_client: Arc::clone(&upstream_client),
                oauth_provider_registry: Arc::clone(&oauth_provider_registry),
            },
        );

        let adapters_snapshot = Arc::clone(&adapters.read());
        let discovery_scheduler = discovery_scheduler::start(
            Arc::clone(&db_pool),
            Arc::clone(&master_key),
            Arc::clone(&adapters_snapshot),
            Arc::clone(&upstream_client),
            openproxy_core::discovery_scheduler::DiscoverySchedulerConfig {
                interval_secs: 3_600,
                initial_stagger_secs: 0,
            },
        );

        let rate_limiter_config = openproxy_core::rate_limit::RateLimitConfig {
            max_requests: config.server.rate_limit_requests_per_minute,
            ..Default::default()
        };
        let rate_limiter: Arc<dyn openproxy_core::rate_limit::RateLimiter> = Arc::new(
            openproxy_core::rate_limit::SlidingWindowRateLimiter::new(rate_limiter_config),
        );
        spawn_rate_limiter_cleanup(&supervisor, Arc::clone(&rate_limiter));

        let selection_registry = Arc::new(openproxy_types::SelectionRegistry::new());
        let circuit_breaker = openproxy_pipeline::circuit_breaker::CircuitBreakerRegistry::new(
            &openproxy_types::config::CircuitBreakerConfig {
                failure_threshold: 5,
                unhealthy_duration_ms: 60_000,
            },
        );
        let predictive_limiter = Arc::new(openproxy_pipeline::PredictiveRateLimiter::new());
        let api_key_cache = Arc::new(dashmap::DashMap::new());
        spawn_memory_cleanup(
            &supervisor,
            Arc::clone(&db_pool),
            Arc::clone(&selection_registry),
            circuit_breaker.clone(),
            Arc::clone(&predictive_limiter),
            Arc::clone(&api_key_cache),
        );

        openproxy_core::notifications::init_broadcast();

        let (background_tx, _) = tokio::sync::mpsc::channel(1);
        let services = Arc::new(crate::services::Services::new(Arc::clone(&db_pool)));

        Self {
            config: config.clone(),
            db_pool,
            master_key,
            services,
            adapters,
            rate_limiter,
            upstream_client,
            usage_tx: usage::init_usage_broadcast(),
            stage_tx: usage::init_stage_broadcast(),
            models_refreshed_tx: openproxy_core::models::init_models_refreshed_broadcast(),
            record_bodies_and_headers: Arc::new(AtomicBool::new(false)),
            timeouts_cell: Arc::new(RwLock::new(config.timeouts)),
            compression_mode_cell: Arc::new(RwLock::new(
                openproxy_compression::CompressionMode::Off,
            )),
            recording_ttl_secs_cell,
            discovery_scheduler: Arc::new(discovery_scheduler),
            oauth_provider_registry,
            idle_chunk_retryable_cell: Arc::new(AtomicBool::new(
                db::app_config::IDLE_CHUNK_RETRYABLE_DEFAULT,
            )),
            quota_protection_cell: Arc::new(RwLock::new(config.quota_protection)),
            pii_config_cell: Arc::new(RwLock::new(config.pii)),
            notifications_enabled_cell: Arc::new(AtomicBool::new(
                db::app_config::NOTIFICATIONS_ENABLED_DEFAULT,
            )),
            selection_registry,
            circuit_breaker,
            predictive_limiter,
            maintenance_cell,
            vacuum_status,
            backfill_status,
            background_tx,
            supervisor,
            api_key_cache,
        }
    }
}
