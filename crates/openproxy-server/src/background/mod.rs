//! Background daemon supervision and unified lifecycle traits.

use futures::FutureExt;
use openproxy_adapters::upstream::CancellationToken;
use parking_lot::RwLock;
use std::sync::Arc;
use std::time::Duration;

/// Unified trait for background daemons and services with graceful shutdown support.
pub trait BackgroundService: Send + Sync + 'static {
    /// Human-readable identifier for tracing and metrics.
    fn name(&self) -> &'static str;

    /// Runs the service until `cancel` is signaled.
    fn run(&self, cancel: CancellationToken) -> impl std::future::Future<Output = ()> + Send;
}

/// State container for registered background tasks.
struct SupervisorRegistry {
    tasks: Vec<tokio::task::JoinHandle<()>>,
    closed: bool,
}

impl SupervisorRegistry {
    fn prune_finished(&mut self) {
        self.tasks.retain_mut(|handle| {
            if handle.is_finished()
                && let Some(res) = (&mut *handle).now_or_never()
            {
                if let Err(e) = res
                    && !e.is_cancelled()
                {
                    tracing::error!("background task failed: {e}");
                }
                return false;
            }
            true
        });
    }
}

/// Supervisor for background services managing cancellation and deterministic task drain.
#[derive(Clone)]
pub struct BackgroundSupervisor {
    cancel: CancellationToken,
    registry: Arc<parking_lot::Mutex<SupervisorRegistry>>,
    drain_lock: Arc<tokio::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl Default for BackgroundSupervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl BackgroundSupervisor {
    /// Create a new supervisor with an un-cancelled token and empty task registry.
    pub fn new() -> Self {
        Self {
            cancel: CancellationToken::new(),
            registry: Arc::new(parking_lot::Mutex::new(SupervisorRegistry {
                tasks: Vec::new(),
                closed: false,
            })),
            drain_lock: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        }
    }

    /// Obtain a clone of the cancellation token.
    pub fn token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Signal cancellation to all supervised background services (synchronous API).
    /// Marks the supervisor as closed under the registry lock so subsequent spawns are rejected.
    pub fn shutdown(&self) {
        self.cancel.cancel();
        let mut reg = self.registry.lock();
        reg.closed = true;
    }

    /// Signal cancellation and await all registered background tasks until they drain.
    ///
    /// Safe against cancellation: pending tasks are taken into the async drain lock
    /// under the synchronous registry lock, and awaited in place via `drain.last_mut()`,
    /// popping each handle only upon completion. If an awaiting caller is cancelled,
    /// in-flight and pending handles remain in `drain_lock` so subsequent callers continue
    /// draining without task detachment or deadlock.
    pub async fn shutdown_and_wait(&self) {
        self.cancel.cancel();
        let mut drain = self.drain_lock.lock().await;
        {
            let mut reg = self.registry.lock();
            reg.closed = true;
            if !reg.tasks.is_empty() {
                drain.extend(std::mem::take(&mut reg.tasks));
            }
        }

        while let Some(handle) = drain.last_mut() {
            if let Err(e) = handle.await
                && !e.is_cancelled()
            {
                tracing::error!("background service task failed on shutdown: {e}");
            }
            drain.pop();
        }
    }

    fn admit(
        &self,
        name: &'static str,
        spawn_fn: impl FnOnce() -> tokio::task::JoinHandle<()>,
    ) -> bool {
        let mut reg = self.registry.lock();
        reg.prune_finished();
        if reg.closed || self.cancel.is_cancelled() {
            tracing::warn!(service = name, "supervisor is closed; rejecting new task");
            return false;
        }
        let handle = spawn_fn();
        reg.tasks.push(handle);
        true
    }

    /// Spawn a [`BackgroundService`] under this supervisor's cancellation scope.
    /// Returns `true` if admitted, or `false` if rejected because the supervisor is closed.
    pub fn spawn<S: BackgroundService>(&self, service: S) -> bool {
        let name = service.name();
        let cancel = self.cancel.clone();
        self.admit(name, move || {
            tokio::spawn(async move {
                tracing::debug!(service = name, "background service started");
                service.run(cancel).await;
                tracing::debug!(service = name, "background service stopped");
            })
        })
    }

    /// Spawn a one-shot background task under this supervisor.
    /// Admitted work runs to completion, including during shutdown; it is not
    /// interrupted by cancellation and remains owned until drained or reaped.
    /// Returns `true` if admitted, or `false` if rejected because the supervisor is closed.
    pub fn spawn_one_shot(
        &self,
        name: &'static str,
        future: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> bool {
        self.admit(name, move || {
            tokio::spawn(async move {
                tracing::debug!(task = name, "one-shot background task started");
                future.await;
                tracing::debug!(task = name, "one-shot background task finished");
            })
        })
    }

    /// Count of currently registered tasks (pruning finished tasks first).
    #[cfg(test)]
    pub(crate) fn task_count(&self) -> usize {
        let mut reg = self.registry.lock();
        reg.prune_finished();
        reg.tasks.len()
    }
}

/// Periodically prunes expired combo target cooldowns.
pub struct CooldownPrunerService {
    pub db_pool: Arc<openproxy_db::DbPool>,
    pub interval: Duration,
}

impl BackgroundService for CooldownPrunerService {
    fn name(&self) -> &'static str {
        "cooldown_pruner"
    }

    async fn run(&self, cancel: CancellationToken) {
        let mut tick = tokio::time::interval(self.interval);
        tick.tick().await;
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                _ = tick.tick() => {
                    let pool = Arc::clone(&self.db_pool);
                    if let Err(e) =
                        tokio::task::spawn_blocking(move || {
                            let w = pool.writer();
                            let _ = openproxy_pipeline::repository::prune_expired_cooldowns(&w);
                        })
                        .await
                    {
                        tracing::warn!(service = "cooldown_pruner", "prune task join failed: {e}");
                    }
                }
            }
        }
    }
}

