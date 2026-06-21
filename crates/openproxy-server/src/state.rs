//! Application state shared across all handlers.
//!
//! `AppState` is constructed once at startup and then cloned (via `Arc`
//! internally) into every axum handler. It owns:
//!
//! - The parsed [`AppConfig`] (timeouts, racing, logging, etc.).
//! - The SQLite [`DbPool`] used for all persistence.
//! - The [`MasterKey`] used to decrypt provider API keys at request time.
//! - The registry of built-in [`ProviderAdapter`]s.
//! - A shared [`reqwest::Client`] used for upstream LLM calls.
//!
//! All heavy fields are wrapped in `Arc` so handler signatures stay
//! cheap-to-clone and the type itself is `Send + Sync` by construction.

use openproxy_core::{
    adapters, db,
    discovery_scheduler::{self, DiscoveryScheduler},
    oauth,
    secrets::MasterKey,
    upstream::UpstreamClient,
    usage, AppConfig,
};
use parking_lot::RwLock;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// Per-process application state.
///
/// Cheap to clone: every field is either an `Arc` or a cheap handle
/// (`reqwest::Client` is internally `Arc`-backed, [`AppConfig`] is `Clone`).
#[derive(Clone)]
pub struct AppState {
    config: AppConfig,
    db_pool: Arc<db::DbPool>,
    master_key: Arc<MasterKey>,
    adapters: Arc<RwLock<Vec<Arc<dyn adapters::ProviderAdapter>>>>,
    /// Shared HTTP client used for upstream calls, hot-swappable so
    /// the `timeouts.connect_ms` admin update can rebuild it with a
    /// new `connect_timeout` without restarting the process. The
    /// `Arc` lets handlers hold a cheap, stable handle to "the
    /// current client" while [`Self::set_timeouts`] swaps the
    /// inner `reqwest::Client` in place; reqwest's own
    /// `Arc`-internals keep the connection pool shared across swaps
    /// of the outer wrapper.
    http_client: Arc<RwLock<reqwest::Client>>,
    /// Shared hyper-based upstream client used by the new
    /// `UpstreamClient::call()` path. The chat pipeline and the
    /// kiro/antigravity executors hold an `Arc<UpstreamClient>`
    /// clone of this; the admin `run_test_for_model` endpoint
    /// pulls it from [`Self::upstream_client`]. The legacy
    /// `reqwest::Client` above is still kept around for the OAuth
    /// flows and quota/model-refresh paths that have not yet been
    /// ported (see Gate 5 for cleanup).
    upstream_client: Arc<UpstreamClient>,
    usage_tx: tokio::sync::broadcast::Sender<usage::RecentUsageRow>,
    /// Secondary broadcast sender for in-flight stage events
    /// (`started`/`connecting`/`waiting_ttft`/`streaming`/`failed`).
    /// The live-log dashboard subscribes to both senders and
    /// multiplexes them into a single WS stream.
    stage_tx: tokio::sync::broadcast::Sender<usage::StageEvent>,
    /// Shared toggle that controls whether the pipeline records full
    /// request/response bodies and headers in the `usage` table.
    /// The chat handler passes a clone of this `Arc` into every
    /// `Pipeline` it builds so the admin endpoint can flip the
    /// state for the whole process at once.
    record_bodies_and_headers: Arc<AtomicBool>,
    /// Hot-swappable slot for [`openproxy_core::config::TimeoutsConfig`].
    /// Reads in `chat.rs` go through [`AppState::timeouts`] which
    /// copies the 5-u64 struct atomically. Writes are done by the
    /// `PUT /v1/admin/config/timeouts` handler after the DB
    /// row has been updated. See spec §5 / §7.
    timeouts_cell: Arc<RwLock<openproxy_core::config::TimeoutsConfig>>,
    /// Hot-swappable slot for [`openproxy_core::compression::CompressionMode`].
    compression_mode_cell: Arc<RwLock<openproxy_core::compression::CompressionMode>>,
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
    /// Marked `dead_code` because Gate B (read side) hasn't
    /// landed yet; suppressing the warning keeps the field
    /// discoverable for the Drop impl / future admin endpoints
    /// without sprinkling `#[allow]` on every reference.
    #[allow(dead_code)]
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
    pub async fn new(config: AppConfig) -> anyhow::Result<Self> {
        // 1. Open DB and run migrations.
        let path = config.expanded_database_path();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let db_pool = Arc::new(db::DbPool::open(&path)?);
        let mut config = config;
        let mut recording_ttl_secs = db::app_config::RECORDING_TTL_DEFAULT_SECS;
        let mut idle_chunk_retryable = db::app_config::IDLE_CHUNK_RETRYABLE_DEFAULT;
        let mut compression_mode = openproxy_core::compression::CompressionMode::Off;
        {
            let mut w = db_pool.writer();
            db::migrations::run(&mut w)?;
            // 1b. (spec §4) If a previous run persisted a `timeouts`
            //     override via the admin PUT endpoint, load it now
            //     and overwrite the TOML-derived value. The TOML
            //     value remains the fallback if the row is missing
            //     or the JSON is corrupt.
            if let Some(override_cfg) = db::app_config::load_timeouts_override_from_db(&w)? {
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
            // 1c. Load the persisted recording TTL override.
            if let Some(ttl) = db::app_config::load_recording_ttl_from_db(&w)? {
                recording_ttl_secs = ttl;
            }
            tracing::info!(
                recording_ttl_secs,
                "loaded recording TTL from app_config (default 300s)"
            );
            // 1d. Load the persisted idle_chunk_retryable override.
            if let Some(val) = db::app_config::load_idle_chunk_retryable_from_db(&w)? {
                idle_chunk_retryable = val;
            }
            tracing::info!(
                idle_chunk_retryable,
                "loaded idle_chunk_retryable from app_config (default false)"
            );
            // 1e. Load the persisted compression override. The
            //     admin PUT endpoint for compression persists the
            //     chosen mode here at runtime; if we don't load it
            //     back at boot, the in-memory `compression_mode_cell`
            //     silently defaults to `Off` and every subsequent
            //     request runs uncompressed even though the operator
            //     clearly enabled it via the dashboard. This was
            //     the half of the compression bug that made the
            //     feature invisible in the `usage` table on a
            //     server restart (the other half — dropping the
            //     stats before writing the row — lives in
            //     `pipeline.rs`). Mirrors the timeouts /
            //     recording_ttl / idle_chunk_retryable pattern
            //     immediately above.
            if let Some(mode) =
                db::app_config::load_compression_override_from_db(&w)?
            {
                tracing::info!(
                    ?mode,
                    "loaded persisted compression override from app_config"
                );
                compression_mode = mode;
            } else {
                tracing::info!(
                    ?compression_mode,
                    "no persisted compression override; \
                     using config default"
                );
            }
            // Auto-seed the three built-in providers (OpenRouter, MiniMax
            // Coding, OpenCode Zen) so the dashboard shows them on first
            // run. The seed is idempotent: existing rows are skipped.
            let seeded = openproxy_core::seed::seed_builtin_providers(&w)?;
            if seeded > 0 {
                tracing::info!(
                    seeded,
                    "auto-seeded built-in providers on first start"
                );
            }
            // Seed the virtual "combo" provider row used as a placeholder
            // `provider_id` on sub-combo (combo-in-combo) targets. Idempotent
            // and decoupled from the built-in list because there is no
            // adapter registered against this id; it exists in the
            // `providers` table only to satisfy the combo_targets FK and
            // the `p.active = 1` filter in `list_targets`.
            if openproxy_core::seed::seed_virtual_combo_provider(&w)? {
                tracing::info!("auto-seeded virtual 'combo' provider for sub-combo targets");
            }
            // Backfill model metadata (context_length, capabilities, …)
            // for any rows that pre-date migration 000014. Idempotent:
            // a second start touches zero rows. Logged so operators can
            // see the migration happened.
            let backfilled = openproxy_core::seed::backfill_model_metadata(&w)?;
            if backfilled > 0 {
                tracing::info!(
                    backfilled,
                    "backfilled model metadata from heuristics on first start"
                );
            }
            // Backfill `model_id_normalized` for existing model rows.
            // Migration 000033 added the column but left it NULL for
            // pre-existing rows. The models.dev sync enrichment and the
            // pricing lookup both depend on this column being populated.
            // Running it at boot (unconditionally, even if sync is
            // disabled) ensures the column is ready when the sync fires.
            let normalized = openproxy_core::models_dev_sync::backfill_model_id_normalized(&w)?;
            if normalized > 0 {
                tracing::info!(
                    normalized,
                    "backfilled model_id_normalized for existing model rows on boot"
                );
            }
            // Re-price historical usage rows that had no pricing at
            // record time (cost_usd = 0 AND tokens > 0). This runs at
            // boot so the operator sees correct costs immediately after
            // restart, without having to manually trigger a models.dev
            // sync. Uses whatever pricing data is already in the sync
            // table (from a previous sync) plus the static PRICING_TABLE
            // fallback. If the sync hasn't run yet, only the static
            // table entries (11 models) will be re-priced; the rest
            // will be re-priced when the sync runs and the operator
            // hits the manual recompute endpoint.
            let repriced = openproxy_core::models_dev_sync::recompute_costs(&w)?;
            if repriced > 0 {
                tracing::info!(
                    repriced,
                    "re-priced historical usage rows with missing pricing on boot"
                );
            }
            // First-run bootstrap: if the api_keys table is empty,
            // create a single `["manage", "chat"]` key and print the
            // plaintext to the logs + stderr. The operator copies it
            // out of the boot logs and uses it for everything (admin
            // calls, chat calls) until they rotate to a per-client
            // key. No-op on subsequent boots.
            if let Some(b) = openproxy_core::bootstrap::ensure_bootstrap_key(
                &w, "bootstrap"
            )? {
                tracing::info!(
                    id = b.id.0,
                    prefix = ?b.key_prefix,
                    "bootstrap key ready (see WARN log / stderr for plaintext)"
                );
            }
        }

        // 2. Master key from env.
        let master_key = Arc::new(MasterKey::from_env()?);

        // 3. HTTP client for upstream calls.
        //
        // The `connect_timeout` is wired to `timeouts.connect_ms` at
        // startup (and re-applied live by `set_timeouts` below). The
        // default `timeouts.connect_ms` is 5 s; reqwest's own default
        // is "no timeout" which leaves the TCP-connect arm of a
        // request open indefinitely. The rest of the timeout budget
        // (`request_send_ms`, `ttft_ms`, `total_ms`) is enforced
        // elsewhere: per-request via `RequestBuilder::timeout(total)`
        // in `pipeline.rs`, and `ttft` / `idle_chunk` are measured
        // by the pipeline. See the comment block above
        // `dispatch_upstream_streaming` in `pipeline.rs` for the
        // full mapping.
        let http_client = reqwest::Client::builder()
            .user_agent("openproxy/0.1")
            .connect_timeout(Duration::from_millis(
                config.timeouts.connect_ms,
            ))
            .build()?;

        // 4. Adapters — built-in + any custom providers stored in DB.
        //    The in-memory registry is wrapped in a `RwLock` so the
        //    admin handlers can hot-reload it (via `rebuild_adapters`)
        //    after create / update / delete of a custom provider.
        //    At startup we call `rebuild_adapters` once; subsequent
        //    reloads happen in the admin handler path.
        let adapters = Arc::new(RwLock::new(adapters::builtin_adapters()));

        // 5. Usage broadcast sender for admin live-log WebSockets.
        let usage_tx = usage::init_usage_broadcast();
        // 5b. Stage broadcast sender for in-flight per-phase updates.
        //     Lives in the same process but a separate channel so
        //     the dashboard can map stages to a row by `request_id`
        //     without re-deriving from a `RecentUsageRow`.
        let stage_tx = usage::init_stage_broadcast();

        // 6. Background prune of expired cooldowns. The
        //    `target_cooldowns` table is append-mostly (failures
        //    insert, successes delete, the loop's own UPSERT on a
        //    second failure just updates the existing row), but
        //    abandoned rows — a target whose combo was deleted,
        //    for example, or one that's been parked for hours —
        //    would otherwise live forever. The 60-second cadence
        //    is the same as the dashboard's poll interval, so the
        //    "⏸ cooldown" badge can't visibly outlive its row by
        //    more than a minute.
        //
        //    We spawn before returning `AppState` so the task is
        //    anchored to the tokio runtime the caller is already
        //    driving. The task holds only an `Arc<DbPool>`, so the
        //    process can shut down without an explicit cancel
        //    signal: dropping the last `DbPool` clone is enough.
        let prune_pool = db_pool.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            // First tick fires immediately; skip it so we don't
            // double-prune on a fresh boot (migrations just ran,
            // there are no expired rows yet).
            tick.tick().await;
            loop {
                tick.tick().await;
                let pruned = {
                    let w = prune_pool.writer();
                    openproxy_core::cooldown::prune_expired(&w)
                };
                match pruned {
                    Ok(0) => {}
                    Ok(n) => {
                        tracing::info!(pruned = n, "pruned expired target cooldowns");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "cooldown prune tick failed");
                    }
                }
            }
        });

        // 6b. Background prune of expired recorded request/response
        //     bodies and headers. The metadata rows stay intact for
        //     analytics, but the heavy live-log detail fields are
        //     nullified after the configured TTL.
        // 6c. Same tick: DELETE entire usage rows older than the TTL
        //     so the live-logs table does not grow indefinitely. Both
        //     prunes share the same TTL value and 60s cadence.
        let recording_ttl_secs_cell = Arc::new(RwLock::new(recording_ttl_secs));
        let recording_ttl_pool = db_pool.clone();
        let recording_ttl_cell = Arc::clone(&recording_ttl_secs_cell);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            tick.tick().await;
            loop {
                tick.tick().await;
                let ttl = *recording_ttl_cell.read();
                let pruned = {
                    let w = recording_ttl_pool.writer();
                    openproxy_core::usage::prune_expired_recording_bodies(&w, ttl)
                };
                match pruned {
                    Ok(0) => {}
                    Ok(n) => {
                        tracing::info!(pruned = n, ttl_secs = ttl, "pruned expired recording bodies");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, ttl_secs = ttl, "recording TTL prune tick failed");
                    }
                }
            }
        });

        // 7. Background OAuth refresh scheduler. Walks the
        //    `accounts` table every 60s looking for OAuth accounts
        //    whose access token expires within the next 15 minutes
        //    and proactively refreshes them. The 15-minute window
        //    is large enough that the refreshed token is in place
        //    before any in-flight chat call needs it, and small
        //    enough that the scheduler is not constantly
        //    thrashing on tokens that have years of validity
        //    remaining. Like the cooldown pruner, the task
        //    holds only an `Arc<DbPool>` and the shared
        //    `UpstreamClient` (cloned out of the `Arc` we
        //    build for `upstream_client` below).
        //
        //    Built once and shared with the discovery scheduler
        //    (step 8) so both background paths talk to upstreams
        //    through the same client. The `Arc<UpstreamClient>`
        //    field is a cheap clone of the same handle.
        let upstream_client = UpstreamClient::new();
        let refresh_pool = db_pool.clone();
        let refresh_key = master_key.clone();
        let refresh_upstream = upstream_client.clone();
        // Build the OAuth provider registry — a single, shared
        // registry used by the pipeline (for on-demand refresh
        // during chat requests), the background scheduler, and
        // the admin handlers. Built-in providers are registered
        // here; custom providers can be added at runtime via
        // `AppState::oauth_provider_registry().register()`.
        let oauth_provider_registry: Arc<oauth::OAuthProviderRegistry> =
            Arc::new(oauth::OAuthProviderRegistry::builtin());
        let scheduler_registry = oauth_provider_registry.clone();
        tokio::spawn(async move {
            oauth::start_refresh_scheduler(
                refresh_pool,
                refresh_key,
                refresh_upstream,
                scheduler_registry,
                60,    // check every 60s
                900,   // refresh tokens that expire in the next 15min
            )
            .await;
        });

        // 9. models.dev background sync (opt-in).
        //    When `MODELS_DEV_SYNC_ENABLED=true`, spawns a background
        //    task that periodically fetches model pricing, context
        //    length, and capabilities from models.dev and enriches
        //    the local `models` table + auto-creates cross-provider
        //    combos. Default interval: 24h.
        //
        //    The sync is a no-op in `for_test` mode (no env var).
        let sync_pool = db_pool.clone();
        let sync_upstream = upstream_client.clone();
        let models_dev_enabled = std::env::var("MODELS_DEV_SYNC_ENABLED")
            .ok()
            .map(|v| v == "1" || v == "true")
            .unwrap_or(false);
        if models_dev_enabled {
            let interval_secs: u64 = std::env::var("MODELS_DEV_SYNC_INTERVAL_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(86_400);
            tracing::info!(interval_secs, "starting models.dev sync scheduler");
            tokio::spawn(async move {
                openproxy_core::models_dev_sync::start_sync_scheduler(
                    sync_pool,
                    sync_upstream,
                    interval_secs,
                )
                .await;
            });
        }

        // 8. Background model discovery scheduler (Gate A).
        //    Spawns one task per built-in provider that refreshes
        //    the `models` table for that provider on a recurring
        //    interval (default 1h, staggered uniformly in
        //    [0, interval) on boot so providers don't all fire
        //    at t=0). Tasks are fire-and-forget: dropping the
        //    AppState at shutdown does NOT cancel them (they
        //    hold their own `Arc<DbPool>` + `Arc<UpstreamClient>`
        //    clones), and the spec does not require an explicit
        //    shutdown path. The returned handle is stored on
        //    AppState so a future `Drop` impl can call
        //    `.cancel()` if needed.
        let discovery_scheduler = discovery_scheduler::start(
            db_pool.clone(),
            master_key.clone(),
            Arc::new(adapters.read().clone()),
            upstream_client.clone(),
            discovery_scheduler::DiscoverySchedulerConfig::default(),
        )
        .await;
        let discovery_scheduler = Arc::new(discovery_scheduler);

        let timeouts_initial = config.timeouts; // Copy, take it before moving config.
        let state = Self {
            config,
            db_pool,
            master_key,
            adapters,
            http_client: Arc::new(RwLock::new(http_client)),
            upstream_client,
            usage_tx,
            stage_tx,
            record_bodies_and_headers: Arc::new(AtomicBool::new(false)),
            timeouts_cell: Arc::new(RwLock::new(timeouts_initial)),
            compression_mode_cell: Arc::new(RwLock::new(compression_mode)),
            recording_ttl_secs_cell: Arc::clone(&recording_ttl_secs_cell),
            discovery_scheduler,
            oauth_provider_registry,
            idle_chunk_retryable_cell: Arc::new(AtomicBool::new(idle_chunk_retryable)),
        };
        // Hot-reload custom adapters from DB so the registry is
        // complete before the first request arrives.
        state.rebuild_adapters()?;
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
    pub async fn for_test(
        config: AppConfig,
        db_pool: Arc<db::DbPool>,
        master_key: Arc<MasterKey>,
    adapters: Arc<RwLock<Vec<Arc<dyn adapters::ProviderAdapter>>>>,
    ) -> Self {
        // 60-second prune cadence matches production; the spawned
        // task holds only `Arc<DbPool>` so the test's drop of the
        // AppState at the end of the test is enough to terminate
        // it cleanly.
        let recording_ttl_secs_cell = Arc::new(RwLock::new(db::app_config::RECORDING_TTL_DEFAULT_SECS));
        let prune_pool = db_pool.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            tick.tick().await;
            loop {
                tick.tick().await;
                let _ = openproxy_core::cooldown::prune_expired(&prune_pool.writer());
            }
        });

        // Recording TTL prune for tests.
        let recording_ttl_pool = db_pool.clone();
        let recording_ttl_cell = Arc::clone(&recording_ttl_secs_cell);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            tick.tick().await;
            loop {
                tick.tick().await;
                let ttl = *recording_ttl_cell.read();
                let _ = openproxy_core::usage::prune_expired_recording_bodies(
                    &recording_ttl_pool.writer(),
                    ttl,
                );
            }
        });

        // Test-only discovery scheduler: the test path doesn't
        // need a real `UpstreamClient` because the per-provider
        // task body short-circuits on provider rows that don't
        // exist or providers with no accounts. Spinning it up
        // here keeps the field wired and matches the production
        // shape so handler tests can hit the same code path.
        let upstream_client = UpstreamClient::new();
        let discovery_scheduler = discovery_scheduler::start(
            db_pool.clone(),
            master_key.clone(),
            Arc::new(adapters.read().clone()),
            upstream_client.clone(),
            discovery_scheduler::DiscoverySchedulerConfig {
                // 1h cadence + 0 stagger = first tick is
                // immediate, subsequent ticks are well outside
                // the test's lifetime. The test never awaits
                // the second tick.
                interval_secs: 3_600,
                initial_stagger_secs: 0,
            },
        )
        .await;

        let timeouts_initial = config.timeouts; // Copy, take it before moving config.
        Self {
            config,
            db_pool,
            master_key,
            adapters,
            // Test path: still wire `connect_timeout` so unit tests
            // that exercise the HTTP path (e.g. SSE drainers) see
            // the same contract as production. We pull
            // `timeouts.connect_ms` from the config the caller
            // passed in — `TimeoutsConfig::default()` gives 5 s.
            http_client: Arc::new(RwLock::new(
                reqwest::Client::builder()
                    .user_agent("openproxy-test/0.1")
                    .connect_timeout(Duration::from_millis(
                        timeouts_initial.connect_ms,
                    ))
                    .build()
                    .expect("build test http client"),
            )),
            upstream_client,
            usage_tx: usage::init_usage_broadcast(),
            stage_tx: usage::init_stage_broadcast(),
            record_bodies_and_headers: Arc::new(AtomicBool::new(false)),
            timeouts_cell: Arc::new(RwLock::new(timeouts_initial)),
            compression_mode_cell: Arc::new(RwLock::new(openproxy_core::compression::CompressionMode::Off)),
            recording_ttl_secs_cell: Arc::clone(&recording_ttl_secs_cell),
            discovery_scheduler: Arc::new(discovery_scheduler),
            oauth_provider_registry: Arc::new(oauth::OAuthProviderRegistry::builtin()),
            idle_chunk_retryable_cell: Arc::new(AtomicBool::new(
                db::app_config::IDLE_CHUNK_RETRYABLE_DEFAULT,
            )),
        }
    }

    /// Borrow the parsed configuration.
    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    /// Borrow the SQLite connection pool.
    pub fn db_pool(&self) -> &Arc<db::DbPool> {
        &self.db_pool
    }

    /// Borrow the master encryption key.
    pub fn master_key(&self) -> &Arc<MasterKey> {
        &self.master_key
    }

    /// Snapshot the registry of provider adapters.
    ///
    /// Returns a freshly-cloned `Vec<Arc<dyn ProviderAdapter>>` so
    /// callers (the chat pipeline, the admin handlers) see a
    /// self-consistent view of the registry at the moment they take
    /// the snapshot, even if another thread is hot-reloading the
    /// registry via [`Self::rebuild_adapters`] concurrently. The
    /// `Arc::clone` inside the `Vec` is the only allocation —
    /// pipelines already pay this when constructing
    /// `PipelineConfig`.
    pub fn adapters(&self) -> Vec<Arc<dyn adapters::ProviderAdapter>> {
        self.adapters.read().clone()
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
    pub fn rebuild_adapters(&self) -> Result<(), openproxy_core::CoreError> {
        // 1. Start with the static built-in adapter set.
        let mut new_adapters: Vec<Arc<dyn adapters::ProviderAdapter>> =
            adapters::builtin_adapters();
        // 2. Layer in any custom providers the DB has.
        let all_providers = {
            let w = self.db_pool().writer();
            openproxy_core::providers::list(&w).map_err(|e| {
                openproxy_core::CoreError::Internal(format!(
                    "rebuild_adapters: list providers: {e}"
                ))
            })
        }?;
        for p in &all_providers {
            if !openproxy_core::seed::is_builtin(p.id.as_str()) {
                new_adapters
                    .push(Arc::new(adapters::CustomAdapter::from_provider_row(p)));
            }
        }
        // 3. Atomic swap into the shared slot.
        *self.adapters.write() = new_adapters;
        Ok(())
    }

    /// Borrow the shared HTTP client used for upstream calls.
    ///
    /// Returns a fresh `reqwest::Client` snapshot of the **current**
    /// client held by `AppState`. The internal state is
    /// `Arc<RwLock<reqwest::Client>>`; this function takes the
    /// read lock briefly, clones the inner `reqwest::Client`
    /// (which is itself internally `Arc`-backed and shares the
    /// connection pool with the source), and releases the lock.
    /// After the lock is released, the returned client is fully
    /// self-contained and can outlive any subsequent
    /// [`Self::set_timeouts`] swap.
    ///
    /// The chat handler constructs a fresh `Pipeline` per
    /// request, so the pipeline's `PipelineConfig.http_client`
    /// snapshot always reflects the live client at the moment
    /// the request started. In-flight pipelines keep their
    /// original `connect_timeout` until they finish — that is
    /// the correct semantics: we don't want a runtime update to
    /// abort requests that were already in flight.
    pub fn http_client(&self) -> reqwest::Client {
        reqwest::Client::clone(&self.http_client.read())
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
        self.oauth_provider_registry.clone()
    }

    /// Borrow the usage broadcast sender.
    pub fn usage_tx(&self) -> tokio::sync::broadcast::Sender<usage::RecentUsageRow> {
        self.usage_tx.clone()
    }

    /// Borrow the stage broadcast sender. The live-log dashboard
    /// subscribes to this in addition to `usage_tx` so it can show
    /// the operator each request's progress through
    /// `started → connecting → waiting_ttft → streaming → completed`
    /// in real time.
    pub fn stage_tx(&self) -> tokio::sync::broadcast::Sender<usage::StageEvent> {
        self.stage_tx.clone()
    }

    /// Return a clone of the shared recording flag. The chat handler
    /// passes this into every `Pipeline` it builds so the toggle is
    /// visible to all in-flight requests.
    pub fn record_bodies_and_flags(&self) -> Arc<AtomicBool> {
        self.record_bodies_and_headers.clone()
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
    /// `PUT /v1/admin/config/timeouts` — `config()` is the startup
    /// snapshot, this one is the live one.
    pub fn timeouts(&self) -> openproxy_core::config::TimeoutsConfig {
        *self.timeouts_cell.read()
    }

    /// Return the current compression mode (hot-swappable).
    pub fn compression_mode(&self) -> openproxy_core::compression::CompressionMode {
        *self.compression_mode_cell.read()
    }

    /// Replace the live compression mode. Called by
    /// `PUT /v1/admin/config/compression` after the DB UPSERT.
    pub fn set_compression_mode(&self, mode: openproxy_core::compression::CompressionMode) {
        *self.compression_mode_cell.write() = mode;
    }

    /// Replace the live [`TimeoutsConfig`]. Called by the
    /// `PUT /v1/admin/config/timeouts` handler *after* the DB UPSERT
    /// has succeeded. Takes the write lock briefly; readers see the
    /// new value as soon as this returns.
    ///
    /// If `connect_ms` changed we also rebuild the shared
    /// `reqwest::Client` with the new `connect_timeout`. `reqwest`
    /// 0.12 does not expose a per-request connect timeout, and
    /// `RequestBuilder` cannot mutate a `Client`'s
    /// `connect_timeout` after build, so the only correct
    /// application point is the client itself. We rebuild and
    /// swap the inner client under the same write lock used for
    /// the timeouts cell; the lock is held only for the duration
    /// of the build + swap, so the read path in
    /// [`Self::http_client`] sees a self-consistent view.
    pub fn set_timeouts(&self, t: openproxy_core::config::TimeoutsConfig) {
        let prev = *self.timeouts_cell.read();
        let mut cell = self.timeouts_cell.write();
        *cell = t;
        if prev.connect_ms != t.connect_ms {
            let new_client = reqwest::Client::builder()
                .user_agent("openproxy/0.1")
                .connect_timeout(Duration::from_millis(t.connect_ms))
                .build()
                .expect("rebuild upstream http client with new connect_timeout");
            *self.http_client.write() = new_client;
            tracing::info!(
                prev_connect_ms = prev.connect_ms,
                new_connect_ms = t.connect_ms,
                "rebuilt upstream reqwest::Client with new connect_timeout",
            );
        }
    }

    /// Read the current `idle_chunk_retryable` flag (hot-swappable).
    pub fn idle_chunk_retryable(&self) -> bool {
        self.idle_chunk_retryable_cell.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Replace the live `idle_chunk_retryable` flag. Called by the
    /// admin PUT endpoint after the DB UPSERT.
    pub fn set_idle_chunk_retryable(&self, val: bool) {
        self.idle_chunk_retryable_cell.store(val, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    //! Tests for the in-memory adapter registry hot-reload path.
    //!
    //! The regression test exercises the bug fixed by
    //! `rebuild_adapters`: prior to the fix, the registry was built
    //! once at startup and never refreshed, so a `POST
    //! /v1/admin/providers` made AFTER the server was already
    //! running inserted the row but left the in-memory adapter list
    //! stale, causing `CoreError::ProviderNotFound(<id>)` on the
    //! first chat attempt against the new provider. The fix wraps
    //! the registry in an `Arc<RwLock<Vec<...>>>` and exposes
    //! `rebuild_adapters()` so the admin handlers can refresh it.

    use super::*;
    use crate::state::AppState;
    use openproxy_core::{
        adapters,
        db as core_db,
        ids::ProviderId,
        providers,
        secrets::MasterKey,
        AppConfig,
    };
    use std::path::PathBuf;
    use std::sync::Arc;

    /// Build an in-process pool: temp dir on disk, migrations applied.
    fn fresh_pool() -> (core_db::DbPool, PathBuf) {
        static SEQ: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir()
            .join(format!("openproxy-state-test-{}-{}-{}", pid, nanos, n));
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
        let adapters = Arc::new(RwLock::new(Vec::<Arc<dyn adapters::ProviderAdapter>>::new()));
        let state = AppState::for_test(
            AppConfig::default(),
            db_pool,
            master_key,
            adapters,
        )
        .await;
        state
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
            "openrouter built-in must be present after first rebuild: {:?}",
            initial_ids
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
            "registry must NOT contain the new provider before rebuild: {:?}",
            pre_reload_ids
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
            "registry MUST contain the new provider after rebuild: {:?}",
            post_reload_ids
        );
        // Built-ins must still be there.
        assert!(
            post_reload_ids.iter().any(|id| id == "openrouter"),
            "openrouter built-in must remain after rebuild: {:?}",
            post_reload_ids
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
            "deleted provider must be gone from registry after rebuild: {:?}",
            ids_after_delete
        );
        // Built-ins untouched.
        assert!(
            ids_after_delete.iter().any(|id| id == "openrouter"),
            "built-ins must survive a rebuild that removed a custom adapter"
        );
    }
}
