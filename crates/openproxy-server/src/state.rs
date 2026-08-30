//! Application state shared across all handlers.
//!
//! `AppState` is constructed once at startup and then cloned (via `Arc`
//! internally) into every axum handler. It owns:
//!
//! - The parsed [`AppConfig`] (timeouts, racing, logging, etc.).
//! - The SQLite [`DbPool`] used for all persistence.
//! - The [`MasterKey`] used to decrypt provider API keys at request time.
//! - The registry of built-in [`ProviderAdapter`]s.
//! - A shared [`UpstreamClient`] used for upstream LLM calls.
//!
//! All heavy fields are wrapped in `Arc` so handler signatures stay
//! cheap-to-clone and the type itself is `Send + Sync` by construction.

use openproxy_adapters::adapters;
use openproxy_adapters::upstream::UpstreamClient;
use openproxy_core::{
    AppConfig,
    discovery_scheduler::{self, DiscoveryScheduler},
    oauth, usage,
};
use openproxy_db as db;
use openproxy_db::MasterKey;
use parking_lot::RwLock;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// Per-process application state.
///
/// Cheap to clone: every field is either an `Arc` or a cheap handle
/// (`UpstreamClient` is internally `Arc`-backed, [`AppConfig`] is `Clone`).
#[derive(Clone)]
pub struct AppState {
    config: AppConfig,
    db_pool: Arc<db::DbPool>,
    master_key: Arc<MasterKey>,
    services: Arc<crate::services::Services>,
    adapters: Arc<RwLock<Arc<Vec<adapters::ProviderAdapterEnum>>>>,
    /// Per-key rate limiter for /v1/chat/completions. Prevents a
    /// single API key from driving unlimited paid upstream traffic.
    rate_limiter: Arc<dyn openproxy_core::rate_limit::RateLimiter>,
    /// Shared hyper-based upstream client used by the new
    /// `UpstreamClient::call()` path. The chat pipeline and the
    /// kiro/antigravity executors hold an `Arc<UpstreamClient>`
    /// clone of this; the admin `run_test_for_model` endpoint
    /// pulls it from [`Self::upstream_client`].
    upstream_client: Arc<UpstreamClient>,
    usage_tx: tokio::sync::broadcast::Sender<openproxy_types::usage::RecentUsageRow>,
    /// Secondary broadcast sender for in-flight stage events
    /// (`started`/`connecting`/`waiting_ttft`/`streaming`/`failed`).
    /// The live-log dashboard subscribes to both senders and
    /// multiplexes them into a single WS stream.
    stage_tx: tokio::sync::broadcast::Sender<openproxy_types::usage::StageEvent>,
    /// Shared toggle that controls whether the pipeline records full
    /// request/response bodies and headers in the `usage` table.
    /// The chat handler passes a clone of this `Arc` into every
    /// `Pipeline` it builds so the admin endpoint can flip the
    /// state for the whole process at once.
    record_bodies_and_headers: Arc<AtomicBool>,
    /// Hot-swappable slot for [`openproxy_types::config::TimeoutsConfig`].
    /// Reads in `chat.rs` go through [`AppState::timeouts`] which
    /// copies the 5-u64 struct atomically. Writes are done by the
    /// `PUT /admin/config/timeouts` handler after the DB
    /// row has been updated. See spec §5 / §7.
    timeouts_cell: Arc<RwLock<openproxy_types::config::TimeoutsConfig>>,
    /// Hot-swappable slot for [`openproxy_compression::CompressionMode`].
    compression_mode_cell: Arc<RwLock<openproxy_compression::CompressionMode>>,
    /// Hot-swappable slot for the recording body TTL in seconds.
    /// This controls how long request/response bodies and headers
    /// remain in the `usage` table before being nullified.
    /// Default: 300 (5 minutes). The background prune task reads
    /// this on each tick.
    recording_ttl_secs_cell: Arc<RwLock<i64>>,
    /// Background model-discovery scheduler (Gate A). Owns one
    /// `tokio::sync::Notify` shared by all per-provider tasks;
    /// dropping the `AppState` does NOT cancel the running tasks
    /// (they keep going until the runtime shuts down), but a
    /// future Drop impl can call `.cancel()` on this handle to
    /// stop them explicitly. Today no caller cancels it — the
    /// scheduler is fire-and-forget at boot.
    ///
    /// Exposed via [`Self::discovery_scheduler`] so the field
    /// stays live (and is discoverable for the future Drop impl
    /// / admin endpoints) without an `#[allow(dead_code)]`.
    discovery_scheduler: Arc<DiscoveryScheduler>,
    /// Registry of OAuth provider implementations. Used by the
    /// pipeline (on-demand token refresh during chat requests),
    /// the background refresh scheduler, and the admin handlers.
    /// Built-in providers (antigravity, kiro) are registered at
    /// startup; custom providers can be added via
    /// `oauth_provider_registry().register()`.
    oauth_provider_registry: Arc<openproxy_core::oauth::OAuthProviderRegistry>,
    /// Hot-swappable flag: when true, idle_chunk timeouts are
    /// treated as retryable (pipeline falls through to the next
    /// target). Default false. Persisted in `app_config` table.
    idle_chunk_retryable_cell: Arc<AtomicBool>,
    /// Hot-swappable configuration for quota protection.
    quota_protection_cell: Arc<parking_lot::RwLock<openproxy_types::config::QuotaProtectionConfig>>,
    /// In-memory selection registry for the LKGP / least_used /
    /// p2c priority modes (migration 000035). Tracks per-target
    /// recent success timestamps and request counts so the
    /// pipeline's dispatcher can prefer "known-good" or
    /// "less-loaded" targets. Single-instance, lost on restart —
    /// same shape as the per-pipeline `rr_counters`. Shared with
    /// every per-request `Pipeline` via
    /// [`Pipeline::with_selection_registry`] so LKGP state
    /// survives across requests.
    selection_registry: Arc<openproxy_types::SelectionRegistry>,
    /// Shared circuit breaker registry. Created once at boot and
    /// cloned into every per-request `Pipeline` (the registry is
    /// `Clone` — its inner state is `Arc<Mutex<...>>`, so clones
    /// share the same underlying map). This makes the breaker
    /// actually functional: failures in one request affect the
    /// next, and the `prune_idle` sweep in the background task
    /// can clean up stale entries.
    circuit_breaker: openproxy_pipeline::circuit_breaker::CircuitBreakerRegistry,
    predictive_limiter: Arc<openproxy_pipeline::PredictiveRateLimiter>,
    /// VACUUM maintenance settings (runtime-editable via the dashboard
    /// config view or `PUT /admin/api/config/maintenance`). The
    /// background task reads these on every tick.
    maintenance_cell: Arc<RwLock<openproxy_types::config::MaintenanceConfig>>,
    /// VACUUM status: last-run timestamp, last-run result, and whether
    /// a VACUUM is currently in progress. Read by the dashboard's
    /// config view to show the button state.
    vacuum_status: Arc<RwLock<VacuumStatus>>,
    /// Sender for background worker jobs (usage insertion, cooldowns)
    background_tx: tokio::sync::mpsc::Sender<openproxy_pipeline::worker::BackgroundJob>,
    /// Supervisor for managing background service lifecycles and graceful shutdown.
    supervisor: Arc<crate::background::BackgroundSupervisor>,
    /// In-memory cache for validated API keys (key_hash -> (Arc<ApiKey>, expires_at))
    api_key_cache: Arc<dashmap::DashMap<String, (Arc<openproxy_core::api_keys::ApiKey>, std::time::Instant)>>,
}

