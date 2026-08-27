use super::{
    AdapterAuthType, AdapterFormat, Arc, Deserialize, DiscoveredModel, ModelId, ProviderAdapter,
    ProviderAdapterConfig, ProviderId, Result, TargetFormat, UpstreamClient, upstream_get_json,
};

// =====================================================================
// OpenRouter
// =====================================================================

/// Adapter for <https://openrouter.ai>.
///
/// OpenRouter is OpenAI-only on the wire: every model is served through
/// `POST /chat/completions` regardless of which upstream actually answers
/// behind the scenes.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct OpenRouterAdapter {
    config: ProviderAdapterConfig,
}

impl OpenRouterAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("openrouter"),
                name: "OpenRouter".into(),
                anonymous_fallback: false,
                rate_limit_scope: "account".into(),
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
    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn models_dev_canonical_ids(&self) -> &'static [&'static str] {
        &[
            "openai",
            "anthropic",
            "meta",
            "mistral",
            "deepseek",
            "qwen",
            "amazon",
            "cohere",
            "perplexity",
            "groq",
            "together",
            "fireworks",
            "deepinfra",
            "xai",
        ]
    }

    fn metadata(&self) -> openproxy_types::ProviderMetadata {
        let mut meta = openproxy_types::ProviderMetadata::custom_default();
        meta.built_in = true;
        meta.deletable = false;
        meta.supports_quota = false;
        meta.quota_refresh_supported = false;
        meta
    }

    async fn fetch_models(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        let url = self.models_url().ok_or_else(|| {
            openproxy_types::error::CoreError::Internal("openrouter has no models_url".into())
        })?;

        let auth = format!("Bearer {api_key}");
        let body = upstream_get_json(upstream_client, &url, &[("Authorization", &auth)])
            .await
            .map_err(openproxy_types::error::CoreError::UpstreamConnection)?;

        let arr = body.get("data").and_then(|v| v.as_array()).ok_or_else(|| {
            openproxy_types::error::CoreError::Parse(
                "openrouter response missing 'data' array".into(),
            )
        })?;

        Ok(arr.iter().filter_map(map_openrouter_entry).collect())
    }
}

fn extract_modalities(
    arch: &mut Option<OpenRouterArchitecture>,
) -> (Option<Vec<String>>, Option<Vec<String>>) {
    let Some(a) = arch.as_mut() else {
        return (None, None);
    };
    let input = (!a.input_modalities.is_empty()).then(|| std::mem::take(&mut a.input_modalities));
    let output =
        (!a.output_modalities.is_empty()).then(|| std::mem::take(&mut a.output_modalities));
    (input, output)
}

fn derive_openrouter_family(entry: &OpenRouterModelEntry, id_string: &str) -> Option<String> {
    entry
        .canonical_slug
        .clone()
        .or_else(|| entry.hugging_face_id.clone())
        .or_else(|| derive_family_from_id(id_string))
}

