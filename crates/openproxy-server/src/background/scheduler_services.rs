//! Supervisor background services for quota sync and MiniMax sign-in reward check-in.

use super::BackgroundService;
use openproxy_adapters::upstream::{CancellationToken, UpstreamClient};
use openproxy_core::AppConfig;
use openproxy_core::oauth::OAuthProviderRegistry;
use openproxy_db::DbPool;
use openproxy_db::secrets::MasterKey;
use parking_lot::RwLock;
use std::sync::Arc;

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
        let core_cancel = Default::default();
        let runner_cancel = Clone::clone(&core_cancel);

        let runner = openproxy_core::quota_sync::run_quota_sync_scheduler(
            Arc::clone(&self.db_pool),
            self.config.clone(),
            Arc::clone(&self.upstream_client),
            Arc::clone(&self.master_key),
            Arc::clone(&self.adapters),
            Arc::clone(&self.oauth_provider_registry),
            runner_cancel,
        );
        tokio::pin!(runner);

        if cancel.is_cancelled() {
            core_cancel.cancel();
            runner.await;
            return;
        }

        let mirror = async {
            cancel.cancelled().await;
            core_cancel.cancel();
        };
        tokio::pin!(mirror);

        tokio::select! {
            biased;
            () = &mut runner => {}
            () = &mut mirror => {
                runner.await;
            }
        }
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
        let core_cancel = Default::default();
        let runner_cancel = Clone::clone(&core_cancel);

        let runner = openproxy_core::minimax_checkin::run_checkin_scheduler(
            Arc::clone(&self.db_pool),
            Arc::clone(&self.upstream_client),
            Arc::clone(&self.master_key),
            runner_cancel,
        );
        tokio::pin!(runner);

        if cancel.is_cancelled() {
            core_cancel.cancel();
            runner.await;
            return;
        }

        let mirror = async {
            cancel.cancelled().await;
            core_cancel.cancel();
        };
        tokio::pin!(mirror);

        tokio::select! {
            biased;
            () = &mut runner => {}
            () = &mut mirror => {
                runner.await;
            }
        }
    }
}
