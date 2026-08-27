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

        let models: Vec<DiscoveredModel> = arr
            .iter()
            .filter_map(|raw| {
                let mut entry: OpenRouterModelEntry = serde::Deserialize::deserialize(raw).ok()?;
                let id_string = entry.id.take()?;

                // Derive capabilities from supported_parameters.
                let caps = derive_capabilities(&entry);

                // Derive model_type from id and modalities.
                let model_type =
                    infer_model_type_openrouter(&id_string, entry.architecture.as_ref());

                // Extract modalities (skip empty arrays so they serialize
                // as NULL rather than `[]`).
                let input_modalities = entry.architecture.as_mut().and_then(|a| {
                    if a.input_modalities.is_empty() {
                        None
                    } else {
                        Some(std::mem::take(&mut a.input_modalities))
                    }
                });
                let output_modalities = entry.architecture.as_mut().and_then(|a| {
                    if a.output_modalities.is_empty() {
                        None
                    } else {
                        Some(std::mem::take(&mut a.output_modalities))
                    }
                });

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
                    .or(entry.hugging_face_id)
                    .or_else(|| derive_family_from_id(&id_string));

                let display_name = entry.name.or_else(|| Some(id_string.clone()));
                Some(DiscoveredModel {
                    model_id: ModelId::new(id_string),
                    display_name,
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
fn derive_capabilities(entry: &OpenRouterModelEntry) -> openproxy_types::ModelCapabilities {
    use openproxy_types::ModelCapabilities;
    let mut caps = ModelCapabilities::empty();

    // vision: from architecture.input_modalities.
    let has_image_input = entry.architecture.as_ref().is_some_and(|a| {
        a.input_modalities
            .iter()
            .any(|m| m == "image" || m == "video")
    });
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
    let inferred = openproxy_types::capabilities::infer_model_type(id);
    if inferred != "chat" {
        return inferred.to_string();
    }

    // Output modalities: only classify as image/audio if output is dedicated (does not include text)
    if let Some(arch) = architecture {
        let has_text = arch.output_modalities.iter().any(|m| m == "text");
        let has_image = arch.output_modalities.iter().any(|m| m == "image");
        let has_audio = arch.output_modalities.iter().any(|m| m == "audio");

        if !has_text {
            if has_image && !has_audio {
                return "image".to_string();
            }
            if has_audio && !has_image {
                return "audio".to_string();
            }
        }
    }

    "chat".to_string()
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
