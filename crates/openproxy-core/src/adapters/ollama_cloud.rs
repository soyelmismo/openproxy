use super::*;

// =====================================================================
// Ollama Cloud
// =====================================================================

/// Adapter for <https://ollama.com>.
///
/// Ollama Cloud speaks OpenAI-compatible `/v1/chat/completions` with
/// Bearer auth. Model IDs use Ollama's `:` convention (e.g.
/// `gemma4:31b`, `qwen3.5:397b`) — the colon is valid inside JSON
/// strings so no special escaping is needed in the request body.
#[derive(Clone)]
pub struct OllamaCloudAdapter {
    config: ProviderAdapterConfig,
}

impl OllamaCloudAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("ollama-cloud"),
                base_url: "https://ollama.com/v1".into(),
                auth_type: AdapterAuthType::Bearer,
                format: AdapterFormat::Openai,
                extra_headers: vec![],
            },
        }
    }
}

crate::adapters::derive_default_from_new!(OllamaCloudAdapter);

impl ProviderAdapter for OllamaCloudAdapter {
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
        Some("https://ollama.com/api/tags".into())
    }

    async fn fetch_models(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        let url = self.models_url().ok_or_else(|| {
            CoreError::Internal("ollama-cloud: models_url is None (impossible)".into())
        })?;

        let body = upstream_get_json(
            upstream_client,
            &url,
            &[("Authorization", format!("Bearer {api_key}"))],
        )
        .await
        .map_err(|e| CoreError::UpstreamConnection(format!("ollama-cloud /api/tags: {e}")))?;

        let payload: OllamaTagsResponse = serde_json::from_value(body)
            .map_err(|e| CoreError::Parse(format!("ollama-cloud /api/tags parse: {e}")))?;

        let out = payload
            .models
            .into_iter()
            .map(|m| {
                let id = m.name.clone().unwrap_or_default();
                let family = derive_ollama_family(&id);
                DiscoveredModel {
                    model_id: ModelId::new(id.clone()),
                    display_name: m.display_name.or(Some(id)),
                    target_format: TargetFormat::Openai,
                    context_length: None,
                    max_output_tokens: None,
                    input_modalities: None,
                    output_modalities: None,
                    model_type: Some("chat".into()),
                    family,
                    capabilities: None,
                }
            })
            .collect();
        Ok(out)
    }
}

/// Response shape of `GET https://ollama.com/api/tags`.
#[derive(Debug, Deserialize)]
struct OllamaTagsResponse {
    #[serde(default)]
    models: Vec<OllamaTagEntry>,
}

#[derive(Debug, Deserialize)]
struct OllamaTagEntry {
    name: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
}

/// Best-effort family extraction from an Ollama model id.
fn derive_ollama_family(id: &str) -> Option<String> {
    let lower = id.to_ascii_lowercase();
    if lower.contains("deepseek") {
        return Some("deepseek".into());
    }
    if lower.contains("kimi") {
        return Some("kimi".into());
    }
    if lower.contains("glm") {
        return Some("glm".into());
    }
    if lower.contains("minimax") {
        return Some("minimax".into());
    }
    if lower.contains("gemma") {
        return Some("gemma".into());
    }
    if lower.contains("nemotron") {
        return Some("nemotron".into());
    }
    if lower.contains("qwen") {
        return Some("qwen".into());
    }
    None
}
