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
    adapters: Arc<Vec<Arc<dyn adapters::ProviderAdapter>>>,
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

        // 4. Adapters.
        let adapters = Arc::new(adapters::builtin_adapters());

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
        let refresh_pool = db_pool.clone();
        let refresh_key = master_key.clone();
        let refresh_upstream = UpstreamClient::new();
        // The registry of OAuth providers. For now we register the
        // built-in OAuth providers (Antigravity, Antigravity CLI,
        // Kiro); custom OAuth providers would extend this list. The
        // trait object is `Send + Sync` so the scheduler can
        // `await` its `refresh_token` method directly. The Vec is
        // moved into the spawned task; the only reference is the
        // one the task owns.
        let refresh_providers: Arc<Vec<Box<dyn oauth::OAuthProvider + Send + Sync>>> =
            Arc::new(vec![
                Box::new(openproxy_core::oauth_antigravity::AntigravityOAuthProvider::new()),
                Box::new(openproxy_core::oauth_kiro::KiroOAuthProvider::new()),
            ]);
        tokio::spawn(async move {
            oauth::start_refresh_scheduler(
                refresh_pool,
                refresh_key,
                refresh_upstream,
                refresh_providers,
                60,    // check every 60s
                900,   // refresh tokens that expire in the next 15min
            )
            .await;
        });

        let timeouts_initial = config.timeouts; // Copy, take it before moving config.
        Ok(Self {
            config,
            db_pool,
            master_key,
            adapters,
            http_client: Arc::new(RwLock::new(http_client)),
            upstream_client: UpstreamClient::new(),
            usage_tx,
            stage_tx,
            record_bodies_and_headers: Arc::new(AtomicBool::new(false)),
            timeouts_cell: Arc::new(RwLock::new(timeouts_initial)),
        })
    }

    /// Build a minimal `AppState` suitable for tests.
    ///
    /// This skips every side-effect of `new` (env-var master key,
    /// file-backed SQLite, OAuth scheduler, cooldown pruner, seed
    /// rows, etc.) and gives the caller direct control over the
    /// bits tests need to vary. The caller is responsible for
    /// running migrations on `db_pool` before calling this.
    pub fn for_test(
        config: AppConfig,
        db_pool: Arc<db::DbPool>,
        master_key: Arc<MasterKey>,
        adapters: Arc<Vec<Arc<dyn adapters::ProviderAdapter>>>,
    ) -> Self {
        // 60-second prune cadence matches production; the spawned
        // task holds only `Arc<DbPool>` so the test's drop of the
        // AppState at the end of the test is enough to terminate
        // it cleanly.
        let prune_pool = db_pool.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            tick.tick().await;
            loop {
                tick.tick().await;
                let _ = openproxy_core::cooldown::prune_expired(&prune_pool.writer());
            }
        });

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
            upstream_client: UpstreamClient::new(),
            usage_tx: usage::init_usage_broadcast(),
            stage_tx: usage::init_stage_broadcast(),
            record_bodies_and_headers: Arc::new(AtomicBool::new(false)),
            timeouts_cell: Arc::new(RwLock::new(timeouts_initial)),
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

    /// Borrow the registry of built-in provider adapters.
    pub fn adapters(&self) -> &Arc<Vec<Arc<dyn adapters::ProviderAdapter>>> {
        &self.adapters
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
}
