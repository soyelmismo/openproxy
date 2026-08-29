//! Factory for creating provider adapters dynamically.

use crate::adapters::{ProviderAdapterConfig, ProviderAdapterEnum, builtin_adapters};
use openproxy_types::ProviderId;

/// Factory for instantiating provider adapters via dependency injection.
#[derive(Debug, Clone, Default)]
pub struct AdapterFactory;

impl AdapterFactory {
    /// Create a new `AdapterFactory`.
    pub fn new() -> Self {
        Self
    }

    /// Retrieve all default built-in adapters.
    pub fn create_all(&self) -> Vec<ProviderAdapterEnum> {
        builtin_adapters()
    }

    /// Instantiate a built-in adapter by provider ID.
    pub fn create_builtin(&self, id: &ProviderId) -> Option<ProviderAdapterEnum> {
        ProviderAdapterEnum::from_provider_id(id.as_str())
    }

    /// Instantiate an adapter dynamically based on static configuration.
    pub fn create_from_config(&self, config: ProviderAdapterConfig) -> ProviderAdapterEnum {
        self.create_builtin(&config.id).unwrap_or_else(|| {
            ProviderAdapterEnum::Custom(Box::new(
                crate::adapters::custom_adapter::CustomAdapter::from_config(config),
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::{AdapterAuthType, AdapterFormat};

    #[test]
    fn test_adapter_factory_create_builtin() {
        let factory = AdapterFactory::new();
        let gemini = factory.create_builtin(&ProviderId::new("gemini"));
        assert!(gemini.is_some());
        assert_eq!(gemini.unwrap().id().as_str(), "gemini");

        let nonexistent = factory.create_builtin(&ProviderId::new("nonexistent"));
        assert!(nonexistent.is_none());

        for builtin in factory.create_all() {
            let created = factory.create_builtin(builtin.id());
            assert!(created.is_some(), "missing builtin for {}", builtin.id());
            assert_eq!(created.unwrap().id(), builtin.id());
        }
    }

    #[test]
    fn test_adapter_factory_create_from_config() {
        let factory = AdapterFactory::new();
        let config = ProviderAdapterConfig {
            id: ProviderId::new("custom-provider"),
            name: "Custom".to_string(),
            base_url: "https://api.custom.com/v1".into(),
            auth_type: AdapterAuthType::Bearer,
            format: AdapterFormat::Openai,
            extra_headers: vec![],
            anonymous_fallback: false,
            rate_limit_scope: "account".into(),
        };
        let adapter = factory.create_from_config(config);
        assert_eq!(adapter.id().as_str(), "custom-provider");
    }
}