/// Periodically prunes expired request/response bodies and headers based on TTL.
pub struct RecordingTtlPrunerService {
    pub db_pool: Arc<openproxy_db::DbPool>,
    pub recording_ttl_secs_cell: Arc<RwLock<i64>>,
    pub interval: Duration,
}

impl BackgroundService for RecordingTtlPrunerService {
    fn name(&self) -> &'static str {
        "recording_ttl_pruner"
    }

    async fn run(&self, cancel: CancellationToken) {
        let mut tick = tokio::time::interval(self.interval);
        tick.tick().await;
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                _ = tick.tick() => {
                    let ttl = *self.recording_ttl_secs_cell.read();
                    let pool = Arc::clone(&self.db_pool);
                    if let Err(e) =
                        tokio::task::spawn_blocking(move || {
                            let _ = openproxy_core::usage::prune_expired_recording_bodies(
                                &pool.writer(),
                                ttl,
                            );
                        })
                        .await
                    {
                        tracing::warn!(service = "recording_ttl_pruner", "prune task join failed: {e}");
                    }
                }
            }
        }
    }
}

/// Periodically runs rate limiter bucket cleanup.
pub struct RateLimiterCleanupService {
    pub rate_limiter: Arc<dyn openproxy_core::rate_limit::RateLimiter>,
    pub interval: Duration,
}

impl BackgroundService for RateLimiterCleanupService {
    fn name(&self) -> &'static str {
        "rate_limiter_cleanup"
    }

    async fn run(&self, cancel: CancellationToken) {
        let mut tick = tokio::time::interval(self.interval);
        tick.tick().await;
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                _ = tick.tick() => {
                    self.rate_limiter.cleanup();
                }
            }
        }
    }
}

