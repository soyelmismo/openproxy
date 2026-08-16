use super::{ProviderAdapterConfig, ProviderAdapter, ProviderId, AdapterAuthType, AdapterFormat, TargetFormat, ModelId, Arc, UpstreamClient, Result, DiscoveredModel};
pub use crate::adapters::opencode_common::classify_opencode_target_format as classify_go_target_format;
use crate::adapters::opencode_common::{build_opencode_headers, fetch_opencode_models};

// =====================================================================
// OpenCode Go (mixed OpenAI / Anthropic)
// =====================================================================

/// Adapter for <https://opencode.ai/zen/go/v1>.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct OpenCodeGoAdapter {
    config: ProviderAdapterConfig,
}

impl OpenCodeGoAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("opencode-go"),
                name: "OpenCode Go".into(),
                anonymous_fallback: true,
                rate_limit_scope: "account".into(),
                base_url: "https://opencode.ai/zen/go/v1".into(),
                auth_type: AdapterAuthType::Bearer,
                format: AdapterFormat::Mixed,
                extra_headers: vec![],
            },
        }
    }
}

crate::adapters::derive_default_from_new!(OpenCodeGoAdapter);

impl ProviderAdapter for OpenCodeGoAdapter {
    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn build_headers(
        &self,
        api_key: &str,
        target_format: TargetFormat,
        _model: &ModelId,
    ) -> Vec<(String, String)> {
        build_opencode_headers(self, api_key, target_format)
    }

    async fn fetch_models(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        fetch_opencode_models(self, upstream_client, api_key).await
    }
}
