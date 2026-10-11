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

    pub fn register<T: Send + Sync + 'static>(&mut self, service: Arc<T>) {
        self.services.insert(TypeId::of::<T>(), service);
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyService {
        value: String,
    }

    #[test]
    fn test_service_container_register_and_get() {
        let mut container = ServiceContainer::new();
        container.register_val(DummyService {
            value: "hello".to_string(),
        });

        let service = container.get::<DummyService>();
        assert!(service.is_ok());
        assert_eq!(service.unwrap().value, "hello");

        let try_service = container.try_get::<DummyService>();
        assert!(try_service.is_some());
        assert_eq!(try_service.unwrap().value, "hello");
    }

    #[test]
    fn test_service_container_builder_pattern() {
        let container = ServiceContainer::new()
            .with_val(42i32)
            .with(Arc::new("world".to_string()));

        assert_eq!(*container.get::<i32>().unwrap(), 42);
        assert_eq!(container.get::<String>().unwrap().as_str(), "world");
    }

    #[test]
    fn test_service_container_missing_service() {
        let container = ServiceContainer::new();

        let res = container.get::<DummyService>();
        assert!(res.is_err());
        if let Err(CoreError::Internal(msg)) = res {
            assert!(msg.contains("DummyService"));
        } else {
            panic!("expected CoreError::Internal");
        }

        assert!(container.try_get::<DummyService>().is_none());
    }
}