/// VACUUM status reported to the dashboard. Updated by the background
/// task and by the manual `POST /admin/api/debug/vacuum` endpoint.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct VacuumStatus {
    /// ISO-8601 timestamp of the last VACUUM run (manual or automatic).
    /// `None` if no VACUUM has run since boot.
    pub last_run: Option<String>,
    /// `"ok"` or the error message from the last run.
    pub last_result: Option<String>,
    /// `true` while a VACUUM is in progress (the button shows a
    /// spinner and is disabled).
    pub in_progress: bool,
    /// ISO-8601 timestamp of the next scheduled automatic VACUUM.
    /// `None` if auto_vacuum is disabled.
    pub next_scheduled: Option<String>,
}

impl AppState {
    /// Build state from a fully-loaded config.
    ///
    /// Steps (in order, per spec §2 / §10):
    /// 1. Expand the configured database path and ensure its parent dir
    ///    exists.
    /// 2. Open the SQLite pool and run embedded migrations on the writer
    ///    connection.
    /// 3. Load the master encryption key from the
    ///    `OPENPROXY_MASTER_KEY` env var.
    /// 4. Construct the shared HTTP client for upstream calls.
    /// 5. Materialize the registry of built-in provider adapters.
    pub fn new(config: AppConfig) -> anyhow::Result<Self> {
        let db_pool = Arc::new(init_database(&config)?);
        let mut config = config;
        let mut recording_ttl_secs = db::app_config::RECORDING_TTL_DEFAULT_SECS;
        let mut idle_chunk_retryable = db::app_config::IDLE_CHUNK_RETRYABLE_DEFAULT;
        let mut compression_mode = openproxy_compression::CompressionMode::Off;

        run_database_maintenance(
            &mut db_pool.writer(),
            &mut config,
            &mut recording_ttl_secs,
            &mut idle_chunk_retryable,
            &mut compression_mode,
        )?;

        let usage_tx = usage::init_usage_broadcast();
        let stage_tx = usage::init_stage_broadcast();
        openproxy_core::notifications::init_broadcast();

        let recording_ttl_secs_cell = Arc::new(RwLock::new(recording_ttl_secs));
        let idle_chunk_retryable_cell = Arc::new(AtomicBool::new(idle_chunk_retryable));
        let compression_mode_cell = Arc::new(RwLock::new(compression_mode));
        let quota_protection_cell = Arc::new(RwLock::new(config.quota_protection.clone()));

        let master_key = Arc::new(MasterKey::from_env()?);
        let initial_adapters = Self::load_adapters(&db_pool)?;
        let adapters: Arc<RwLock<Arc<Vec<adapters::ProviderAdapterEnum>>>> =
            Arc::new(RwLock::new(Arc::new(initial_adapters)));

        let maintenance_cell = Arc::new(RwLock::new(config.storage.maintenance.clone()));
        let vacuum_status = Arc::new(RwLock::new(VacuumStatus::default()));
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
        spawn_memory_cleanup(
            &supervisor,
            Arc::clone(&selection_registry),
            circuit_breaker.clone(),
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
            record_bodies_and_headers: Arc::new(AtomicBool::new(false)),
            timeouts_cell: Arc::new(RwLock::new(timeouts_initial)),
            compression_mode_cell,
            recording_ttl_secs_cell,
            discovery_scheduler,
            oauth_provider_registry,
            idle_chunk_retryable_cell,
            quota_protection_cell,
            selection_registry,
            circuit_breaker,
            predictive_limiter,
            maintenance_cell,
            vacuum_status,
            background_tx,
            supervisor,
            api_key_cache: Arc::new(dashmap::DashMap::new()),
        };

        Ok(state)
    }