/// Periodically performs memory allocator trimming, SQLite shrinking and cache eviction.
pub struct MemoryCleanupService {
    pub db_pool: Arc<openproxy_db::DbPool>,
    pub selection_registry: Arc<openproxy_types::SelectionRegistry>,
    pub circuit_breaker: openproxy_pipeline::circuit_breaker::CircuitBreakerRegistry,
    pub predictive_limiter: Arc<openproxy_pipeline::PredictiveRateLimiter>,
    pub api_key_cache:
        Arc<dashmap::DashMap<String, (Arc<openproxy_core::api_keys::ApiKey>, std::time::Instant)>>,
}

impl MemoryCleanupService {
    /// Perform an allocator trim, SQLite pool memory shrink, and abandoned request sweep.
    pub async fn run_cleanup_pass(&self) {
        self.run_cleanup_pass_with(true).await;
    }

    /// Perform allocator trimming with explicit collect intensity and SQLite pool shrinking.
    pub async fn run_cleanup_pass_with(&self, force_collect: bool) {
        // Periodic sweep for abandoned requests (>5m TTL)
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        openproxy_core::usage::INFLIGHT_REGISTRY
            .retain(|_, v| now_ms.saturating_sub(v.updated_at_ms) < 300_000);

        // SQLite pool shrink on a blocking thread; mimalloc collects here.
        let pool = Arc::clone(&self.db_pool);
        let _ = tokio::task::spawn_blocking(move || {
            pool.shrink_memory();
            unsafe {
                libmimalloc_sys::mi_collect(true);
            }
        })
        .await;

        // Collect freed allocator pages on the async worker thread
        unsafe {
            libmimalloc_sys::mi_collect(force_collect);
        }
    }
}

impl BackgroundService for MemoryCleanupService {
    fn name(&self) -> &'static str {
        "memory_cleanup"
    }

    async fn run(&self, cancel: CancellationToken) {
        // Early trims at T+5s and T+8s purge startup/discovery allocations before T+10s.
        let early_5s = tokio::time::sleep(Duration::from_secs(5));
        let early_8s = tokio::time::sleep(Duration::from_secs(8));
        tokio::pin!(early_5s);
        tokio::pin!(early_8s);

        let mut ran_5s = false;
        let mut ran_8s = false;

        let mut fast_tick = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_secs(12),
            Duration::from_secs(30),
        );
        let mut slow_counter: u32 = 0;

        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                () = &mut early_5s, if !ran_5s => {
                    ran_5s = true;
                    self.run_cleanup_pass_with(true).await;
                }
                () = &mut early_8s, if !ran_8s => {
                    ran_8s = true;
                    self.run_cleanup_pass_with(true).await;
                }
                _ = fast_tick.tick() => {
                    self.run_cleanup_pass_with(true).await;

                    slow_counter = slow_counter.wrapping_add(1);
                    if slow_counter.is_multiple_of(4) {
                        let now = std::time::Instant::now();
                        self.api_key_cache.retain(|_, (_, exp)| now < *exp);
                        openproxy_adapters::adapters::antigravity::prune_plan_cache();
                        let _ = self.selection_registry.prune_stale(Duration::from_hours(1));
                        let _ = self.circuit_breaker.prune_idle(Duration::from_hours(1));
                        let _ = self.predictive_limiter.prune_stale(Duration::from_hours(1));
                        let pool_clone = Arc::clone(&self.db_pool);
                        let _ = tokio::task::spawn_blocking(move || {
                            pool_clone.shrink_memory();
                            pool_clone.checkpoint_wal();
                            unsafe {
                                libmimalloc_sys::mi_collect(true);
                            }
                        })
                        .await;
                    }
                }
            }
        }
    }
}

/// Periodically prunes historical usage rows and performs incremental/full SQLite auto-vacuum.
pub struct MaintenanceVacuumService {
    pub db_pool: Arc<openproxy_db::DbPool>,
    pub maintenance_cell: Arc<RwLock<openproxy_types::config::MaintenanceConfig>>,
    pub vacuum_status: Arc<RwLock<crate::state::VacuumStatus>>,
}

