use super::*;

// =====================================================================
// OpenRouter
// =====================================================================

/// Adapter for <https://openrouter.ai>.
///
/// OpenRouter is OpenAI-only on the wire: every model is served through
/// `POST /chat/completions` regardless of which upstream actually answers
/// behind the scenes.
#[derive(Clone)]
pub struct OpenRouterAdapter {
    config: ProviderAdapterConfig,
}

impl OpenRouterAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("openrouter"),
                base_url: "https://openrouter.ai/api/v1".into(),
                auth_type: AdapterAuthType::Bearer,
                format: AdapterFormat::Openai,
                extra_headers: vec![
                    ("HTTP-Referer".into(), "https://openproxy.local".into()),
                    ("X-Title".into(), "openproxy".into()),
                ],
            },
        }
    }
}

crate::adapters::derive_default_from_new!(OpenRouterAdapter);

impl ProviderAdapter for OpenRouterAdapter {
    fn id(&self) -> &ProviderId {
        &self.config.id
    }

    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn metadata(&self) -> crate::providers::ProviderMetadata {
        crate::providers::ProviderMetadata {
            built_in: crate::providers::is_builtin(self.id().as_str()),
            deletable: !crate::providers::is_builtin(self.id().as_str()),
            supports_quota: true,
            quota_refresh_supported: true,
        }
    }

