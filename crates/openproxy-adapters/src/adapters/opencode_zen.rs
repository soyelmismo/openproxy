use super::{
    AdapterAuthType, AdapterFormat, Arc, DiscoveredModel, ModelId, ProviderAdapter,
    ProviderAdapterConfig, ProviderId, Result, TargetFormat, UpstreamClient,
};
pub use crate::adapters::opencode_common::classify_opencode_target_format as classify_zen_target_format;
use crate::adapters::opencode_common::{build_opencode_headers, fetch_opencode_models};

// =====================================================================
// OpenCode Zen (mixed OpenAI / Anthropic)
// =====================================================================

/// Adapter for <https://opencode.ai/zen/v1>.
///
/// OpenCode Zen is mixed: some models speak OpenAI, others Anthropic, and
/// the per-model choice is recorded in `models.target_format`. The adapter
/// picks `/chat/completions` vs `/messages` based on that stored value, and
/// the auth header flips between `Authorization: Bearer ...` and
/// `x-api-key: ...` accordingly.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct OpenCodeZenAdapter {
    config: ProviderAdapterConfig,
}

impl OpenCodeZenAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("opencode-zen"),
                name: "OpenCode Zen".into(),
                anonymous_fallback: true,
                rate_limit_scope: "account".into(),
                base_url: "https://opencode.ai/zen/v1".into(),
                auth_type: AdapterAuthType::Bearer,
                format: AdapterFormat::Mixed,
                extra_headers: vec![],
            },
        }
    }
}

crate::adapters::derive_default_from_new!(OpenCodeZenAdapter);

impl ProviderAdapter for OpenCodeZenAdapter {
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
