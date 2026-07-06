use super::*;

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
pub struct OpenCodeZenAdapter {
    config: ProviderAdapterConfig,
}

impl OpenCodeZenAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("opencode-zen"),
                base_url: "https://opencode.ai/zen/v1".into(),
                auth_type: AdapterAuthType::Bearer,
                format: AdapterFormat::Mixed,
                extra_headers: vec![],
            },
        }
    }
}

impl Default for OpenCodeZenAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ProviderAdapter for OpenCodeZenAdapter {
    fn id(&self) -> &ProviderId {
        &self.config.id
    }

    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn build_chat_url(&self, target_format: TargetFormat, _model: &ModelId) -> String {
        match target_format {
            TargetFormat::Openai => format!("{}/chat/completions", self.config.base_url),
            TargetFormat::Anthropic => format!("{}/messages", self.config.base_url),
            TargetFormat::Gemini => format!("{}/chat/completions", self.config.base_url),
        }
    }

    fn build_auth_header(&self, api_key: &str) -> (String, String) {
        // The default trait impl uses a single auth_type from the config;
        // for `Mixed` we want the format-specific header, so we always return
        // the Bearer variant here and let `build_headers` choose per-format.
        // Callers that need the Anthropic-style `x-api-key` should use
        // `build_headers` with `target_format = Anthropic` (or override
        // `build_auth_header` in a derived impl).
        ("Authorization".into(), format!("Bearer {}", api_key))
    }

    fn build_headers(
        &self,
        api_key: &str,
        target_format: TargetFormat,
        _model: &ModelId,
    ) -> Vec<(String, String)> {
        let mut headers = Vec::with_capacity(4);

        // Only add auth headers if we have an API key.
        if !api_key.is_empty() {
            match target_format {
                TargetFormat::Anthropic => {
                    headers.push(("x-api-key".into(), api_key.to_string()));
                    headers.push(("Anthropic-Version".into(), "2023-06-01".into()));
                }
                TargetFormat::Openai | TargetFormat::Gemini => {
                    headers.push(("Authorization".into(), format!("Bearer {}", api_key)));
                }
            }
        }

        headers.push(("Content-Type".into(), "application/json".into()));
        headers.push(("User-Agent".into(), "openproxy/0.1".into()));
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
        let url = self
            .models_url()
            .ok_or_else(|| CoreError::Validation("opencode-zen: models_url is None".into()))?;

        // `bearer_auth(api_key)` is the equivalent of the old
        // `.bearer_auth(api_key)` reqwest call: it sets the
        // `Authorization: Bearer ***` header. We pass the same
        // string in via the helper.
        let body = upstream_get_json(
            upstream_client,
            &url,
            &[("Authorization", format!("Bearer {api_key}"))],
        )
        .await
        .map_err(|e| CoreError::UpstreamConnection(format!("opencode-zen /models: {e}")))?;

        let payload: OpenAIModelsResponse = serde_json::from_value(body)
            .map_err(|e| CoreError::Validation(format!("opencode-zen /models parse: {e}")))?;

        let out = payload
            .data
            .into_iter()
            .map(|m| {
                let id = m.id;
                let target_format = classify_zen_target_format(&id);
                DiscoveredModel {
                    model_id: ModelId::new(id.clone()),
                    display_name: Some(id),
                    target_format,
                    // OpenCode Zen's /models response only carries
                    // ids. The runtime fallback in `GET /v1/models`
                    // fills in the rest via heuristics.
                    context_length: None,
                    max_output_tokens: None,
                    input_modalities: None,
                    output_modalities: None,
                    model_type: None,
                    family: None,
                    capabilities: None,
                }
            })
            .collect();
        Ok(out)
    }
}

/// Heuristic for picking the wire format of a model in OpenCode Zen's catalogue.
///
/// Anthropic-family identifiers (`claude`, `MiniMax`) go to `/messages`; the
/// rest are served as OpenAI on `/chat/completions`. The matching is
/// case-insensitive.
pub(crate) fn classify_zen_target_format(id: &str) -> TargetFormat {
    let lower = id.to_ascii_lowercase();
    if lower.contains("claude") || lower.contains("minimax") {
        TargetFormat::Anthropic
    } else {
        TargetFormat::Openai
    }
}



