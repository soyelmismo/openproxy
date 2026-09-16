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

mod background;
mod builder;
mod init;
#[cfg(test)]
mod tests;

pub(crate) use init::run_boot_backfill;

use openproxy_adapters::adapters;
use openproxy_adapters::upstream::UpstreamClient;
use openproxy_core::{AppConfig, discovery_scheduler::DiscoveryScheduler, oauth};
use openproxy_db as db;
use openproxy_db::MasterKey;
use parking_lot::RwLock;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// Per-process application state.
#[derive(Clone)]
pub struct AppState {
    config: AppConfig,
    db_pool: Arc<db::DbPool>,
    master_key: Arc<MasterKey>,
    services: Arc<crate::services::Services>,
    adapters: Arc<RwLock<Arc<Vec<adapters::ProviderAdapterEnum>>>>,
    rate_limiter: Arc<dyn openproxy_core::rate_limit::RateLimiter>,
    upstream_client: Arc<UpstreamClient>,
    usage_tx: tokio::sync::broadcast::Sender<openproxy_types::usage::RecentUsageRow>,
    stage_tx: tokio::sync::broadcast::Sender<openproxy_types::usage::StageEvent>,
    models_refreshed_tx:
        tokio::sync::broadcast::Sender<openproxy_types::models::ModelsRefreshedEvent>,
    record_bodies_and_headers: Arc<AtomicBool>,
    timeouts_cell: Arc<RwLock<openproxy_types::config::TimeoutsConfig>>,
    compression_mode_cell: Arc<RwLock<openproxy_compression::CompressionMode>>,
    recording_ttl_secs_cell: Arc<RwLock<i64>>,
    discovery_scheduler: Arc<DiscoveryScheduler>,
    oauth_provider_registry: Arc<openproxy_core::oauth::OAuthProviderRegistry>,
    idle_chunk_retryable_cell: Arc<AtomicBool>,
    quota_protection_cell: Arc<parking_lot::RwLock<openproxy_types::config::QuotaProtectionConfig>>,
    pii_config_cell: Arc<parking_lot::RwLock<openproxy_types::config::PiiConfig>>,
    notifications_enabled_cell: Arc<AtomicBool>,
    selection_registry: Arc<openproxy_types::SelectionRegistry>,
    circuit_breaker: openproxy_pipeline::circuit_breaker::CircuitBreakerRegistry,
    predictive_limiter: Arc<openproxy_pipeline::PredictiveRateLimiter>,
    maintenance_cell: Arc<RwLock<openproxy_types::config::MaintenanceConfig>>,
    vacuum_status: Arc<RwLock<VacuumStatus>>,
    #[allow(dead_code)]
    backfill_status: Arc<RwLock<BackfillStatus>>,
    background_tx: tokio::sync::mpsc::Sender<openproxy_pipeline::worker::BackgroundJob>,
    supervisor: Arc<crate::background::BackgroundSupervisor>,
    api_key_cache:
        Arc<dashmap::DashMap<String, (Arc<openproxy_core::api_keys::ApiKey>, std::time::Instant)>>,
}

/// VACUUM status reported to the dashboard.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct VacuumStatus {
    pub last_run: Option<String>,
    pub last_result: Option<String>,
    pub in_progress: bool,
    pub next_scheduled: Option<String>,
}

/// Boot-time backfill status reported to the dashboard.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct BackfillStatus {
    pub in_progress: bool,
    pub last_run: Option<String>,
    pub last_result: Option<String>,
    pub last_repriced: Option<usize>,
}

