use super::{
    AdapterAuthType, AdapterFormat, Arc, Deserialize, DiscoveredModel, ModelId, ProviderAdapter,
    ProviderAdapterConfig, ProviderId, Result, TargetFormat, UpstreamClient,
    build_discovered_model_full, fetch_openai_compatible_models_with_mapper,
};

// OpenRouter

/// Embeddings OpenRouter serves but does not list in `/api/v1/models`.
///
/// Verified against the live API on 2026-10-31: the public models endpoint
/// returns 456 entries and not one of them is an embedding, so discovery alone
/// can never populate this catalog. These rows are declared here so the
/// provider shows the models it actually serves.
///
/// Modalities are not hardcoded here. `build_discovered_model_full` derives
/// them from the model id through the shared capability inference, which is
/// what keeps this list honest when a new family shows up: the fix lands in one
/// place instead of in every curated row.
///
/// The Gemini and NVIDIA rows are the multimodal ones. The rest are text-only,
/// which is what their own cards state.
const OPENROUTER_EMBEDDING_MODELS: &[(&str, &str)] = &[
    ("google/gemini-embedding-001", "Gemini Embedding 001"),
    ("google/gemini-embedding-2", "Gemini Embedding 2"),
    ("openai/text-embedding-3-large", "Text Embedding 3 Large"),
    ("openai/text-embedding-3-small", "Text Embedding 3 Small"),
    ("mistralai/mistral-embed-2312", "Mistral Embed 2312"),
    ("perplexity/pplx-embed-v1-0.6b", "PPLX Embed v1 0.6B"),
    ("perplexity/pplx-embed-v1-4b", "PPLX Embed v1 4B"),
    ("qwen/qwen3-embedding-4b", "Qwen3 Embedding 4B"),
    ("qwen/qwen3-embedding-8b", "Qwen3 Embedding 8B"),
    (
        "nvidia/nemotron-3.5-embed-8b:free",
        "Nemotron 3.5 Embed 8B (free)",
    ),
    (
        "nvidia/nemotron-3.5-embed-1b:free",
        "Nemotron 3.5 Embed 1B (free)",
    ),
    (
        "nvidia/nemotron-3-embed-1b:free",
        "Nemotron 3 Embed 1B (free)",
    ),
    (
        "nvidia/llama-nemotron-embed-vl-1b-v2:free",
        "Llama Nemotron Embed VL 1B V2 (free)",
    ),
];

/// Appends the curated embeddings to whatever `/models` returned.
///
/// Ids already present are skipped, so if OpenRouter ever starts listing its
/// embeddings the upstream row wins and no duplicate is created.
fn append_curated_embeddings(models: &mut Vec<DiscoveredModel>) {
    for (id, display_name) in OPENROUTER_EMBEDDING_MODELS {
        if models.iter().any(|m| m.model_id.as_str() == *id) {
            continue;
        }
        // Context and output limits are left to the shared inference: guessing
        // them here would put a stale number in the catalog.
        models.push(build_discovered_model_full(
            (*id).to_string(),
            Some((*display_name).to_string()),
            TargetFormat::Openai,
            None,
            None,
        ));
    }
}

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
    crate::adapters::impl_adapter_config!();

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

        let auth = crate::upstream::format_bearer(api_key);
        fetch_openai_compatible_models_with_mapper(
            &url,
            upstream_client,
            &[("Authorization", &auth)],
            "data",
            "openrouter",
            map_openrouter_entry,
        )
        .await
        .map(|mut models| {
            // `/models` never lists embeddings, so the curated catalog is what
            // puts them in front of the router.
            append_curated_embeddings(&mut models);
            models
        })
    }
}

type ExtractedModalities = (Option<Box<[String]>>, Option<Box<[String]>>);

fn extract_modalities(arch: &mut Option<OpenRouterArchitecture>) -> ExtractedModalities {
    let Some(a) = arch.as_mut() else {
        return (None, None);
    };
    let input = (!a.input_modalities.is_empty())
        .then(|| std::mem::take(&mut a.input_modalities).into_boxed_slice());
    let output = (!a.output_modalities.is_empty())
        .then(|| std::mem::take(&mut a.output_modalities).into_boxed_slice());
    (input, output)
}

fn derive_openrouter_family(entry: &OpenRouterModelEntry, id_string: &str) -> Option<String> {
    entry
        .canonical_slug
        .as_deref()
        .or(entry.hugging_face_id.as_deref())
        .map(|s| s.to_string())
        .or_else(|| derive_family_from_id(id_string))
}

fn map_openrouter_entry(raw: &serde_json::Value) -> Option<DiscoveredModel> {
    let mut entry: OpenRouterModelEntry = serde::Deserialize::deserialize(raw).ok()?;
    let id_string = entry.id.take()?;
    let caps = derive_capabilities(&entry);
    let model_type = infer_model_type_openrouter(&id_string, entry.architecture.as_ref());
    let family = derive_openrouter_family(&entry, &id_string);
    let display_name = entry
        .name
        .as_deref()
        .or(Some(id_string.as_str()))
        .map(|s| s.to_string());
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
    let has_video_input = arch.is_some_and(|a| a.input_modalities.iter().any(|m| m == "video"));
    if has_video_input {
        caps.video = Some(true);
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

    if let Some(id) = entry.id.as_deref()
        && openproxy_types::capabilities::is_decision_model(&id.to_lowercase())
    {
        caps.decisions = Some(true);
    }

    caps
}

fn detect_non_text_modality(arch: &OpenRouterArchitecture) -> Option<&'static str> {
    if arch.output_modalities.iter().any(|m| m == "decision") {
        return Some("decision");
    }
    let has_text = arch.output_modalities.iter().any(|m| m == "text");
    if has_text {
        return None;
    }
    let has_image = arch.output_modalities.iter().any(|m| m == "image");
    let has_audio = arch.output_modalities.iter().any(|m| m == "audio");
    let has_video = arch.output_modalities.iter().any(|m| m == "video");
    if has_image && !has_audio && !has_video {
        Some("image")
    } else if has_audio && !has_image && !has_video {
        Some("audio")
    } else if has_video && !has_image && !has_audio {
        Some("video")
    } else {
        None
    }
}

