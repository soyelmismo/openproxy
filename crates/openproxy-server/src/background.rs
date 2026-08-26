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
                    let w = self.db_pool.writer();
                    let _ = openproxy_pipeline::repository::prune_expired_cooldowns(&w);
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
                    let _ = openproxy_core::usage::prune_expired_recording_bodies(
                        &self.db_pool.writer(),
                        ttl,
                    );
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

/// Periodically performs memory allocator trimming and cache eviction.
pub struct MemoryCleanupService {
    pub selection_registry: Arc<openproxy_types::SelectionRegistry>,
    pub circuit_breaker: openproxy_pipeline::circuit_breaker::CircuitBreakerRegistry,
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
                    if slow_counter.is_multiple_of(10) {
                        let _ = self.selection_registry.prune_stale(Duration::from_hours(1));
                        let _ = self.circuit_breaker.prune_idle(Duration::from_hours(1));
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
                    prune_usage_and_dead_proxies(&self.db_pool, retention_days);
                    let interval_ticks = interval_hours.max(1);
                    vacuum_counter = vacuum_counter.wrapping_add(1);
                    if auto_vacuum && vacuum_counter >= interval_ticks {
                        vacuum_counter = 0;
                        execute_vacuum_cycle(&self.db_pool, &self.vacuum_status, interval_hours, auto_vacuum);
                    }
                }
            }
        }
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
            tracing::info!("running scheduled background proxy sync");
            let mut next_sleep = interval_hours * 3600;

            match openproxy_core::free_proxies::sync_all_providers(Arc::clone(&self.db_pool)).await
            {
                Ok(summary) => {
                    tracing::info!(added = summary.added, "background proxy sync completed");
                    if summary.fetched == 0 {
                        tracing::warn!("0 proxies fetched, retrying in 5 minutes");
                        next_sleep = 300;
                    } else {
                        openproxy_core::free_proxies::test_all_proxies_background(Arc::clone(
                            &self.db_pool,
                        ));
                    }
                }
                Err(e) => {
                    tracing::error!("background proxy sync failed: {}", e);
                    next_sleep = 300;
                }
            }

            tokio::select! {
                () = cancel.cancelled() => break,
                () = tokio::time::sleep(Duration::from_secs(next_sleep)) => {}
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

pub(crate) fn prune_usage_and_dead_proxies(prune_pool: &openproxy_db::DbPool, retention_days: u32) {
    let retention_secs: i64 = i64::from(retention_days) * 24 * 3600;
    if retention_secs > 0 {
        let _ =
            openproxy_core::usage::prune_expired_usage_rows(&prune_pool.writer(), retention_secs);
    }
    let _ = openproxy_core::free_proxies::prune_dead_proxies(&prune_pool.writer());
}

pub(crate) fn execute_vacuum_cycle(
    prune_pool: &openproxy_db::DbPool,
    vac_status: &RwLock<crate::state::VacuumStatus>,
    interval_hours: u32,
    auto_vacuum: bool,
) {
    {
        let mut st = vac_status.write();
        st.in_progress = true;
    }
    let vacuum_result = {
        let w = prune_pool.writer();
        let _ = w.pragma_update(None, "auto_vacuum", "INCREMENTAL");
        let inc_result = w.execute_batch("PRAGMA incremental_vacuum(1000);");
        match inc_result {
            Ok(()) => Ok(()),
            Err(_) => w.execute_batch("VACUUM;"),
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
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(count.load(Ordering::SeqCst) > 0);

        // Signal graceful shutdown
        supervisor.shutdown();

        // Ensure task completes
        handle.await.expect("join handle");
        let stopped_at = count.load(Ordering::SeqCst);

        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(count.load(Ordering::SeqCst), stopped_at);
    }
}