fn map_openrouter_entry(raw: &serde_json::Value) -> Option<DiscoveredModel> {
    let mut entry: OpenRouterModelEntry = serde::Deserialize::deserialize(raw).ok()?;
    let id_string = entry.id.take()?;
    let caps = derive_capabilities(&entry);
    let model_type = infer_model_type_openrouter(&id_string, entry.architecture.as_ref());
    let family = derive_openrouter_family(&entry, &id_string);
    let display_name = entry.name.or_else(|| Some(id_string.clone()));
    let context_length = entry
        .context_length
        .or_else(|| entry.top_provider.as_ref().and_then(|t| t.context_length));
    let max_output_tokens = entry
        .top_provider
        .as_ref()
        .and_then(|t| t.max_completion_tokens);
    let (input_modalities, output_modalities) = extract_modalities(&mut entry.architecture);

    Some(DiscoveredModel {
        model_id: ModelId::new(id_string),
        display_name,
        target_format: TargetFormat::Openai,
        context_length,
        max_output_tokens,
        input_modalities,
        output_modalities,
        model_type: Some(model_type),
        family,
        capabilities: Some(caps),
    })
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

fn derive_vision_capabilities(
    caps: &mut openproxy_types::ModelCapabilities,
    arch: Option<&OpenRouterArchitecture>,
) {
    let has_image_input = arch.is_some_and(|a| {
        a.input_modalities
            .iter()
            .any(|m| m == "image" || m == "video")
    });
    if has_image_input {
        caps.vision = Some(true);
        caps.attachment = Some(true);
    }
}

fn derive_params_capabilities(caps: &mut openproxy_types::ModelCapabilities, params: &[String]) {
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
}

fn apply_params_fallback(caps: &mut openproxy_types::ModelCapabilities) {
    caps.tool_calling.get_or_insert(true);
    caps.structured_output.get_or_insert(true);
    caps.temperature.get_or_insert(true);
}

/// Derive capabilities from the OpenRouter model entry. Every capability
/// is set only when there's positive evidence; everything else stays
/// `None` so the public `GET /v1/models` projection can distinguish
/// "unknown" from "explicitly false".
fn derive_capabilities(entry: &OpenRouterModelEntry) -> openproxy_types::ModelCapabilities {
    use openproxy_types::ModelCapabilities;
    let mut caps = ModelCapabilities::empty();
    derive_vision_capabilities(&mut caps, entry.architecture.as_ref());

    let params = entry.supported_parameters.as_deref().unwrap_or(&[]);
    derive_params_capabilities(&mut caps, params);

    if params.is_empty() {
        apply_params_fallback(&mut caps);
    }

    caps
}

fn detect_non_text_modality(arch: &OpenRouterArchitecture) -> Option<&'static str> {
    let has_text = arch.output_modalities.iter().any(|m| m == "text");
    if has_text {
        return None;
    }
    let has_image = arch.output_modalities.iter().any(|m| m == "image");
    let has_audio = arch.output_modalities.iter().any(|m| m == "audio");
    if has_image && !has_audio {
        Some("image")
    } else if has_audio && !has_image {
        Some("audio")
    } else {
        None
    }
}

/// Classify a model id into a coarse `model_type` string
/// (`"chat" | "embedding" | "image" | "audio" | "rerank"`) using both
/// the id's name and the `architecture.output_modalities` field.
fn infer_model_type_openrouter(id: &str, architecture: Option<&OpenRouterArchitecture>) -> String {
    let inferred = openproxy_types::capabilities::infer_model_type(id);
    if inferred != "chat" {
        return inferred.to_string();
    }
    match architecture.and_then(detect_non_text_modality) {
        Some(kind) => kind.to_string(),
        None => "chat".to_string(),
    }
}

/// Best-effort extraction of a model "family" from a model id. The
/// `canonical_slug` and `hugging_face_id` paths in the adapter's main
/// loop are preferred when present; this is the final fallback for
/// upstreams that only supply the raw id.
fn derive_family_from_id(id: &str) -> Option<String> {
    let name = id.rsplit_once('/').map_or(id, |(_, tail)| tail);
    openproxy_types::capabilities::infer_family(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_infer_model_type_openrouter() {
        assert_eq!(infer_model_type_openrouter("openai/gpt-4", None), "chat");

        let img_arch = OpenRouterArchitecture {
            input_modalities: vec![],
            output_modalities: vec!["image".to_string()],
        };
        assert_eq!(
            infer_model_type_openrouter("some-model", Some(&img_arch)),
            "image"
        );

        let audio_arch = OpenRouterArchitecture {
            input_modalities: vec![],
            output_modalities: vec!["audio".to_string()],
        };
        assert_eq!(
            infer_model_type_openrouter("some-model", Some(&audio_arch)),
            "audio"
        );

        let mixed_arch = OpenRouterArchitecture {
            input_modalities: vec![],
            output_modalities: vec!["text".to_string(), "image".to_string()],
        };
        assert_eq!(
            infer_model_type_openrouter("some-model", Some(&mixed_arch)),
            "chat"
        );
    }
}