impl AppState {
    /// Retrieve an active API key from the fast in-memory cache if not expired.
    pub fn get_cached_api_key(
        &self,
        key_hash: &str,
    ) -> Option<Arc<openproxy_core::api_keys::ApiKey>> {
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

    /// Prune expired entries from the API key in-memory cache.
    pub fn prune_api_key_cache(&self) -> usize {
        let now = std::time::Instant::now();
        let mut pruned = 0;
        self.api_key_cache.retain(|_, (_, exp)| {
            if now < *exp {
                true
            } else {
                pruned += 1;
                false
            }
        });
        pruned
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
    pub fn adapters(&self) -> Arc<Vec<adapters::ProviderAdapterEnum>> {
        Arc::clone(&self.adapters.read())
    }

    /// Rebuild the in-memory adapter registry from scratch.
    pub async fn rebuild_adapters(&self) -> Result<(), openproxy_types::CoreError> {
        let pool = Arc::clone(&self.db_pool);
        let new_adapters = tokio::task::spawn_blocking(move || Self::load_adapters(&pool))
            .await
            .map_err(|e| {
                openproxy_types::CoreError::Internal(format!(
                    "rebuild_adapters: spawn_blocking join: {e}"
                ))
            })??;
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

    pub fn upstream_client(&self) -> &Arc<UpstreamClient> {
        &self.upstream_client
    }

    pub fn oauth_provider_registry(&self) -> Arc<oauth::OAuthProviderRegistry> {
        Arc::clone(&self.oauth_provider_registry)
    }

    pub fn supervisor(&self) -> &Arc<crate::background::BackgroundSupervisor> {
        &self.supervisor
    }

    pub fn discovery_scheduler(&self) -> &Arc<DiscoveryScheduler> {
        &self.discovery_scheduler
    }

    pub fn usage_tx(
        &self,
    ) -> tokio::sync::broadcast::Sender<openproxy_types::usage::RecentUsageRow> {
        tokio::sync::broadcast::Sender::clone(&self.usage_tx)
    }

    pub fn stage_tx(&self) -> tokio::sync::broadcast::Sender<openproxy_types::usage::StageEvent> {
        tokio::sync::broadcast::Sender::clone(&self.stage_tx)
    }

    pub fn models_refreshed_tx(
        &self,
    ) -> tokio::sync::broadcast::Sender<openproxy_types::models::ModelsRefreshedEvent> {
        tokio::sync::broadcast::Sender::clone(&self.models_refreshed_tx)
    }

    pub fn record_bodies_and_flags(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.record_bodies_and_headers)
    }

    pub fn is_recording(&self) -> bool {
        self.record_bodies_and_headers
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn set_recording(&self, enabled: bool) {
        self.record_bodies_and_headers
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn recording_ttl_secs(&self) -> i64 {
        *self.recording_ttl_secs_cell.read()
    }

    pub fn set_recording_ttl_secs(&self, secs: i64) {
        *self.recording_ttl_secs_cell.write() = secs;
    }

    pub fn timeouts(&self) -> openproxy_types::config::TimeoutsConfig {
        *self.timeouts_cell.read()
    }

    pub fn compression_mode(&self) -> openproxy_compression::CompressionMode {
        *self.compression_mode_cell.read()
    }

    pub fn set_compression_mode(&self, mode: openproxy_compression::CompressionMode) {
        *self.compression_mode_cell.write() = mode;
    }

    pub fn set_timeouts(&self, t: openproxy_types::config::TimeoutsConfig) {
        let mut cell = self.timeouts_cell.write();
        *cell = t;
    }

    pub fn idle_chunk_retryable(&self) -> bool {
        self.idle_chunk_retryable_cell
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn set_idle_chunk_retryable(&self, val: bool) {
        self.idle_chunk_retryable_cell
            .store(val, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn quota_protection(&self) -> openproxy_types::config::QuotaProtectionConfig {
        self.quota_protection_cell.read().clone()
    }

    pub fn set_quota_protection(&self, config: openproxy_types::config::QuotaProtectionConfig) {
        *self.quota_protection_cell.write() = config;
    }

    pub fn pii_config(&self) -> openproxy_types::config::PiiConfig {
        self.pii_config_cell.read().clone()
    }

    pub fn set_pii_config(&self, mut config: openproxy_types::config::PiiConfig) {
        let mut seen = std::collections::HashSet::with_capacity(config.pii_entities.len());
        config.pii_entities.retain(|e| seen.insert(*e));
        *self.pii_config_cell.write() = config;
    }

    pub fn notifications_enabled(&self) -> bool {
        self.notifications_enabled_cell
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn set_notifications_enabled(&self, enabled: bool) {
        self.notifications_enabled_cell
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
        openproxy_core::notifications::set_enabled(enabled);
    }

    pub fn selection_registry(&self) -> Arc<openproxy_types::SelectionRegistry> {
        Arc::clone(&self.selection_registry)
    }

    pub fn circuit_breaker(&self) -> openproxy_pipeline::circuit_breaker::CircuitBreakerRegistry {
        self.circuit_breaker.clone()
    }

    pub fn backfill_status(&self) -> BackfillStatus {
        self.backfill_status.read().clone()
    }

    pub fn predictive_limiter(&self) -> Arc<openproxy_pipeline::PredictiveRateLimiter> {
        Arc::clone(&self.predictive_limiter)
    }

    pub fn background_tx(
        &self,
    ) -> tokio::sync::mpsc::Sender<openproxy_pipeline::worker::BackgroundJob> {
        tokio::sync::mpsc::Sender::clone(&self.background_tx)
    }

    pub fn maintenance_config(&self) -> openproxy_types::config::MaintenanceConfig {
        self.maintenance_cell.read().clone()
    }

    pub fn set_maintenance_config(&self, cfg: openproxy_types::config::MaintenanceConfig) {
        *self.maintenance_cell.write() = cfg;
    }

    pub fn vacuum_status(&self) -> VacuumStatus {
        self.vacuum_status.read().clone()
    }

    pub fn set_vacuum_in_progress(&self, in_progress: bool) {
        self.vacuum_status.write().in_progress = in_progress;
    }

    pub fn record_vacuum_result(&self, result: &str) {
        let mut st = self.vacuum_status.write();
        st.in_progress = false;
        st.last_run = Some(chrono::Utc::now().to_rfc3339());
        st.last_result = Some(result.to_string());
    }
}
