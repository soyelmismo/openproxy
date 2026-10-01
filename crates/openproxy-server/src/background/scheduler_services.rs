//! Supervisor background services for quota sync and MiniMax sign-in reward check-in.

use super::BackgroundService;
use openproxy_adapters::upstream::{CancellationToken, UpstreamClient};
use openproxy_core::AppConfig;
use openproxy_core::oauth::OAuthProviderRegistry;
use openproxy_db::DbPool;
use openproxy_db::secrets::MasterKey;
use parking_lot::RwLock;
use std::sync::Arc;

/// Mirror transport cancellation without dropping a runner's in-flight work.
pub(super) async fn with_core_cancellation<F, Fut>(cancel: CancellationToken, run: F)
where
    F: FnOnce(tokio_util::sync::CancellationToken) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let core_cancel = tokio_util::sync::CancellationToken::new();
    if cancel.is_cancelled() {
        core_cancel.cancel();
    }
    let runner = run(core_cancel.clone());
    tokio::pin!(runner);
    tokio::select! {
        biased;
        () = cancel.cancelled() => {
            core_cancel.cancel();
            runner.await;
        }
        () = &mut runner => {}
    }
}

/// Background daemon for synchronizing upstream provider quota and model usage.
pub struct QuotaSyncService {
    pub db_pool: Arc<DbPool>,
    pub config: AppConfig,
    pub upstream_client: Arc<UpstreamClient>,
    pub master_key: Arc<MasterKey>,
    pub adapters: Arc<RwLock<Arc<Vec<openproxy_adapters::adapters::ProviderAdapterEnum>>>>,
    pub oauth_provider_registry: Arc<OAuthProviderRegistry>,
}

impl BackgroundService for QuotaSyncService {
    fn name(&self) -> &'static str {
        "quota_sync"
    }

    async fn run(&self, cancel: CancellationToken) {
        with_core_cancellation(cancel, |core_cancel| {
            openproxy_core::quota_sync::run_quota_sync_scheduler(
                Arc::clone(&self.db_pool),
                self.config.clone(),
                Arc::clone(&self.upstream_client),
                Arc::clone(&self.master_key),
                Arc::clone(&self.adapters),
                Arc::clone(&self.oauth_provider_registry),
                core_cancel,
            )
        })
        .await;
    }
}

/// Background daemon for MiniMax signin / daily reward check-ins.
pub struct MiniMaxCheckinService {
    pub db_pool: Arc<DbPool>,
    pub upstream_client: Arc<UpstreamClient>,
    pub master_key: Arc<MasterKey>,
}

impl BackgroundService for MiniMaxCheckinService {
    fn name(&self) -> &'static str {
        "minimax_checkin"
    }

    async fn run(&self, cancel: CancellationToken) {
        with_core_cancellation(cancel, |core_cancel| {
            openproxy_core::minimax_checkin::run_checkin_scheduler(
                Arc::clone(&self.db_pool),
                Arc::clone(&self.upstream_client),
                Arc::clone(&self.master_key),
                core_cancel,
            )
        })
        .await;
    }
}

pub struct SmartWarmupService {
    pub db_pool: Arc<DbPool>,
    pub config: AppConfig,
    pub upstream_client: Arc<UpstreamClient>,
    pub master_key: Arc<MasterKey>,
}

impl BackgroundService for SmartWarmupService {
    fn name(&self) -> &'static str {
        "smart_warmup"
    }

    async fn run(&self, cancel: CancellationToken) {
        with_core_cancellation(cancel, |core_cancel| {
            openproxy_core::smart_warmup::run_smart_warmup_scheduler(
                Arc::clone(&self.db_pool),
                self.config.clone(),
                Arc::clone(&self.upstream_client),
                Arc::clone(&self.master_key),
                core_cancel,
            )
        })
        .await;
    }
}
