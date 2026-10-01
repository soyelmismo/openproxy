//! Facade for models_dev_sync, re-exporting openproxy-discovery models_dev_sync
//! with ServiceContainer DI wrappers.

pub use openproxy_discovery::models_dev_sync::*;

pub mod client {
    pub use openproxy_discovery::models_dev_sync::client::*;

    pub async fn start_sync_scheduler_with_container(
        services: &crate::di::ServiceContainer,
        check_interval_secs: u64,
    ) -> crate::error::Result<()> {
        let db_pool = services.db_pool()?;
        let upstream_client = services.upstream_client()?;
        openproxy_discovery::models_dev_sync::client::start_sync_scheduler(
            db_pool,
            upstream_client,
            check_interval_secs,
        )
        .await;
        Ok(())
    }
}

pub use client::start_sync_scheduler_with_container;
