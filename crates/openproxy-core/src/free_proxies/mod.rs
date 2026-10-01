//! Facade for free_proxies, re-exporting openproxy-proxy-pool with ServiceContainer DI wrappers.

pub use openproxy_proxy_pool::free_proxies::*;

pub mod sync {
    pub use openproxy_proxy_pool::free_proxies::sync::*;

    pub async fn sync_all_providers_with_container(
        services: &crate::di::ServiceContainer,
    ) -> crate::error::Result<openproxy_proxy_pool::free_proxies::SyncSummary> {
        let pool = services.db_pool()?;
        openproxy_proxy_pool::free_proxies::sync_all_providers(pool).await
    }
}

pub use sync::sync_all_providers_with_container;
