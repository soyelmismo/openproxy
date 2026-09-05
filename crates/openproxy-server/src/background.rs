//! Background daemon supervision and unified lifecycle traits.

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

/// Supervisor for background services managing a shared cancellation token.
#[derive(Clone, Default)]
pub struct BackgroundSupervisor {
    cancel: CancellationToken,
}

impl BackgroundSupervisor {
    /// Create a new supervisor with an un-cancelled token.
    pub fn new() -> Self {
        Self {
            cancel: CancellationToken::new(),
        }
    }

    /// Obtain a clone of the cancellation token.
    pub fn token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Signal cancellation to all supervised background services.
    pub fn shutdown(&self) {
        self.cancel.cancel();
    }

    /// Spawn a [`BackgroundService`] under this supervisor's cancellation scope.
    pub fn spawn<S: BackgroundService>(&self, service: S) -> tokio::task::JoinHandle<()> {
        let name = service.name();
        let cancel = self.cancel.clone();
        tokio::spawn(async move {
            tracing::debug!(service = name, "background service started");
            service.run(cancel).await;
            tracing::debug!(service = name, "background service stopped");
        })
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

impl BackgroundService for MemoryCleanupService {
    fn name(&self) -> &'static str {
        "memory_cleanup"
    }

    async fn run(&self, cancel: CancellationToken) {
        let mut fast_tick = tokio::time::interval(Duration::from_mins(1));
        let mut slow_counter: u32 = 0;
        fast_tick.tick().await;
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                _ = fast_tick.tick() => {
                    unsafe {
                        libmimalloc_sys::mi_collect(true);
                    }
                    slow_counter = slow_counter.wrapping_add(1);
                    if slow_counter.is_multiple_of(5) {
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

/// Runs the boot-time backfill (provider seed, model-metadata
/// backfill, `recompute_costs`, `cost::backfill_usage_pricing`, and
/// bootstrap key creation) on a background task so the listener socket
/// can bind immediately. The first tick fires right after `spawn`, then
/// the service sleeps for `interval` between passes so pricing drift is
/// picked up over time.
///
/// Status is reported through [`crate::state::BackfillStatus`] so the
/// admin UI can show a "warming up" / "backfilling" banner while the
/// slow `backfill_usage_pricing` full-table scan runs.
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
        // First pass: fire ~immediately after the listener is bound so
        // historical usage rows get repriced before the first dashboard
        // poll. Subsequent passes run on the slow `interval` cadence.
        if self.run_one_pass().await.is_none() {
            return;
        }

        let mut tick = tokio::time::interval(self.interval);
        // Skip the immediate tick (we already ran one pass above).
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
            let w = pool.writer();
            crate::state::run_boot_backfill(&w)
        })
        .await;

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
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(Duration::from_secs(10)) => {}
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
                openproxy_core::free_proxies::test_all_proxies_background(Arc::clone(db_pool));
                interval_hours * 3600
            }
        }
        Err(e) => {
            tracing::error!("background proxy sync failed: {e}");
            300
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
        tokio::select! {
            () = cancel.cancelled() => {}
            () = openproxy_core::oauth::start_refresh_scheduler(
                Arc::clone(&self.db_pool),
                Arc::clone(&self.master_key),
                Arc::clone(&self.upstream_client),
                Arc::clone(&self.oauth_provider_registry),
                60,
            ) => {}
        }
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
        tokio::select! {
            () = cancel.cancelled() => {}
            () = openproxy_core::models_dev_sync::start_sync_scheduler(
                Arc::clone(&self.db_pool),
                Arc::clone(&self.upstream_client),
                self.interval_secs,
            ) => {}
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestCounterService {
        count: Arc<AtomicUsize>,
    }

    impl BackgroundService for TestCounterService {
        fn name(&self) -> &'static str {
            "test_counter"
        }

        async fn run(&self, cancel: CancellationToken) {
            let mut tick = tokio::time::interval(Duration::from_millis(5));
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    _ = tick.tick() => {
                        self.count.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn supervisor_spawns_and_shuts_down_service() {
        let supervisor = BackgroundSupervisor::new();
        let count = Arc::new(AtomicUsize::new(0));

        let handle = supervisor.spawn(TestCounterService {
            count: Arc::clone(&count),
        });

        // Let the service run for a short duration
        for _ in 0..20 {
            if count.load(Ordering::SeqCst) > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(count.load(Ordering::SeqCst) > 0);

        // Signal graceful shutdown
        supervisor.shutdown();

        // Ensure task completes
        handle.await.expect("join handle");
        let stopped_at = count.load(Ordering::SeqCst);

        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(count.load(Ordering::SeqCst), stopped_at);
    }

    /// Smoke test for [`BackfillService`]: a fresh empty DB should
    /// still complete one backfill pass and populate `last_run` /
    /// `last_result` so the admin UI can clear the "warming up" banner.
    /// We use a 60s interval so the service only runs one pass during
    /// the test and exits cleanly on shutdown.
    #[tokio::test]
    async fn backfill_service_completes_initial_pass_on_empty_db() {
        use openproxy_db::DbPool;

        let dir = std::env::temp_dir().join(format!(
            "openproxy-backfill-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        ));
        std::fs::create_dir_all(&dir).expect("mkdir tempdir");
        let path = dir.join("backfill.db");
        let pool = Arc::new(DbPool::open(&path).expect("open pool"));
        {
            let mut w = pool.writer();
            openproxy_db::migrations::run(&mut w).expect("migrations");
        }

        let status = Arc::new(parking_lot::RwLock::new(
            crate::state::BackfillStatus::default(),
        ));
        let supervisor = BackgroundSupervisor::new();
        let handle = supervisor.spawn(BackfillService {
            db_pool: Arc::clone(&pool),
            backfill_status: Arc::clone(&status),
            interval: Duration::from_secs(60),
        });

        // Poll the status for up to 5s waiting for the first pass to
        // finish. On an empty DB the backfill is fast (just seeding
        // built-in providers + the bootstrap key).
        let mut completed = false;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if status.read().last_run.is_some() {
                completed = true;
                break;
            }
        }
        supervisor.shutdown();
        handle.await.expect("join handle");

        let s = status.read().clone();
        assert!(completed, "backfill pass did not complete; status={s:?}");
        assert_eq!(s.last_result.as_deref(), Some("ok"));
        assert!(!s.in_progress, "status still in_progress after pass");
    }
}
