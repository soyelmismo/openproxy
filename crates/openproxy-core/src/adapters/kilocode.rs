use super::*;

// =====================================================================
// Kilocode
// =====================================================================

/// Adapter for <https://api.kilo.ai/api/openrouter>.
///
/// Kilocode is an OpenRouter gateway with its own auth. Chat goes through
/// `/v1/chat/completions` but models are listed at `/models` (not
/// `/v1/models`), so [`models_url`] overrides the default.
#[derive(Clone)]
pub struct KilocodeAdapter {
    config: ProviderAdapterConfig,
}

impl KilocodeAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("kilocode"),
                base_url: "https://api.kilo.ai/api/openrouter/v1".into(),
                auth_type: AdapterAuthType::Bearer,
                format: AdapterFormat::Openai,
                extra_headers: vec![],
            },
        }
    }
}

crate::adapters::derive_default_from_new!(KilocodeAdapter);

impl ProviderAdapter for KilocodeAdapter {
    fn id(&self) -> &ProviderId {
        &self.config.id
    }

    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn build_chat_url(&self, _target_format: TargetFormat, _model: &ModelId) -> String {
        format!("{}/chat/completions", self.config.base_url)
    }

    fn build_auth_header(&self, api_key: &str) -> Option<(String, String)> {
        Some(("Authorization".into(), format!("Bearer {}", api_key)))
    }

    fn build_headers(
        &self,
        api_key: &str,
        _target_format: TargetFormat,
        _model: &ModelId,
    ) -> Vec<(String, String)> {
        let mut headers = vec![
            ("Content-Type".into(), "application/json".into()),
        ];
        if let Some(auth) = self.build_auth_header(api_key) {
            headers.push(auth);
        }
        headers
    }

    fn models_url(&self) -> Option<String> {
        // Kilocode's model list is at /api/openrouter/models, not
        // base_url + "/models".
        Some("https://api.kilo.ai/api/openrouter/models".into())
    }

    async fn fetch_models(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        fetch_openai_models(
            "https://api.kilo.ai/api/openrouter/models",
            upstream_client,
            api_key,
            "kilocode",
        )
        .await
    }
}
