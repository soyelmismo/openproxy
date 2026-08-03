use openproxy_types::error::{CoreError, Result};
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

/// A thread-safe generic Service-Locator / TypeMap for Dependency Injection.
#[derive(Clone, Default)]
pub struct ServiceContainer {
    services: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
}

impl ServiceContainer {
    pub fn new() -> Self {
        Self {
            services: HashMap::new(),
        }
    }

    /// Register a service instance `T` wrapped in an `Arc`.
    pub fn register<T: Send + Sync + 'static>(&mut self, service: Arc<T>) {
        self.services.insert(TypeId::of::<T>(), service);
    }

    /// Register a service instance `T` (will be wrapped in `Arc`).
    pub fn register_val<T: Send + Sync + 'static>(&mut self, service: T) {
        self.register(Arc::new(service));
    }

    /// Builder pattern method to register an `Arc<T>`.
    pub fn with<T: Send + Sync + 'static>(mut self, service: Arc<T>) -> Self {
        self.register(service);
        self
    }

    /// Builder pattern method to register a `T`.
    pub fn with_val<T: Send + Sync + 'static>(mut self, service: T) -> Self {
        self.register_val(service);
        self
    }

    /// Retrieve a service `T` wrapped in `Arc<T>`.
    pub fn get<T: Send + Sync + 'static>(&self) -> Result<Arc<T>> {
        self.services
            .get(&TypeId::of::<T>())
            .cloned()
            .and_then(|boxed| boxed.downcast::<T>().ok())
            .ok_or_else(|| {
                CoreError::Internal(format!(
                    "Service '{}' not found in ServiceContainer",
                    std::any::type_name::<T>()
                ))
            })
    }

    /// Optionally retrieve a service `T` wrapped in `Arc<T>`.
    pub fn try_get<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.services
            .get(&TypeId::of::<T>())
            .cloned()
            .and_then(|boxed| boxed.downcast::<T>().ok())
    }

    /// Helper for retrieving `DbPool`.
    pub fn db_pool(&self) -> Result<Arc<openproxy_db::DbPool>> {
        self.get::<openproxy_db::DbPool>()
    }

    /// Helper for retrieving `MasterKey`.
    pub fn master_key(&self) -> Result<Arc<openproxy_db::secrets::MasterKey>> {
        self.get::<openproxy_db::secrets::MasterKey>()
    }

    /// Helper for retrieving `UpstreamClient`.
    pub fn upstream_client(&self) -> Result<Arc<openproxy_adapters::upstream::UpstreamClient>> {
        self.get::<openproxy_adapters::upstream::UpstreamClient>()
    }

    /// Helper for retrieving adapters vector.
    pub fn adapters(&self) -> Result<Arc<Vec<openproxy_adapters::adapters::ProviderAdapterEnum>>> {
        self.get::<Vec<openproxy_adapters::adapters::ProviderAdapterEnum>>()
    }
}
