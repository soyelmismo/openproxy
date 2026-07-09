use super::*;

// =====================================================================
// Gemini (Google AI Studio)
// =====================================================================

/// Adapter for Google's Gemini API (`generativelanguage.googleapis.com`).
///
/// Gemini uses its own wire format (not OpenAI-compatible):
/// - Auth: `x-goog-api-key: <key>` header
/// - Chat URL: `${base}/models/${model}:generateContent`
/// - Models URL: `${base}/models`
#[derive(Clone)]
pub struct GeminiAdapter {
    config: ProviderAdapterConfig,
}

impl GeminiAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("gemini"),
                base_url: "https://generativelanguage.googleapis.com/v1beta".into(),
                auth_type: AdapterAuthType::GoogApiKey,
                format: AdapterFormat::Gemini,
                extra_headers: vec![],
            },
        }
    }
}

crate::adapters::derive_default_from_new!(GeminiAdapter);

impl ProviderAdapter for GeminiAdapter {
    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn build_chat_url(&self, _target_format: TargetFormat, model: &ModelId) -> String {
        // Gemini puts the model in the URL path.
        // Since openproxy always uses streaming to the upstream (dispatch_upstream
        // forces is_streaming = true and expects SSE chunks), we must use the
        // streamGenerateContent?alt=sse endpoint. Calling generateContent would
        // return a non-streaming JSON body, which blocks headers until completion
        // and causes timeouts.
        //
        // Sanitize the model id to prevent path traversal — strip "/"
        // segments.  Dot characters are *kept* because real model names
        // like "gemini-2.5-flash" contain them.
        let model_str = model.as_str();
        let safe_model: String = model_str.replace('/', "");
        format!(
            "{}/models/{}:streamGenerateContent?alt=sse",
            self.config.base_url, safe_model
        )
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
            .ok_or_else(|| CoreError::Internal("gemini: models_url is None (impossible)".into()))?;

        fetch_models_with_auth(
            &url,
            upstream_client,
            &[("x-goog-api-key", api_key.to_string())],
            "models",
            "gemini",
            |m| {
                let full_name = m.get("name").and_then(|v| v.as_str())?;
                let id = full_name.strip_prefix("models/").unwrap_or(full_name);
                let display_name = m
                    .get("displayName")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| id.to_string());
                Some(DiscoveredModel {
                    model_id: ModelId::new(id.to_string()),
                    display_name: Some(display_name),
                    target_format: TargetFormat::Gemini,
                    context_length: None,
                    max_output_tokens: None,
                    input_modalities: None,
                    output_modalities: None,
                    model_type: None,
                    family: None,
                    capabilities: None,
                })
            },
        )
        .await
    }
}
