//! Facade for discovery_scheduler, exposing ServiceContainer DI wrappers in openproxy-core
//! while delegating core scheduler mechanics to openproxy-discovery.

pub use openproxy_discovery::discovery_scheduler::*;

pub mod scheduler {
    pub use openproxy_discovery::discovery_scheduler::scheduler::*;

    pub fn start_with_container(
        services: &crate::di::ServiceContainer,
        config: super::DiscoverySchedulerConfig,
    ) -> crate::error::Result<super::DiscoveryScheduler> {
        let db_pool = services.db_pool()?;
        let master_key = services.master_key()?;
        let adapters = services.adapters()?;
        let upstream_client = services.upstream_client()?;
        Ok(openproxy_discovery::discovery_scheduler::start(
            db_pool,
            master_key,
            adapters,
            upstream_client,
            config,
        ))
    }

    pub async fn start_with_container_async(
        services: &crate::di::ServiceContainer,
        config: super::DiscoverySchedulerConfig,
    ) -> crate::error::Result<super::DiscoveryScheduler> {
        openproxy_discovery::discovery_scheduler::start_async(
            services.db_pool()?,
            services.master_key()?,
            services.adapters()?,
            services.upstream_client()?,
            config,
        )
        .await
    }
}

pub use scheduler::{start_with_container, start_with_container_async};
