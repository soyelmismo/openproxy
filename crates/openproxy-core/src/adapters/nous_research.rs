use super::*;

// =====================================================================
// Nous Research
// =====================================================================

/// Adapter for <https://inference-api.nousresearch.com>.
///
/// Nous Research speaks OpenAI-compatible `/v1/chat/completions` with
/// Bearer auth. Free-tier models include Hermes-4-405B and Hermes-4-70B.
#[derive(Clone)]
pub struct NousResearchAdapter {
    config: ProviderAdapterConfig,
}

impl NousResearchAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("nous-research"),
                base_url: "https://inference-api.nousresearch.com/v1".into(),
                auth_type: AdapterAuthType::Bearer,
                format: AdapterFormat::Openai,
                extra_headers: vec![],
            },
        }
    }
}

crate::adapters::derive_default_from_new!(NousResearchAdapter);

impl ProviderAdapter for NousResearchAdapter {
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
        Some(format!("{}/models", self.config.base_url))
    }

    async fn fetch_models(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        fetch_openai_models(
            &self.models_url().expect("always Some"),
            upstream_client,
            api_key,
            "nous-research",
            crate::models::TargetFormat::Openai,
        )
        .await
    }
}