impl BackgroundService for MaintenanceVacuumService {
    fn name(&self) -> &'static str {
        "maintenance_vacuum"
    }

    async fn run(&self, cancel: CancellationToken) {
        let mut prune_tick = tokio::time::interval(Duration::from_hours(1));
        let mut vacuum_counter: u32 = 0;
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                _ = prune_tick.tick() => {
                    let (auto_vacuum, interval_hours, retention_days) = {
                        let m = self.maintenance_cell.read();
                        (
                            m.auto_vacuum,
                            (m.interval_secs / 3600) as u32,
                            m.usage_retention_days,
                        )
                    };
                    let pool = Arc::clone(&self.db_pool);
                    if let Err(e) = tokio::task::spawn_blocking(move || {
                        prune_usage_and_dead_proxies(&pool, retention_days);
                    })
                    .await
                    {
                        tracing::warn!(service = "maintenance_vacuum", "prune task join failed: {e}");
                    }
                    let interval_ticks = interval_hours.max(1);
                    vacuum_counter = vacuum_counter.wrapping_add(1);
                    if auto_vacuum && vacuum_counter >= interval_ticks {
                        vacuum_counter = 0;
                        let pool = Arc::clone(&self.db_pool);
                        let vac_status = Arc::clone(&self.vacuum_status);
                        if let Err(e) = tokio::task::spawn_blocking(move || {
                            execute_vacuum_cycle(&pool, &vac_status, interval_hours, auto_vacuum);
                        })
                        .await
                        {
                            tracing::warn!(service = "maintenance_vacuum", "vacuum task join failed: {e}");
                        }
                    }
                }
            }
        }
    }
}

/// Runs the boot-time backfill (provider seed, model-metadata backfill,
/// `recompute_costs`, `cost::backfill_usage_pricing`, bootstrap key creation) on a
/// background task so the listener socket binds immediately. The first tick fires
/// right after `spawn`, then it sleeps `interval` between passes so pricing drift
/// is picked up over time.
///
/// Status flows through [`crate::state::BackfillStatus`] so the admin UI can show
/// a "warming up" / "backfilling" banner during the `backfill_usage_pricing`
/// full-table scan.
pub struct BackfillService {
    pub db_pool: Arc<openproxy_db::DbPool>,
    pub backfill_status: Arc<parking_lot::RwLock<crate::state::BackfillStatus>>,
    pub interval: Duration,
}

impl BackgroundService for BackfillService {
    fn name(&self) -> &'static str {
        "backfill"
    }

    async fn run(&self, cancel: CancellationToken) {
        // Initial delay isolates T+1s..T+10s startup RSS window before backfill.
        let default_delay = if cfg!(test) {
            Duration::from_millis(20)
        } else {
            Duration::from_secs(18)
        };
        let initial_delay = std::env::var("OPENPROXY_BACKFILL_INITIAL_DELAY_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .map_or(default_delay, Duration::from_secs);

        tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(initial_delay) => {}
        }

        if self.run_one_pass().await.is_none() {
            return;
        }

        let mut tick = tokio::time::interval(self.interval);
        // Skip the immediate tick: one pass already ran above.
        tick.tick().await;
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                _ = tick.tick() => {
                    if self.run_one_pass().await.is_none() {
                        return;
                    }
                }
            }
        }
    }
}

impl BackfillService {
    /// Execute a single backfill pass on a blocking worker thread.
    /// Returns `None` if the service was cancelled mid-pass.
    async fn run_one_pass(&self) -> Option<()> {
        {
            let mut st = self.backfill_status.write();
            st.in_progress = true;
        }
        let pool = Arc::clone(&self.db_pool);
        let join = tokio::task::spawn_blocking(move || {
            let res = {
                let w = pool.writer();
                crate::state::run_boot_backfill(&w)
            };
            pool.shrink_memory();
            unsafe {
                libmimalloc_sys::mi_collect(true);
            }
            res
        })
        .await;

        unsafe {
            libmimalloc_sys::mi_collect(true);
        }

        let result_str;
        let mut touched = 0usize;
        match join {
            Ok(Ok(n)) => {
                touched = n;
                result_str = "ok".to_string();
                tracing::info!(touched, "boot backfill pass complete");
            }
            Ok(Err(e)) => {
                result_str = e.to_string();
                tracing::warn!(error = %e, "boot backfill pass failed");
            }
            Err(e) if e.is_cancelled() => return None,
            Err(e) => {
                result_str = e.to_string();
                tracing::warn!(error = %e, "boot backfill task join failed");
            }
        }

        let now = chrono::Utc::now().to_rfc3339();
        {
            let mut st = self.backfill_status.write();
            st.in_progress = false;
            st.last_run = Some(now);
            st.last_result = Some(result_str);
            st.last_repriced = Some(touched);
        }
        Some(())
    }
}

