//! Factory for creating provider adapters dynamically.

use crate::adapters::{builtin_adapters, ProviderAdapterConfig, ProviderAdapterEnum};
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
        builtin_adapters().into_iter().find(|a| a.id() == id)
    }

    /// Instantiate an adapter dynamically based on static configuration.
    pub fn create_from_config(&self, config: ProviderAdapterConfig) -> ProviderAdapterEnum {
        match config.id.as_str() {
            "openrouter" => ProviderAdapterEnum::OpenRouter(crate::adapters::openrouter::OpenRouterAdapter::new()),
            "minimax" => ProviderAdapterEnum::MiniMax(crate::adapters::minimax::MiniMaxAdapter::new()),
            "opencode-zen" => ProviderAdapterEnum::OpenCodeZen(crate::adapters::opencode_zen::OpenCodeZenAdapter::new()),
            "ollama-cloud" => ProviderAdapterEnum::OllamaCloud(crate::adapters::ollama_cloud::OllamaCloudAdapter::new()),
            "nous-research" => ProviderAdapterEnum::NousResearch(crate::adapters::nous_research::NousResearchAdapter::new()),
            "nvidia-nim" => ProviderAdapterEnum::NvidiaNim(crate::adapters::nvidia_nim::NvidiaNimAdapter::new()),
            "kilocode" => ProviderAdapterEnum::Kilocode(crate::adapters::kilocode::KilocodeAdapter::new()),
            "cloudflare-workers-ai" => {
                ProviderAdapterEnum::CloudflareWorkersAI(crate::adapters::cloudflare_workers_ai::CloudflareWorkersAIAdapter::new())
            }
            "gemini" => ProviderAdapterEnum::Gemini(crate::adapters::gemini::GeminiAdapter::new()),
            "antigravity" => ProviderAdapterEnum::Antigravity(crate::adapters::antigravity::AntigravityAdapter::new()),
            "codex" => ProviderAdapterEnum::Codex(crate::adapters::codex::CodexAdapter::new()),
            "kiro" => ProviderAdapterEnum::Kiro(crate::adapters::kiro_ai::KiroAdapter::new()),
            _ => ProviderAdapterEnum::Custom(crate::adapters::custom_adapter::CustomAdapter::from_config(config)),
        }
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
    }

    #[test]
    fn test_adapter_factory_create_from_config() {
        let factory = AdapterFactory::new();
        let config = ProviderAdapterConfig {
            id: ProviderId::new("custom-provider"),
            base_url: "https://api.custom.com/v1".into(),
            auth_type: AdapterAuthType::Bearer,
            format: AdapterFormat::Openai,
            extra_headers: vec![],
        };
        let adapter = factory.create_from_config(config);
        assert_eq!(adapter.id().as_str(), "custom-provider");
    }
}