    /// Build a minimal `AppState` suitable for tests.
    ///
    /// This skips every side-effect of `new` (env-var master key,
    /// file-backed SQLite, OAuth scheduler, cooldown pruner, seed
    /// rows, etc.) and gives the caller direct control over the
    /// bits tests need to vary. The caller is responsible for
    /// running migrations on `db_pool` before calling this.
    ///
    /// The discovery scheduler is started with a 1-hour cadence
    /// and a 0-second initial stagger so the loop fires its first
    /// tick immediately at boot. Tests that want to drive the
    /// scheduler's tick cadence construct their own scheduler
    /// directly via [`openproxy_core::discovery_scheduler::start`]
    /// rather than going through `for_test`.
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
        spawn_memory_cleanup(
            &supervisor,
            Arc::clone(&selection_registry),
            circuit_breaker.clone(),
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
            selection_registry,
            circuit_breaker,
            predictive_limiter,
            maintenance_cell,
            vacuum_status,
            background_tx,
            supervisor,
            api_key_cache: Arc::new(dashmap::DashMap::new()),
        }
    }

    /// Retrieve an active API key from the fast in-memory cache if not expired.
    pub fn get_cached_api_key(&self, key_hash: &str) -> Option<Arc<openproxy_core::api_keys::ApiKey>> {
        let entry = self.api_key_cache.get(key_hash)?;
        if std::time::Instant::now() < entry.1 {
            Some(Arc::clone(&entry.0))
        } else {
            None
        }
    }

    /// Insert or refresh a validated API key in the fast in-memory cache (60s TTL).
    pub fn cache_api_key(&self, key: Arc<openproxy_core::api_keys::ApiKey>) {
        let now = std::time::Instant::now();
        let ttl = std::time::Duration::from_secs(60);
        self.api_key_cache
            .insert(key.key_hash.clone(), (key, now + ttl));
    }

    /// Invalidate one or all cached API keys upon mutations (create/update/revoke/delete).
    pub fn invalidate_api_key_cache(&self, key_hash: Option<&str>) {
        if let Some(hash) = key_hash {
            self.api_key_cache.remove(hash);
        } else {
            self.api_key_cache.clear();
        }
    }

    /// Borrow application services.
    pub fn services(&self) -> &crate::services::Services {
        &self.services
    }

    /// Borrow the parsed configuration.
    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    /// Borrow the SQLite connection pool.
    pub fn db_pool(&self) -> &Arc<db::DbPool> {
        &self.db_pool
    }

    /// Borrow the per-key rate limiter.
    pub fn rate_limiter(&self) -> &(dyn openproxy_core::rate_limit::RateLimiter + 'static) {
        self.rate_limiter.as_ref()
    }

    /// Borrow the master encryption key.
    pub fn master_key(&self) -> &Arc<MasterKey> {
        &self.master_key
    }

    /// Snapshot the registry of provider adapters.
    ///
    /// Returns a freshly-cloned `Vec<crate::adapters::ProviderAdapterEnum>` so
    /// callers (the chat pipeline, the admin handlers) see a
    /// self-consistent view of the registry at the moment they take
    /// the snapshot, even if another thread is hot-reloading the
    /// registry via [`Self::rebuild_adapters`] concurrently. The
    /// `Arc::clone` inside the `Vec` is the only allocation —
    /// pipelines already pay this when constructing
    /// `PipelineConfig`.
    pub fn adapters(&self) -> Arc<Vec<adapters::ProviderAdapterEnum>> {
        Arc::clone(&self.adapters.read())
    }

    /// Rebuild the in-memory adapter registry from scratch.
    ///
    /// Walks the `providers` table, adds a `CustomAdapter` for every
    /// non-builtin row, and swaps the result into the
    /// `Arc<RwLock<Vec<...>>>` slot atomically. Built-in adapters
    /// are always re-added at the front of the list (matching
    /// startup ordering).
    ///
    /// Called once at startup from `AppState::new`, and from the
    /// admin handlers after a successful create / update / delete
    /// on a custom provider so the chat pipeline can dispatch to
    /// newly-registered providers without a process restart.
    ///
    /// # Errors
    ///
    /// Returns `Err(CoreError::Internal)` if reading the provider
    /// list from the DB fails. Admin callers log and continue
    /// (the DB write has already committed; a future admin action
    /// will retry the reload) rather than failing the request.
    pub fn rebuild_adapters(&self) -> Result<(), openproxy_types::CoreError> {
        let new_adapters = Self::load_adapters(&self.db_pool)?;
        *self.adapters.write() = Arc::new(new_adapters);
        Ok(())
    }

    fn load_adapters(
        db_pool: &Arc<db::DbPool>,
    ) -> Result<Vec<adapters::ProviderAdapterEnum>, openproxy_types::CoreError> {
        let mut new_adapters: Vec<adapters::ProviderAdapterEnum> = adapters::builtin_adapters();
        let all_providers = {
            let w = db_pool.writer();
            openproxy_core::providers::list(&w).map_err(|e| {
                openproxy_types::CoreError::Internal(format!(
                    "rebuild_adapters: list providers: {e}"
                ))
            })
        }?;
        for p in &all_providers {
            if !openproxy_core::seed::is_builtin(p.id.as_str()) {
                new_adapters.push(adapters::ProviderAdapterEnum::Custom(Box::new(
                    adapters::CustomAdapter::from_provider_row(p),
                )));
            }
        }
        Ok(new_adapters)
    }

    /// Borrow the shared hyper-based upstream client used by the
    /// new `UpstreamClient::call()` path. Returns a reference to
    /// the `Arc<UpstreamClient>` so callers (the kiro and
    /// antigravity executors in particular) can take a cheap
    /// `Arc` clone of the same underlying client. The returned
    /// reference is tied to `&self`, but the `Arc` is cheap to
    /// clone and outlives any subsequent `set_timeouts` call
    /// (the upstream client does not need to be hot-swapped: its
    /// per-request timeouts are baked into the hyper client at
    /// build time, and the chat pipeline enforces the rest of
    /// the timeout budget on its own).
    pub fn upstream_client(&self) -> &Arc<UpstreamClient> {
        &self.upstream_client
    }

    /// Return a clone of the OAuth provider registry (cheap —
    /// internally `Arc`-backed).
    pub fn oauth_provider_registry(&self) -> Arc<oauth::OAuthProviderRegistry> {
        Arc::clone(&self.oauth_provider_registry)
    }

    /// Borrow the background services supervisor.
    pub fn supervisor(&self) -> &Arc<crate::background::BackgroundSupervisor> {
        &self.supervisor
    }

    /// Borrow the background discovery scheduler handle.
    ///
    /// No call site reads the scheduler today (Gate B / read side
    /// hasn't landed), but the handle must stay alive on
    /// `AppState` for the process lifetime: dropping it would NOT
    /// cancel the per-provider refresh tasks (the scheduler owns
    /// the parent `CancellationToken`), but the field is the only
    /// path a future `Drop` impl or admin endpoint has to call
    /// `.cancel()`. Exposed as a public accessor so the field is
    /// considered live by the compiler without an `#[allow]`.
    pub fn discovery_scheduler(&self) -> &Arc<DiscoveryScheduler> {
        &self.discovery_scheduler
    }

    /// Borrow the usage broadcast sender.
    pub fn usage_tx(
        &self,
    ) -> tokio::sync::broadcast::Sender<openproxy_types::usage::RecentUsageRow> {
        tokio::sync::broadcast::Sender::clone(&self.usage_tx)
    }

    /// Borrow the stage broadcast sender. The live-log dashboard
    /// subscribes to this in addition to `usage_tx` so it can show
    /// the operator each request's progress through
    /// `started → connecting → waiting_ttft → streaming → completed`
    /// in real time.
    pub fn stage_tx(&self) -> tokio::sync::broadcast::Sender<openproxy_types::usage::StageEvent> {
        tokio::sync::broadcast::Sender::clone(&self.stage_tx)
    }

    /// Return a clone of the shared recording flag. The chat handler
    /// passes this into every `Pipeline` it builds so the toggle is
    /// visible to all in-flight requests.
    pub fn record_bodies_and_flags(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.record_bodies_and_headers)
    }

    /// Read the current recording state.
    pub fn is_recording(&self) -> bool {
        self.record_bodies_and_headers
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Flip the recording state. When `true`, every new pipeline call
    /// will record the full request/response bodies and headers in
    /// the `usage` table.
    pub fn set_recording(&self, enabled: bool) {
        self.record_bodies_and_headers
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
    }

    /// Read the current recording body TTL in seconds.
    pub fn recording_ttl_secs(&self) -> i64 {
        *self.recording_ttl_secs_cell.read()
    }

    /// Update the recording body TTL in seconds.
    pub fn set_recording_ttl_secs(&self, secs: i64) {
        *self.recording_ttl_secs_cell.write() = secs;
    }

    /// Read the live [`TimeoutsConfig`]. Returns a `Copy` of the 5-u64
    /// struct, so it's cheap and safe to call from any handler.
    /// The read lock is held for the duration of the `Copy` and
    /// released before the function returns.
    ///
    /// This is the value used by the chat pipeline and the watchdog
    /// (see `chat.rs`). It may differ from `config().timeouts` after a
    /// `PUT /admin/config/timeouts` — `config()` is the startup
    /// snapshot, this one is the live one.
    pub fn timeouts(&self) -> openproxy_types::config::TimeoutsConfig {
        *self.timeouts_cell.read()
    }

    /// Return the current compression mode (hot-swappable).
    pub fn compression_mode(&self) -> openproxy_compression::CompressionMode {
        *self.compression_mode_cell.read()
    }

    /// Replace the live compression mode. Called by
    /// `PUT /admin/config/compression` after the DB UPSERT.
    pub fn set_compression_mode(&self, mode: openproxy_compression::CompressionMode) {
        *self.compression_mode_cell.write() = mode;
    }

    /// Replace the live [`TimeoutsConfig`]. Called by the
    /// `PUT /admin/config/timeouts` handler *after* the DB UPSERT
    /// has succeeded. Takes the write lock briefly; readers see the
    /// new value as soon as this returns.
    pub fn set_timeouts(&self, t: openproxy_types::config::TimeoutsConfig) {
        let mut cell = self.timeouts_cell.write();
        *cell = t;
    }

    /// Read the current `idle_chunk_retryable` flag (hot-swappable).
    pub fn idle_chunk_retryable(&self) -> bool {
        self.idle_chunk_retryable_cell
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Replace the live `idle_chunk_retryable` flag. Called by the
    /// admin PUT endpoint after the DB UPSERT.
    pub fn set_idle_chunk_retryable(&self, val: bool) {
        self.idle_chunk_retryable_cell
            .store(val, std::sync::atomic::Ordering::Relaxed);
    }

    /// Read the current live `quota_protection` configuration.
    pub fn quota_protection(&self) -> openproxy_types::config::QuotaProtectionConfig {
        self.quota_protection_cell.read().clone()
    }

    /// Replace the live `quota_protection` configuration. Called by the
    /// admin PUT endpoint after the DB UPSERT.
    pub fn set_quota_protection(&self, config: openproxy_types::config::QuotaProtectionConfig) {
        *self.quota_protection_cell.write() = config;
    }

    /// Return a clone of the shared selection registry. The chat
    /// handler passes this into every `Pipeline` it builds via
    /// [`openproxy_pipeline::Pipeline::with_selection_registry`]
    /// so the LKGP / least_used / p2c priority modes share state
    /// across all in-flight requests.
    pub fn selection_registry(&self) -> Arc<openproxy_types::SelectionRegistry> {
        Arc::clone(&self.selection_registry)
    }

    /// Borrow the shared circuit breaker registry. Cloned into every
    /// per-request `Pipeline` so failures in one request affect the
    /// next. The registry is `Clone` (inner `Arc<Mutex<...>>`), so
    /// the clone shares the same underlying map.
    pub fn circuit_breaker(&self) -> openproxy_pipeline::circuit_breaker::CircuitBreakerRegistry {
        self.circuit_breaker.clone()
    }

    pub fn predictive_limiter(&self) -> Arc<openproxy_pipeline::PredictiveRateLimiter> {
        Arc::clone(&self.predictive_limiter)
    }

    pub fn background_tx(
        &self,
    ) -> tokio::sync::mpsc::Sender<openproxy_pipeline::worker::BackgroundJob> {
        tokio::sync::mpsc::Sender::clone(&self.background_tx)
    }

    /// Read the current maintenance config (auto_vacuum, interval,
    /// retention). Returns a clone so callers don't hold the lock.
    pub fn maintenance_config(&self) -> openproxy_types::config::MaintenanceConfig {
        self.maintenance_cell.read().clone()
    }

    /// Update the maintenance config at runtime. Persists to the
    /// in-memory cell; the background task picks up the new values
    /// on its next tick.
    pub fn set_maintenance_config(&self, cfg: openproxy_types::config::MaintenanceConfig) {
        *self.maintenance_cell.write() = cfg;
    }

    /// Read the current VACUUM status (last_run, in_progress, etc.)
    /// for the dashboard's config view.
    pub fn vacuum_status(&self) -> VacuumStatus {
        self.vacuum_status.read().clone()
    }

    /// Mark VACUUM as in-progress (called by the manual vacuum endpoint).
    pub fn set_vacuum_in_progress(&self, in_progress: bool) {
        self.vacuum_status.write().in_progress = in_progress;
    }

    /// Record the result of a VACUUM run (called by both the manual
    /// endpoint and the background task).
    pub fn record_vacuum_result(&self, result: &str) {
        let mut st = self.vacuum_status.write();
        st.in_progress = false;
        st.last_run = Some(chrono::Utc::now().to_rfc3339());
        st.last_result = Some(result.to_string());
    }
}

// ── Private helpers for construction and background tasks ───────────

fn init_database(config: &openproxy_core::AppConfig) -> anyhow::Result<openproxy_db::DbPool> {
    let path = config.expanded_database_path();
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    Ok(openproxy_db::DbPool::open(&path)?)
}

fn run_database_maintenance(
    w: &mut openproxy_db::conn::WriterGuard<'_>,
    config: &mut openproxy_core::AppConfig,
    recording_ttl_secs: &mut i64,
    idle_chunk_retryable: &mut bool,
    compression_mode: &mut openproxy_compression::CompressionMode,
) -> anyhow::Result<()> {
    openproxy_db::migrations::run(w)?;
    load_persisted_config_overrides(
        w,
        config,
        recording_ttl_secs,
        idle_chunk_retryable,
        compression_mode,
    )?;
    seed_and_backfill_database(w)?;
    Ok(())
}

fn load_persisted_timeouts(
    w: &openproxy_db::conn::WriterGuard<'_>,
    config: &mut openproxy_core::AppConfig,
) -> anyhow::Result<()> {
    if let Some(override_cfg) = openproxy_db::app_config::load_timeouts_override_from_db(w)? {
        tracing::info!(
            connect_ms = override_cfg.connect_ms,
            request_send_ms = override_cfg.request_send_ms,
            ttft_ms = override_cfg.ttft_ms,
            idle_chunk_ms = override_cfg.idle_chunk_ms,
            total_ms = override_cfg.total_ms,
            "loaded persisted timeouts override from app_config"
        );
        config.timeouts = override_cfg;
    }
    Ok(())
}

fn load_persisted_runtime_flags(
    w: &openproxy_db::conn::WriterGuard<'_>,
    recording_ttl_secs: &mut i64,
    idle_chunk_retryable: &mut bool,
) -> anyhow::Result<()> {
    if let Some(ttl) = openproxy_db::app_config::load_recording_ttl_from_db(w)? {
        *recording_ttl_secs = ttl;
    }
    tracing::info!(
        recording_ttl_secs = *recording_ttl_secs,
        "loaded recording TTL from app_config (default 300s)"
    );

    if let Some(val) = openproxy_db::app_config::load_idle_chunk_retryable_from_db(w)? {
        *idle_chunk_retryable = val;
    }
    tracing::info!(
        idle_chunk_retryable = *idle_chunk_retryable,
        "loaded idle_chunk_retryable from app_config (default false)"
    );
    Ok(())
}

fn load_persisted_compression_and_quota(
    w: &openproxy_db::conn::WriterGuard<'_>,
    config: &mut openproxy_core::AppConfig,
    compression_mode: &mut openproxy_compression::CompressionMode,
) -> anyhow::Result<()> {
    if let Some(mode) = openproxy_db::app_config::load_compression_override_from_db(w)? {
        tracing::info!(
            ?mode,
            "loaded persisted compression override from app_config"
        );
        *compression_mode = mode;
    } else {
        tracing::info!(
            ?compression_mode,
            "no persisted compression override; using config default"
        );
    }

    if let Some(quota_cfg) = openproxy_db::app_config::load_quota_protection_override_from_db(w)? {
        tracing::info!(
            enabled = quota_cfg.enabled,
            threshold_percentage = quota_cfg.threshold_percentage,
            "loaded persisted quota_protection override from app_config"
        );
        config.quota_protection = quota_cfg;
    }
    Ok(())
}

fn load_persisted_config_overrides(
    w: &openproxy_db::conn::WriterGuard<'_>,
    config: &mut openproxy_core::AppConfig,
    recording_ttl_secs: &mut i64,
    idle_chunk_retryable: &mut bool,
    compression_mode: &mut openproxy_compression::CompressionMode,
) -> anyhow::Result<()> {
    load_persisted_timeouts(w, config)?;
    load_persisted_runtime_flags(w, recording_ttl_secs, idle_chunk_retryable)?;
    load_persisted_compression_and_quota(w, config, compression_mode)?;
    Ok(())
}

fn run_provider_and_combo_seeding(w: &openproxy_db::conn::WriterGuard<'_>) -> anyhow::Result<()> {
    let seeded = openproxy_core::seed::seed_builtin_providers(w)?;
    if seeded > 0 {
        tracing::info!(seeded, "auto-seeded built-in providers on first start");
    }

    if openproxy_core::seed::seed_virtual_combo_provider(w)? {
        tracing::info!("auto-seeded virtual 'combo' provider for sub-combo targets");
    }
    Ok(())
}

fn run_model_and_usage_backfills(w: &openproxy_db::conn::WriterGuard<'_>) -> anyhow::Result<()> {
    let backfilled = openproxy_core::seed::backfill_model_metadata(w)?;
    if backfilled > 0 {
        tracing::info!(
            backfilled,
            "backfilled model metadata from heuristics on first start"
        );
    }

    let normalized = openproxy_core::models_dev_sync::backfill_model_id_normalized(w)?;
    if normalized > 0 {
        tracing::info!(
            normalized,
            "backfilled model_id_normalized for existing model rows on boot"
        );
    }

    let repriced = openproxy_core::models_dev_sync::recompute_costs(w)?;
    if repriced > 0 {
        tracing::info!(
            repriced,
            "re-priced historical usage rows with missing pricing on boot"
        );
    }
    Ok(())
}

fn ensure_bootstrap_key_logged(w: &openproxy_db::conn::WriterGuard<'_>) -> anyhow::Result<()> {
    if let Some(b) = openproxy_core::bootstrap::ensure_bootstrap_key(w, "bootstrap")? {
        tracing::info!(
            id = b.id.0,
            prefix = ?b.key_prefix,
            "bootstrap key ready (see WARN log / stderr for plaintext)"
        );
    }
    Ok(())
}

fn seed_and_backfill_database(w: &openproxy_db::conn::WriterGuard<'_>) -> anyhow::Result<()> {
    run_provider_and_combo_seeding(w)?;
    run_model_and_usage_backfills(w)?;
    ensure_bootstrap_key_logged(w)?;
    Ok(())
}

struct SpawnBackgroundTasksArgs {
    db_pool: Arc<openproxy_db::DbPool>,
    config: openproxy_core::AppConfig,
    recording_ttl_secs_cell: Arc<RwLock<i64>>,
    maintenance_cell: Arc<RwLock<openproxy_types::config::MaintenanceConfig>>,
    vacuum_status: Arc<RwLock<crate::state::VacuumStatus>>,
    master_key: Arc<openproxy_db::secrets::MasterKey>,
    adapters: Arc<RwLock<Arc<Vec<openproxy_adapters::adapters::ProviderAdapterEnum>>>>,
    upstream_client: Arc<openproxy_adapters::upstream::UpstreamClient>,
    oauth_provider_registry: Arc<openproxy_core::oauth::OAuthProviderRegistry>,
}

fn spawn_background_tasks(
    supervisor: &crate::background::BackgroundSupervisor,
    args: SpawnBackgroundTasksArgs,
) {
    let SpawnBackgroundTasksArgs {
        db_pool,
        config,
        recording_ttl_secs_cell,
        maintenance_cell,
        vacuum_status,
        master_key,
        adapters,
        upstream_client,
        oauth_provider_registry,
    } = args;

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

fn start_discovery_scheduler(
    db_pool: Arc<openproxy_db::DbPool>,
    master_key: Arc<openproxy_db::secrets::MasterKey>,
    adapters: &Arc<RwLock<Arc<Vec<openproxy_adapters::adapters::ProviderAdapterEnum>>>>,
    upstream_client: Arc<openproxy_adapters::upstream::UpstreamClient>,
) -> openproxy_core::discovery_scheduler::DiscoveryScheduler {
    let adapters_clone = Arc::clone(&adapters.read());
    openproxy_core::discovery_scheduler::start(
        db_pool,
        master_key,
        adapters_clone,
        upstream_client,
        openproxy_core::discovery_scheduler::DiscoverySchedulerConfig::default(),
    )
}

fn spawn_rate_limiter_cleanup(
    supervisor: &crate::background::BackgroundSupervisor,
    rate_limiter: Arc<dyn openproxy_core::rate_limit::RateLimiter>,
) {
    supervisor.spawn(crate::background::RateLimiterCleanupService {
        rate_limiter,
        interval: std::time::Duration::from_mins(5),
    });
}

fn spawn_memory_cleanup(
    supervisor: &crate::background::BackgroundSupervisor,
    selection_registry: Arc<openproxy_types::SelectionRegistry>,
    circuit_breaker: openproxy_pipeline::circuit_breaker::CircuitBreakerRegistry,
) {
    supervisor.spawn(crate::background::MemoryCleanupService {
        selection_registry,
        circuit_breaker,
    });
}

#[cfg(test)]
mod tests {
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
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!("openproxy-state-test-{pid}-{nanos}-{n}"));
        std::fs::create_dir_all(&dir).expect("mkdir tempdir");
        let path = dir.join("state.db");
        let pool = core_db::DbPool::open(&path).expect("open pool");
        {
            let mut w = pool.writer();
            core_db::migrations::run(&mut w).expect("migrations");
        }
        (pool, path)
    }

    /// Build a minimal `AppState` mirroring the test helper in
    /// `crates/openproxy-server/src/handlers/admin.rs::tests::make_state_with_key`.
    async fn make_state() -> AppState {
        let (pool, _path) = fresh_pool();
        let db_pool = Arc::new(pool);
        // MasterKey::generate() returns a fresh 32-byte key — safe
        // for tests that don't decrypt any real secrets.
        let master_key = Arc::new(MasterKey::generate());
        // Start with an empty adapter registry; `rebuild_adapters`
        // is responsible for filling in both the built-ins and any
        // custom rows.
        let adapters = Arc::new(RwLock::new(Arc::new(
            Vec::<adapters::ProviderAdapterEnum>::new(),
        )));
        AppState::for_test(AppConfig::default(), db_pool, master_key, adapters)
    }

    /// Regression test for the frozen-registry bug.
    ///
    /// 1. Build an `AppState` with an empty custom-provider table.
    ///    After the initial `rebuild_adapters` the registry must
    ///    contain only built-ins.
    /// 2. Insert a custom provider via `providers::create`.
    /// 3. Call `rebuild_adapters` again — the registry must now
    ///    contain a `CustomAdapter` whose `id()` matches the new
    ///    provider id, so a chat request targeting it will dispatch
    ///    without a process restart.
    #[tokio::test]
    async fn rebuild_adapters_registers_custom_provider() {
        let state = make_state().await;

        // 1. Empty DB → registry should contain only built-ins.
        state.rebuild_adapters().expect("first rebuild");
        let initial_ids: Vec<String> = state
            .adapters()
            .iter()
            .map(|a| a.id().as_str().to_string())
            .collect();
        assert!(
            initial_ids.iter().any(|id| id == "openrouter"),
            "openrouter built-in must be present after first rebuild: {initial_ids:?}"
        );

        // 2. Insert a custom provider via the same helper the admin
        //    handler uses.
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

        // 3. The registry must NOT know about it yet — without the
        //    fix this is exactly the bug: the row is in the DB but
        //    the in-memory list is frozen.
        let pre_reload_ids: Vec<String> = state
            .adapters()
            .iter()
            .map(|a| a.id().as_str().to_string())
            .collect();
        assert!(
            !pre_reload_ids.iter().any(|id| id == "hot-reload-test"),
            "registry must NOT contain the new provider before rebuild: {pre_reload_ids:?}"
        );

        // 4. Hot-reload. After this the registry must contain the
        //    custom adapter.
        state.rebuild_adapters().expect("second rebuild");
        let post_reload_ids: Vec<String> = state
            .adapters()
            .iter()
            .map(|a| a.id().as_str().to_string())
            .collect();
        assert!(
            post_reload_ids.iter().any(|id| id == "hot-reload-test"),
            "registry MUST contain the new provider after rebuild: {post_reload_ids:?}"
        );
        // Built-ins must still be there.
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
        state.rebuild_adapters().expect("rebuild after create");
        let ids_after_create: Vec<String> = state
            .adapters()
            .iter()
            .map(|a| a.id().as_str().to_string())
            .collect();
        assert!(
            ids_after_create.iter().any(|id| id == "will-be-deleted"),
            "sanity: id present after create+rebuild"
        );

        // Delete the row (admin::delete_provider rejects built-ins;
        // a custom id skips that guard), then rebuild again.
        {
            let w = state.db_pool().writer();
            providers::delete(&w, &custom_id).expect("delete custom provider");
        }
        state.rebuild_adapters().expect("rebuild after delete");
        let ids_after_delete: Vec<String> = state
            .adapters()
            .iter()
            .map(|a| a.id().as_str().to_string())
            .collect();
        assert!(
            !ids_after_delete.iter().any(|id| id == "will-be-deleted"),
            "deleted provider must be gone from registry after rebuild: {ids_after_delete:?}"
        );
        // Built-ins untouched.
        assert!(
            ids_after_delete.iter().any(|id| id == "openrouter"),
            "built-ins must survive a rebuild that removed a custom adapter"
        );
    }
}