/// Periodically synchronizes and health-tests free public proxy lists.
pub struct FreeProxiesSyncService {
    pub db_pool: Arc<openproxy_db::DbPool>,
}

impl BackgroundService for FreeProxiesSyncService {
    fn name(&self) -> &'static str {
        "free_proxies_sync"
    }

    async fn run(&self, cancel: CancellationToken) {
        let interval_hours: u64 = std::env::var("OPENPROXY_PROXIES_SYNC_INTERVAL_HOURS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(6)
            .max(1);

        tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(Duration::from_secs(20)) => {}
        }

        loop {
            let next_sleep = sync_proxies_iteration(&self.db_pool, interval_hours).await;

            tokio::select! {
                () = cancel.cancelled() => break,
                () = tokio::time::sleep(Duration::from_secs(next_sleep)) => {}
            }
        }
    }
}

async fn sync_proxies_iteration(db_pool: &Arc<openproxy_db::DbPool>, interval_hours: u64) -> u64 {
    tracing::info!("running scheduled background proxy sync");
    match openproxy_core::free_proxies::sync_all_providers(Arc::clone(db_pool)).await {
        Ok(summary) => {
            tracing::info!(added = summary.added, "background proxy sync completed");
            if summary.fetched == 0 {
                tracing::warn!("0 proxies fetched, retrying in 5 minutes");
                300
            } else {
                openproxy_core::free_proxies::test_all_proxies(Arc::clone(db_pool)).await;
                interval_hours * 3600
            }
        }
        Err(e) => {
            tracing::error!("background proxy sync failed: {e}");
            300
        }
    }
}

/// Periodically validates and health-checks existing and candidate free proxies.
pub struct FreeProxiesValidatorService {
    pub db_pool: Arc<openproxy_db::DbPool>,
}

impl BackgroundService for FreeProxiesValidatorService {
    fn name(&self) -> &'static str {
        "free_proxies_validator"
    }

    async fn run(&self, cancel: CancellationToken) {
        let interval_secs: u64 = std::env::var("OPENPROXY_PROXIES_VALIDATION_INTERVAL_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(120)
            .max(30);

        tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(Duration::from_secs(10)) => {}
        }

        loop {
            openproxy_core::free_proxies::test_all_proxies(Arc::clone(&self.db_pool)).await;

            tokio::select! {
                () = cancel.cancelled() => break,
                () = tokio::time::sleep(Duration::from_secs(interval_secs)) => {}
            }
        }
    }
}

/// Runs OAuth refresh scheduler with cancellation support.
pub struct OAuthRefreshService {
    pub db_pool: Arc<openproxy_db::DbPool>,
    pub master_key: Arc<openproxy_db::secrets::MasterKey>,
    pub upstream_client: Arc<openproxy_adapters::upstream::UpstreamClient>,
    pub oauth_provider_registry: Arc<openproxy_core::oauth::OAuthProviderRegistry>,
}

impl BackgroundService for OAuthRefreshService {
    fn name(&self) -> &'static str {
        "oauth_refresh"
    }

    async fn run(&self, cancel: CancellationToken) {
        scheduler_services::with_core_cancellation(cancel, |core_cancel| {
            openproxy_core::oauth::run_refresh_scheduler(
                Arc::clone(&self.db_pool),
                Arc::clone(&self.master_key),
                Arc::clone(&self.upstream_client),
                Arc::clone(&self.oauth_provider_registry),
                60,
                core_cancel,
            )
        })
        .await;
    }
}