/// Classify a model id into a coarse `model_type` string
/// (`"chat" | "embedding" | "image" | "audio" | "rerank" | "decision"`) using both
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

    /// Every embedding the catalog declares must actually be classified as an
    /// embedding, or the router would send it down the chat path.
    #[test]
    fn every_curated_embedding_is_typed_as_an_embedding() {
        for (id, _) in OPENROUTER_EMBEDDING_MODELS {
            assert_eq!(
                openproxy_types::capabilities::infer_model_type(id),
                "embedding",
                "{id} must be typed as an embedding"
            );
        }
    }

    /// The modalities each curated row ends up with, which is the whole point of
    /// the catalog: these models are absent from `/models`, so nothing else
    /// would ever declare what they accept.
    #[test]
    fn curated_embeddings_carry_the_modalities_their_cards_state() {
        let expected: &[(&str, &[&str])] = &[
            (
                "google/gemini-embedding-2",
                &["text", "image", "audio", "video", "document"],
            ),
            ("nvidia/nemotron-3.5-embed-8b:free", &["text", "image"]),
            ("nvidia/nemotron-3.5-embed-1b:free", &["text", "image"]),
            ("nvidia/nemotron-3-embed-1b:free", &["text", "image"]),
            (
                "nvidia/llama-nemotron-embed-vl-1b-v2:free",
                &["text", "image"],
            ),
            ("google/gemini-embedding-001", &["text"]),
            ("openai/text-embedding-3-large", &["text"]),
            ("qwen/qwen3-embedding-8b", &["text"]),
            ("perplexity/pplx-embed-v1-4b", &["text"]),
            ("mistralai/mistral-embed-2312", &["text"]),
        ];

        for (id, modalities) in expected {
            let caps = openproxy_types::capabilities::infer_capabilities(id);
            let inferred =
                openproxy_types::capabilities::infer_input_modalities_for_model(id, &caps);
            assert_eq!(inferred.to_vec(), *modalities, "{id}");
        }
    }

    /// The catalog is appended to whatever the upstream returned, which is how
    /// the rows reach the database.
    #[test]
    fn curated_embeddings_are_appended_to_the_discovered_list() {
        let mut models = Vec::new();
        append_curated_embeddings(&mut models);
        assert_eq!(models.len(), OPENROUTER_EMBEDDING_MODELS.len());

        let ids: Vec<&str> = models.iter().map(|m| m.model_id.as_str()).collect();
        for (id, _) in OPENROUTER_EMBEDDING_MODELS {
            assert!(ids.contains(id), "{id} missing from the appended catalog");
        }
        // Every row is typed and carries modalities, not left blank.
        for model in &models {
            assert_eq!(model.model_type.as_deref(), Some("embedding"));
            assert!(model.input_modalities.is_some());
        }
    }

    /// If OpenRouter ever starts listing its embeddings, the upstream row must
    /// win instead of producing a duplicate.
    #[test]
    fn curated_embeddings_do_not_duplicate_what_upstream_already_listed() {
        let upstream = build_discovered_model_full(
            "nvidia/nemotron-3.5-embed-8b:free".to_string(),
            Some("from upstream".to_string()),
            TargetFormat::Openai,
            Some(32_768),
            None,
        );
        let mut models = vec![upstream];
        append_curated_embeddings(&mut models);

        let matching: Vec<&DiscoveredModel> = models
            .iter()
            .filter(|m| m.model_id.as_str() == "nvidia/nemotron-3.5-embed-8b:free")
            .collect();
        assert_eq!(matching.len(), 1, "the curated row must not be added twice");
        assert_eq!(
            matching[0].display_name.as_deref(),
            Some("from upstream"),
            "the upstream row must survive untouched"
        );
        assert_eq!(matching[0].context_length, Some(32_768));
    }

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

        let decision_arch = OpenRouterArchitecture {
            input_modalities: vec![],
            output_modalities: vec!["decision".to_string()],
        };
        assert_eq!(
            infer_model_type_openrouter("some-model", Some(&decision_arch)),
            "decision"
        );

        let video_arch = OpenRouterArchitecture {
            input_modalities: vec!["text".to_string(), "image".to_string()],
            output_modalities: vec!["video".to_string()],
        };
        assert_eq!(
            infer_model_type_openrouter("some-model", Some(&video_arch)),
            "video"
        );
    }

    #[test]
    fn test_video_input_sets_video_capability() {
        let entry = OpenRouterModelEntry {
            id: Some("some-model".to_string()),
            name: None,
            canonical_slug: None,
            hugging_face_id: None,
            context_length: None,
            architecture: Some(OpenRouterArchitecture {
                input_modalities: vec!["text".to_string(), "video".to_string()],
                output_modalities: vec!["text".to_string()],
            }),
            top_provider: None,
            supported_parameters: None,
        };
        let caps = derive_capabilities(&entry);
        assert_eq!(caps.video, Some(true));
        // Video input still implies vision+attachment, as before.
        assert_eq!(caps.vision, Some(true));
        assert_eq!(caps.attachment, Some(true));
    }
}