    fn build_chat_url(&self, _target_format: TargetFormat, _model: &ModelId) -> String {
        // OpenRouter is OpenAI-only; the target_format arg is ignored.
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
        let mut headers = Vec::with_capacity(2 + self.config.extra_headers.len());
        if let Some((name, value)) = self.build_auth_header(api_key) {
            headers.push((name, value));
        }
        headers.push(("Content-Type".into(), "application/json".into()));
        for (k, v) in &self.config.extra_headers {
            headers.push((k.clone(), v.clone()));
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
        let url = self.models_url().ok_or_else(|| {
            crate::error::CoreError::Internal("openrouter has no models_url".into())
        })?;

        let body = upstream_get_json(
            upstream_client,
            &url,
            &[("Authorization", format!("Bearer {api_key}"))],
        )
        .await
        .map_err(|e| crate::error::CoreError::UpstreamConnection(e.to_string()))?;

        let arr = body.get("data").and_then(|v| v.as_array()).ok_or_else(|| {
            crate::error::CoreError::Parse("openrouter response missing 'data' array".into())
        })?;

        let models: Vec<DiscoveredModel> = arr
            .iter()
            .filter_map(|raw| {
                let entry: OpenRouterModelEntry = serde_json::from_value(raw.clone()).ok()?;
                // Borrow the id first so the rest of the closure can
                // keep `&entry` borrowable; only clone the String when
                // we need to move it into the `DiscoveredModel`.
                let id_ref = entry.id.as_ref()?;
                let id_string = id_ref.clone();

                // Derive capabilities from supported_parameters.
                let caps = derive_capabilities(&entry);

                // Derive model_type from id and modalities.
                let model_type =
                    infer_model_type_openrouter(&id_string, entry.architecture.as_ref());

                // Extract modalities (skip empty arrays so they serialize
                // as NULL rather than `[]`).
                let input_modalities = entry
                    .architecture
                    .as_ref()
                    .map(|a| a.input_modalities.clone())
                    .filter(|v| !v.is_empty());
                let output_modalities = entry
                    .architecture
                    .as_ref()
                    .map(|a| a.output_modalities.clone())
                    .filter(|v| !v.is_empty());

                // Context: prefer top-level, fallback to top_provider.
                let context_length = entry
                    .context_length
                    .or_else(|| entry.top_provider.as_ref().and_then(|t| t.context_length));

                // Max output: from top_provider.
                let max_output_tokens = entry
                    .top_provider
                    .as_ref()
                    .and_then(|t| t.max_completion_tokens);

                // Family: derive from canonical_slug or hugging_face_id or id.
                let family = entry
                    .canonical_slug
                    .clone()
                    .or_else(|| entry.hugging_face_id.clone())
                    .or_else(|| derive_family_from_id(&id_string));

                Some(DiscoveredModel {
                    model_id: ModelId::new(id_string.clone()),
                    display_name: entry.name.or(Some(id_string)),
                    // OpenRouter is OpenAI-only on the wire for chat completions.
                    target_format: TargetFormat::Openai,
                    context_length,
                    max_output_tokens,
                    input_modalities,
                    output_modalities,
                    model_type: Some(model_type),
                    family,
                    capabilities: Some(caps),
                })
            })
            .collect();

        Ok(models)
    }
}

#[derive(Debug, Deserialize)]
struct OpenRouterArchitecture {
    #[serde(default)]
    input_modalities: Vec<String>,
    #[serde(default)]
    output_modalities: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterTopProvider {
    #[serde(default)]
    context_length: Option<i64>,
    #[serde(default)]
    max_completion_tokens: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterModelEntry {
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    canonical_slug: Option<String>,
    #[serde(default)]
    hugging_face_id: Option<String>,
    #[serde(default)]
    context_length: Option<i64>,
    #[serde(default)]
    architecture: Option<OpenRouterArchitecture>,
    #[serde(default)]
    top_provider: Option<OpenRouterTopProvider>,
    #[serde(default)]
    supported_parameters: Option<Vec<String>>,
}

/// Build a [`crate::capabilities::ModelCapabilities`] from an OpenRouter
/// model entry's `supported_parameters` and `architecture`. Each field
/// is set only when there's positive evidence; everything else stays
/// `None` so the public `GET /v1/models` projection can distinguish
/// "unknown" from "explicitly false".
fn derive_capabilities(entry: &OpenRouterModelEntry) -> crate::capabilities::ModelCapabilities {
    use crate::capabilities::ModelCapabilities;
    let mut caps = ModelCapabilities::empty();

    // vision: from architecture.input_modalities.
    let has_image_input = entry
        .architecture
        .as_ref()
        .map(|a| {
            a.input_modalities
                .iter()
                .any(|m| m == "image" || m == "video")
        })
        .unwrap_or(false);
    if has_image_input {
        caps.vision = Some(true);
        caps.attachment = Some(true);
    }

    // tool_calling / reasoning / structured_output / temperature come
    // straight from the supported_parameters list OpenRouter publishes.
    let params = entry.supported_parameters.as_deref().unwrap_or(&[]);
    if params.iter().any(|p| p == "tools") {
        caps.tool_calling = Some(true);
    }
    if params
        .iter()
        .any(|p| p == "reasoning" || p == "include_reasoning")
    {
        caps.reasoning = Some(true);
        caps.thinking = Some(true);
    }
    if params.iter().any(|p| p == "structured_outputs") {
        caps.structured_output = Some(true);
    }
    if params.iter().any(|p| p == "temperature") {
        caps.temperature = Some(true);
    }

    // If supported_parameters is missing entirely, fall back to the
    // chat-model defaults so the model is still advertised as usable
    // for tool_calling/structured_output/temperature. This matches
    // the heuristic in `capabilities::infer_capabilities` for the
    // no-evidence case.
    if params.is_empty() {
        if caps.tool_calling.is_none() {
            caps.tool_calling = Some(true);
        }
        if caps.structured_output.is_none() {
            caps.structured_output = Some(true);
        }
        if caps.temperature.is_none() {
            caps.temperature = Some(true);
        }
    }

    caps
}

/// Classify a model id into a coarse `model_type` string
/// (`"chat" | "embedding" | "image" | "audio" | "rerank"`) using both
/// the id's name and the `architecture.output_modalities` field.
fn infer_model_type_openrouter(id: &str, architecture: Option<&OpenRouterArchitecture>) -> String {
    let lower = id.to_lowercase();

    if lower.contains("embed") {
        return "embedding".to_string();
    }
    if lower.contains("dall-e")
        || lower.contains("flux")
        || lower.contains("imagen")
        || lower.contains("sdxl")
        || lower.contains("ideogram")
    {
        return "image".to_string();
    }
    if lower.contains("whisper") || lower.contains("tts") || lower.contains("eleven") {
        return "audio".to_string();
    }
    if lower.contains("rerank") {
        return "rerank".to_string();
    }

    // Output modalities: if a model emits image/audio, classify by that
    // even if the name doesn't carry a giveaway keyword.
    if let Some(arch) = architecture {
        if arch.output_modalities.iter().any(|m| m == "image") {
            return "image".to_string();
        }
        if arch.output_modalities.iter().any(|m| m == "audio") {
            return "audio".to_string();
        }
    }

    "chat".to_string()
}

/// Best-effort extraction of a model "family" from a model id. The
/// `canonical_slug` and `hugging_face_id` paths in the adapter's main
/// loop are preferred when present; this is the final fallback for
/// upstreams that only supply the raw id.
fn derive_family_from_id(id: &str) -> Option<String> {
    // Strip the `<org>/` prefix that OpenRouter ids carry, fall back to
    // the raw id when no slash is present.
    let name = id.split('/').next_back()?;
    let lower = name.to_lowercase();

    // Order matters only for substrings: more-specific strings are
    // checked first so e.g. `gpt-4o` wins over `gpt-4`.
    if lower.contains("gpt-4o") {
        return Some("gpt-4o".into());
    }
    if lower.contains("gpt-4") {
        return Some("gpt-4".into());
    }
    if lower.contains("gpt-3.5") {
        return Some("gpt-3.5".into());
    }
    if lower == "o1" || lower.starts_with("o1-") {
        return Some("o1".into());
    }
    if lower == "o3" || lower.starts_with("o3-") {
        return Some("o3".into());
    }
    if lower.contains("claude-opus-4") {
        return Some("claude-opus-4".into());
    }
    if lower.contains("claude-sonnet-4") {
        return Some("claude-sonnet-4".into());
    }
    if lower.contains("claude-3.5") {
        return Some("claude-3.5".into());
    }
    if lower.contains("claude-3") {
        return Some("claude-3".into());
    }
    if lower.contains("gemini-2.5") {
        return Some("gemini-2.5".into());
    }
    if lower.contains("gemini-1.5") {
        return Some("gemini-1.5".into());
    }
    if lower.contains("deepseek") {
        return Some("deepseek".into());
    }
    if lower.contains("llama-3.3") {
        return Some("llama-3.3".into());
    }
    if lower.contains("llama-3.1") {
        return Some("llama-3.1".into());
    }
    if lower.contains("qwen3") {
        return Some("qwen3".into());
    }
    if lower.contains("qwen2.5") {
        return Some("qwen2.5".into());
    }
    if lower.contains("qwen2") {
        return Some("qwen2".into());
    }
    if lower.contains("gemma-3") {
        return Some("gemma-3".into());
    }
    if lower.contains("gemma-2") {
        return Some("gemma-2".into());
    }
    if lower.contains("mistral") {
        return Some("mistral".into());
    }
    if lower.contains("mixtral") {
        return Some("mixtral".into());
    }
    if lower.contains("phi-3") {
        return Some("phi-3".into());
    }
    if lower.contains("nemotron") {
        return Some("nemotron".into());
    }
    if lower.contains("command-r") {
        return Some("command-r".into());
    }
    if lower.contains("cogito") {
        return Some("cogito".into());
    }
    None
}