/// Runs models.dev pricing & model catalog sync scheduler.
pub struct ModelsDevSyncService {
    pub db_pool: Arc<openproxy_db::DbPool>,
    pub upstream_client: Arc<openproxy_adapters::upstream::UpstreamClient>,
    pub interval_secs: u64,
}

impl BackgroundService for ModelsDevSyncService {
    fn name(&self) -> &'static str {
        "models_dev_sync"
    }

    async fn run(&self, cancel: CancellationToken) {
        openproxy_core::models_dev_sync::run_sync_scheduler(
            Arc::clone(&self.db_pool),
            Arc::clone(&self.upstream_client),
            self.interval_secs,
            cancel,
        )
        .await;
    }
}

pub(crate) fn prune_usage_and_dead_proxies(
    prune_pool: &Arc<openproxy_db::DbPool>,
    retention_days: u32,
) {
    let retention_secs: i64 = i64::from(retention_days) * 24 * 3600;
    if retention_secs > 0 {
        if let Some(w) = prune_pool.try_writer_for(std::time::Duration::from_secs(5)) {
            let _ = openproxy_core::usage::prune_expired_usage_rows(&w, retention_secs);
        } else {
            tracing::warn!("prune_usage_and_dead_proxies: writer lock contention, skipping tick");
            return;
        }
    }
    if let Some(w) = prune_pool.try_writer_for(std::time::Duration::from_secs(5)) {
        let _ = openproxy_core::free_proxies::prune_dead_proxies(&w);
    } else {
        tracing::warn!(
            "prune_usage_and_dead_proxies: writer lock contention, skipping dead-proxy prune"
        );
    }
    // W1 retention: notifications older than 1 day are deleted only when
    // they are archived or read — unread active rows are never touched.
    // Runs on the same hourly maintenance tick as usage pruning.
    if let Some(w) = prune_pool.try_writer_for(std::time::Duration::from_secs(5)) {
        match openproxy_db::notifications::prune(&w) {
            Ok(deleted) if deleted > 0 => {
                tracing::info!(deleted, "pruned stale notifications (retention 1 day)");
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, "notification prune failed");
            }
        }
    } else {
        tracing::warn!(
            "prune_usage_and_dead_proxies: writer lock contention, skipping notification prune"
        );
    }
}

pub(crate) fn execute_vacuum_cycle(
    prune_pool: &Arc<openproxy_db::DbPool>,
    vac_status: &RwLock<crate::state::VacuumStatus>,
    interval_hours: u32,
    auto_vacuum: bool,
) {
    {
        let mut st = vac_status.write();
        st.in_progress = true;
    }
    let vacuum_result = match prune_pool.try_writer_for(std::time::Duration::from_secs(5)) {
        Some(w) => {
            let _ = w.pragma_update(None, "auto_vacuum", "INCREMENTAL");
            let inc_result = w.execute_batch("PRAGMA incremental_vacuum(1000);");
            match inc_result {
                Ok(()) => Ok(()),
                Err(_) => w.execute_batch("VACUUM;"),
            }
        }
        None => {
            tracing::warn!("execute_vacuum_cycle: writer lock contention, skipping cycle");
            Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                Some("writer lock contention".into()),
            ))
        }
    };
    let now = chrono::Utc::now().to_rfc3339();
    let result_str = match vacuum_result {
        Ok(()) => "ok".to_string(),
        Err(e) => e.to_string(),
    };
    {
        let mut st = vac_status.write();
        st.in_progress = false;
        st.last_run = Some(now);
        st.last_result = Some(result_str);
        if auto_vacuum {
            let next = chrono::Utc::now() + chrono::Duration::hours(i64::from(interval_hours));
            st.next_scheduled = Some(next.to_rfc3339());
        } else {
            st.next_scheduled = None;
        }
    }
}

pub mod scheduler_services;
pub use scheduler_services::*;

#[cfg(test)]
mod tests;
