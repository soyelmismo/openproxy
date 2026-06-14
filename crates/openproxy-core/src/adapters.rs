//! Provider adapter trait + built-in adapters (OpenRouter, MiniMax Coding, OpenCode Zen, etc.).
//!
//! See mvp-spec §3 (per-provider config) and §5 (the trait surface).
//!
//! Each adapter knows how to:
//! - build the upstream URL for a chat completion,
//! - build the auth header for a given API key,
//! - build the full set of request headers,
//! - locate the provider's `/models` endpoint (or report it doesn't exist),
//! - fetch and normalize the model list into [`DiscoveredModel`] rows.
//!
//! This module is the trait layer only; persistent CRUD for `providers` lives
//! in [`crate::providers`].

use crate::error::{CoreError, Result};
use crate::ids::{ModelId, ProviderId};
use crate::models::{DiscoveredModel, TargetFormat};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Static configuration for a single provider adapter.
///
/// `id`, `base_url`, `auth_type`, and `format` describe a fixed upstream; the
/// runtime state (API keys, per-account selection, etc.) lives elsewhere and
/// is passed into the methods on the trait.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderAdapterConfig {
    pub id: ProviderId,
    pub base_url: String,
    pub auth_type: AdapterAuthType,
    pub format: AdapterFormat,
    pub extra_headers: Vec<(String, String)>,
}

/// How the adapter encodes the API key in the HTTP request.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AdapterAuthType {
    /// `Authorization: Bearer <key>`
    Bearer,
    /// `x-api-key: <key>`
    XApiKey,
    /// `x-goog-api-key: <key>` (Google Gemini API)
    GoogApiKey,
    /// No auth header sent (anonymous access).
    None,
}

/// Native wire format the provider speaks for chat completions.
///
/// `Openai` -> `/chat/completions`, `Anthropic` -> `/messages`,
/// `Gemini` -> `/models/{model}:generateContent`,
/// `Mixed` -> depends on the model's stored `target_format`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AdapterFormat {
    Openai,
    Anthropic,
    Mixed,
    Gemini,
}

/// Per-provider adapter. One concrete impl per upstream.
///
/// All methods are `&self` and the trait is `Send + Sync` so adapters can live
/// behind an `Arc<dyn ProviderAdapter>` in long-lived registries.
#[async_trait]
pub trait ProviderAdapter: Send + Sync {
    /// Stable identifier of this provider (e.g. `"openrouter"`).
    fn id(&self) -> &ProviderId;

    /// Static configuration snapshot.
    fn config(&self) -> &ProviderAdapterConfig;

    /// Shortcut for `self.config().auth_type`.
    fn auth_type(&self) -> AdapterAuthType {
        self.config().auth_type
    }

    /// Shortcut for `self.config().format`.
    fn format(&self) -> AdapterFormat {
        self.config().format
    }

    /// Build the URL to POST a chat completion to.
    ///
    /// - `Openai` -> `base_url + "/chat/completions"`
    /// - `Anthropic` -> `base_url + "/messages"` (plus any provider-specific
    ///   query string such as `?beta=true`)
    /// - `Gemini` -> `base_url + "/models/" + model + ":generateContent"`
    /// - `Mixed` -> depends on `target_format` (same per-branch rules as above)
    fn build_chat_url(&self, target_format: TargetFormat, model: &ModelId) -> String;

    /// Build the auth header pair `(header_name, header_value)` for the given
    /// API key.
    fn build_auth_header(&self, api_key: &str) -> (String, String);

    /// Build the full set of request headers for a chat completion call.
    ///
    /// Implementations should at least include the auth header, a
    /// `Content-Type: application/json` entry, and any `extra_headers` from
    /// the config. Providers with per-format quirks (Anthropic versioning,
    /// `User-Agent`, etc.) override the default.
    fn build_headers(
        &self,
        api_key: &str,
        target_format: TargetFormat,
        model: &ModelId,
    ) -> Vec<(String, String)>;

    /// URL of the provider's `/models` endpoint for live discovery, or `None`
    /// if the provider does not expose a model list (e.g. MiniMax).
    fn models_url(&self) -> Option<String>;

    /// Fetch the live model list using the provided HTTP client and API key.
    ///
    /// The default implementation GETs [`Self::models_url`] with a `Bearer`
    /// auth header and parses an OpenRouter-style `{"data": [{...}]}` payload.
    /// Providers with a different response shape override this method.
    async fn fetch_models(
        &self,
        client: &reqwest::Client,
        api_key: &str,
    ) -> Result<Vec<DiscoveredModel>>;
}

// =====================================================================
// OpenRouter
// =====================================================================

/// Adapter for <https://openrouter.ai>.
///
/// OpenRouter is OpenAI-only on the wire: every model is served through
/// `POST /chat/completions` regardless of which upstream actually answers
/// behind the scenes.
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

impl Default for OpenRouterAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ProviderAdapter for OpenRouterAdapter {
    fn id(&self) -> &ProviderId {
        &self.config.id
    }

    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn build_chat_url(&self, _target_format: TargetFormat, _model: &ModelId) -> String {
        // OpenRouter is OpenAI-only; the target_format arg is ignored.
        format!("{}/chat/completions", self.config.base_url)
    }

    fn build_auth_header(&self, api_key: &str) -> (String, String) {
        ("Authorization".into(), format!("Bearer {}", api_key))
    }

    fn build_headers(
        &self,
        api_key: &str,
        _target_format: TargetFormat,
        _model: &ModelId,
    ) -> Vec<(String, String)> {
        let (name, value) = self.build_auth_header(api_key);
        let mut headers = Vec::with_capacity(2 + self.config.extra_headers.len());
        headers.push((name, value));
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
        client: &reqwest::Client,
        api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        let url = self.models_url().ok_or_else(|| {
            crate::error::CoreError::Internal("openrouter has no models_url".into())
        })?;

        let resp = client
            .get(&url)
            .header("Authorization", format!("Bearer {}", api_key))
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| crate::error::CoreError::UpstreamConnection(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(crate::error::CoreError::UpstreamConnection(format!(
                "openrouter /models returned status {}",
                resp.status().as_u16()
            )));
        }

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| crate::error::CoreError::Parse(e.to_string()))?;

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
                let model_type = infer_model_type_openrouter(&id_string, entry.architecture.as_ref());

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
struct OpenRouterModelsResponse {
    #[serde(default)]
    data: Vec<OpenRouterModelEntry>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterArchitecture {
    #[serde(default)]
    input_modalities: Vec<String>,
    #[serde(default)]
    output_modalities: Vec<String>,
    #[serde(default)]
    modality: Option<String>,
    #[serde(default)]
    tokenizer: Option<String>,
    #[serde(default)]
    instruct_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterPricing {
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    completion: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterTopProvider {
    #[serde(default)]
    context_length: Option<i64>,
    #[serde(default)]
    max_completion_tokens: Option<i64>,
    #[serde(default)]
    is_moderated: Option<bool>,
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
    pricing: Option<OpenRouterPricing>,
    #[serde(default)]
    top_provider: Option<OpenRouterTopProvider>,
    #[serde(default)]
    supported_parameters: Option<Vec<String>>,
    #[serde(default)]
    description: Option<String>,
}

/// Build a [`crate::capabilities::ModelCapabilities`] from an OpenRouter
/// model entry's `supported_parameters` and `architecture`. Each field
/// is set only when there's positive evidence; everything else stays
/// `None` so the public `GET /v1/models` projection can distinguish
/// "unknown" from "explicitly false".
fn derive_capabilities(
    entry: &OpenRouterModelEntry,
) -> crate::capabilities::ModelCapabilities {
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
    if params.iter().any(|p| p == "reasoning" || p == "include_reasoning") {
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
fn infer_model_type_openrouter(
    id: &str,
    architecture: Option<&OpenRouterArchitecture>,
) -> String {
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

// =====================================================================
// MiniMax (Coding)
// =====================================================================

/// Adapter for MiniMax's Anthropic-compatible coding endpoint.
///
/// The base URL is `https://api.minimax.io` (the bare host, no path). The
/// chat endpoint is reached by appending `/anthropic/v1/messages?beta=true`
/// at request time, and the model-discovery endpoint is reached by
/// appending `/v1/models`. Splitting the two paths this way is what lets
/// the same `base_url` serve both surfaces without one being a substring
/// of the other.
pub struct MiniMaxAdapter {
    config: ProviderAdapterConfig,
}

impl MiniMaxAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("minimax"),
                base_url: "https://api.minimax.io".into(),
                auth_type: AdapterAuthType::Bearer,
                format: AdapterFormat::Anthropic,
                extra_headers: vec![("Anthropic-Version".into(), "2023-06-01".into())],
            },
        }
    }
}

impl Default for MiniMaxAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ProviderAdapter for MiniMaxAdapter {
    fn id(&self) -> &ProviderId {
        &self.config.id
    }

    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn build_chat_url(&self, _target_format: TargetFormat, _model: &ModelId) -> String {
        // MiniMax exposes the Anthropic Messages API at /anthropic/v1/messages.
        // The `?beta=true` query parameter is required to enable the relevant
        // beta features (tool use, prompt caching, etc.).
        format!("{}/anthropic/v1/messages?beta=true", self.config.base_url)
    }

    fn build_auth_header(&self, api_key: &str) -> (String, String) {
        ("Authorization".into(), format!("Bearer {}", api_key))
    }

    fn build_headers(
        &self,
        api_key: &str,
        _target_format: TargetFormat,
        _model: &ModelId,
    ) -> Vec<(String, String)> {
        let (name, value) = self.build_auth_header(api_key);
        let mut headers = Vec::with_capacity(2 + self.config.extra_headers.len());
        headers.push((name, value));
        headers.push(("Content-Type".into(), "application/json".into()));
        for (k, v) in &self.config.extra_headers {
            headers.push((k.clone(), v.clone()));
        }
        headers
    }

    fn models_url(&self) -> Option<String> {
        // MiniMax exposes its model catalogue at /v1/models (separate from
        // the /anthropic/v1/ chat surface). The auth scheme is the same
        // Bearer token.
        Some(format!("{}/v1/models", self.config.base_url))
    }

    async fn fetch_models(
        &self,
        client: &reqwest::Client,
        api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        let url = self.models_url().ok_or_else(|| {
            CoreError::Internal("minimax: models_url is None (impossible)".into())
        })?;

        let resp = client
            .get(&url)
            .header("Authorization", format!("Bearer {}", api_key))
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| {
                CoreError::UpstreamConnection(format!("minimax /v1/models: {}", e))
            })?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(CoreError::UpstreamError {
                status,
                provider: self.config.id.to_string(),
                model: "<models>".into(),
                body,
            });
        }

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| CoreError::Parse(format!("minimax /v1/models parse: {}", e)))?;

        // MiniMax returns an OpenAI-shaped list: {"object": "list",
        // "data": [{...}, ...]}. We accept a few equivalent shapes to
        // stay forward-compatible.
        let arr = body
            .get("data")
            .or_else(|| body.get("models"))
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                CoreError::Parse(
                    "minimax /v1/models: missing 'data' or 'models' array".into(),
                )
            })?;

        let out = arr
            .iter()
            .filter_map(|m| {
                let id = m.get("id").and_then(|v| v.as_str())?;
                let name = m
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| id.to_string());
                Some(DiscoveredModel {
                    model_id: ModelId::new(id.to_string()),
                    display_name: Some(name),
                    // MiniMax's chat surface is Anthropic Messages; every
                    // discovered model goes there.
                    target_format: TargetFormat::Anthropic,
                    // MiniMax's /v1/models response doesn't expose
                    // context_length / modalities / capabilities — leave
                    // them as None so the runtime fallback in
                    // `GET /v1/models` fills them in via heuristics.
                    context_length: None,
                    max_output_tokens: None,
                    input_modalities: None,
                    output_modalities: None,
                    model_type: None,
                    family: None,
                    capabilities: None,
                })
            })
            .collect();
        Ok(out)
    }
}

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
        client: &reqwest::Client,
        api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        let url = self
            .models_url()
            .ok_or_else(|| CoreError::Validation("opencode-zen: models_url is None".into()))?;

        let resp = client
            .get(&url)
            .bearer_auth(api_key)
            .send()
            .await
            .map_err(|e| CoreError::UpstreamConnection(format!("opencode-zen /models: {}", e)))?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(CoreError::UpstreamError {
                status,
                provider: self.config.id.to_string(),
                model: "<models>".into(),
                body,
            });
        }

        let payload: OpenAIModelsResponse = resp
            .json()
            .await
            .map_err(|e| CoreError::Validation(format!("opencode-zen /models parse: {}", e)))?;

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
fn classify_zen_target_format(id: &str) -> TargetFormat {
    let lower = id.to_ascii_lowercase();
    if lower.contains("claude") || lower.contains("minimax") {
        TargetFormat::Anthropic
    } else {
        TargetFormat::Openai
    }
}

#[derive(Debug, Deserialize)]
struct OpenAIModelsResponse {
    #[serde(default)]
    data: Vec<OpenAIModelEntry>,
}

#[derive(Debug, Deserialize)]
struct OpenAIModelEntry {
    id: String,
}

// =====================================================================
// Ollama Cloud
// =====================================================================

/// Adapter for <https://ollama.com>.
///
/// Ollama Cloud speaks OpenAI-compatible `/v1/chat/completions` with
/// Bearer auth. Model IDs use Ollama's `:` convention (e.g.
/// `gemma4:31b`, `qwen3.5:397b`) — the colon is valid inside JSON
/// strings so no special escaping is needed in the request body.
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

impl Default for OllamaCloudAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
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

    fn build_auth_header(&self, api_key: &str) -> (String, String) {
        ("Authorization".into(), format!("Bearer {}", api_key))
    }

    fn build_headers(
        &self,
        api_key: &str,
        _target_format: TargetFormat,
        _model: &ModelId,
    ) -> Vec<(String, String)> {
        let (name, value) = self.build_auth_header(api_key);
        vec![
            (name, value),
            ("Content-Type".into(), "application/json".into()),
        ]
    }

    fn models_url(&self) -> Option<String> {
        Some("https://ollama.com/api/tags".into())
    }

    async fn fetch_models(
        &self,
        client: &reqwest::Client,
        api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        let url = self.models_url().ok_or_else(|| {
            CoreError::Internal("ollama-cloud: models_url is None (impossible)".into())
        })?;

        let resp = client
            .get(&url)
            .bearer_auth(api_key)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| {
                CoreError::UpstreamConnection(format!("ollama-cloud /api/tags: {}", e))
            })?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(CoreError::UpstreamError {
                status,
                provider: self.config.id.to_string(),
                model: "<models>".into(),
                body,
            });
        }

        let payload: OllamaTagsResponse = resp.json().await.map_err(|e| {
            CoreError::Parse(format!("ollama-cloud /api/tags parse: {}", e))
        })?;

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
    model: Option<String>,
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

// =====================================================================
// Nous Research
// =====================================================================

/// Adapter for <https://inference-api.nousresearch.com>.
///
/// Nous Research speaks OpenAI-compatible `/v1/chat/completions` with
/// Bearer auth. Free-tier models include Hermes-4-405B and Hermes-4-70B.
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

impl Default for NousResearchAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
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

    fn build_auth_header(&self, api_key: &str) -> (String, String) {
        ("Authorization".into(), format!("Bearer {}", api_key))
    }

    fn build_headers(
        &self,
        api_key: &str,
        _target_format: TargetFormat,
        _model: &ModelId,
    ) -> Vec<(String, String)> {
        let (name, value) = self.build_auth_header(api_key);
        vec![
            (name, value),
            ("Content-Type".into(), "application/json".into()),
        ]
    }

    fn models_url(&self) -> Option<String> {
        Some(format!("{}/models", self.config.base_url))
    }

    async fn fetch_models(
        &self,
        client: &reqwest::Client,
        api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        fetch_openai_models(
            &self.models_url().expect("always Some"),
            client,
            api_key,
            "nous-research",
        )
        .await
    }
}

// =====================================================================
// NVIDIA NIM
// =====================================================================

/// Adapter for <https://integrate.api.nvidia.com>.
///
/// NVIDIA NIM speaks OpenAI-compatible `/v1/chat/completions` with
/// Bearer auth. Free tier offers 70+ models at ~40 RPM.
pub struct NvidiaNimAdapter {
    config: ProviderAdapterConfig,
}

impl NvidiaNimAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("nvidia-nim"),
                base_url: "https://integrate.api.nvidia.com/v1".into(),
                auth_type: AdapterAuthType::Bearer,
                format: AdapterFormat::Openai,
                extra_headers: vec![],
            },
        }
    }
}

impl Default for NvidiaNimAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ProviderAdapter for NvidiaNimAdapter {
    fn id(&self) -> &ProviderId {
        &self.config.id
    }

    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn build_chat_url(&self, _target_format: TargetFormat, _model: &ModelId) -> String {
        format!("{}/chat/completions", self.config.base_url)
    }

    fn build_auth_header(&self, api_key: &str) -> (String, String) {
        ("Authorization".into(), format!("Bearer {}", api_key))
    }

    fn build_headers(
        &self,
        api_key: &str,
        _target_format: TargetFormat,
        _model: &ModelId,
    ) -> Vec<(String, String)> {
        let (name, value) = self.build_auth_header(api_key);
        vec![
            (name, value),
            ("Content-Type".into(), "application/json".into()),
        ]
    }

    fn models_url(&self) -> Option<String> {
        Some(format!("{}/models", self.config.base_url))
    }

    async fn fetch_models(
        &self,
        client: &reqwest::Client,
        api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        fetch_openai_models(
            &self.models_url().expect("always Some"),
            client,
            api_key,
            "nvidia-nim",
        )
        .await
    }
}

// =====================================================================
// Kilocode
// =====================================================================

/// Adapter for <https://api.kilo.ai/api/openrouter>.
///
/// Kilocode is an OpenRouter gateway with its own auth. Chat goes through
/// `/v1/chat/completions` but models are listed at `/models` (not
/// `/v1/models`), so [`models_url`] overrides the default.
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

impl Default for KilocodeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
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

    fn build_auth_header(&self, api_key: &str) -> (String, String) {
        ("Authorization".into(), format!("Bearer {}", api_key))
    }

    fn build_headers(
        &self,
        api_key: &str,
        _target_format: TargetFormat,
        _model: &ModelId,
    ) -> Vec<(String, String)> {
        let (name, value) = self.build_auth_header(api_key);
        vec![
            (name, value),
            ("Content-Type".into(), "application/json".into()),
        ]
    }

    fn models_url(&self) -> Option<String> {
        // Kilocode's model list is at /api/openrouter/models, not
        // base_url + "/models".
        Some("https://api.kilo.ai/api/openrouter/models".into())
    }

    async fn fetch_models(
        &self,
        client: &reqwest::Client,
        api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        fetch_openai_models(
            "https://api.kilo.ai/api/openrouter/models",
            client,
            api_key,
            "kilocode",
        )
        .await
    }
}

// =====================================================================
// Shared OpenAI model-list fetcher
// =====================================================================

/// Fetch and parse an OpenAI-shaped `GET /models` response.
///
/// All three new OpenAI-compatible providers (Nous Research, NVIDIA NIM,
/// Kilocode) return `{"data": [{"id": "...", ...}]}`. This helper
/// avoids duplicating the HTTP + deserialization boilerplate.
async fn fetch_openai_models(
    url: &str,
    client: &reqwest::Client,
    api_key: &str,
    provider_name: &str,
) -> Result<Vec<DiscoveredModel>> {
    let resp = client
        .get(url)
        .bearer_auth(api_key)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| CoreError::UpstreamConnection(format!("{} /models: {}", provider_name, e)))?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Err(CoreError::UpstreamError {
            status,
            provider: provider_name.to_string(),
            model: "<models>".into(),
            body,
        });
    }

    let payload: OpenAIModelsResponse = resp
        .json()
        .await
        .map_err(|e| CoreError::Parse(format!("{} /models parse: {}", provider_name, e)))?;

    let out = payload
        .data
        .into_iter()
        .map(|m| {
            let id = m.id;
            DiscoveredModel {
                model_id: ModelId::new(id.clone()),
                display_name: Some(id),
                target_format: TargetFormat::Openai,
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

// =====================================================================
// Gemini (Google AI Studio)
// =====================================================================

/// Adapter for Google's Gemini API (`generativelanguage.googleapis.com`).
///
/// Gemini uses its own wire format (not OpenAI-compatible):
/// - Auth: `x-goog-api-key: <key>` header
/// - Chat URL: `${base}/models/${model}:generateContent`
/// - Models URL: `${base}/models`
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

impl Default for GeminiAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ProviderAdapter for GeminiAdapter {
    fn id(&self) -> &ProviderId {
        &self.config.id
    }

    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn build_chat_url(&self, _target_format: TargetFormat, model: &ModelId) -> String {
        // Gemini puts the model in the URL path.
        format!(
            "{}/models/{}:generateContent",
            self.config.base_url,
            model.as_str()
        )
    }

    fn build_auth_header(&self, api_key: &str) -> (String, String) {
        ("x-goog-api-key".into(), api_key.to_string())
    }

    fn build_headers(
        &self,
        api_key: &str,
        _target_format: TargetFormat,
        _model: &ModelId,
    ) -> Vec<(String, String)> {
        let (name, value) = self.build_auth_header(api_key);
        vec![
            (name, value),
            ("Content-Type".into(), "application/json".into()),
        ]
    }

    fn models_url(&self) -> Option<String> {
        Some(format!("{}/models", self.config.base_url))
    }

    async fn fetch_models(
        &self,
        client: &reqwest::Client,
        api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        let url = self.models_url().ok_or_else(|| {
            CoreError::Internal("gemini: models_url is None (impossible)".into())
        })?;

        let resp = client
            .get(&url)
            .header("x-goog-api-key", api_key)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| CoreError::UpstreamConnection(format!("gemini /models: {}", e)))?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(CoreError::UpstreamError {
                status,
                provider: "gemini".into(),
                model: "<models>".into(),
                body,
            });
        }

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| CoreError::Parse(format!("gemini /models parse: {}", e)))?;

        // Gemini returns {"models": [{"name": "models/gemini-2.0-flash", ...}]}
        let arr = body
            .get("models")
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                CoreError::Parse("gemini /models: missing 'models' array".into())
            })?;

        let out = arr
            .iter()
            .filter_map(|m| {
                // The model name is "models/gemini-2.0-flash" — strip the prefix.
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
            })
            .collect();
        Ok(out)
    }
}

// =====================================================================
// Antigravity (Cloud Code)
// =====================================================================

/// Adapter for Google's Antigravity (Cloud Code) API.
///
/// Antigravity wraps Gemini requests in a Cloud Code envelope:
/// - Auth: `Authorization: Bearer <token>` (OAuth)
/// - Chat URL: `${base}/v1internal:generateContent`
/// - No model discovery endpoint (models are hardcoded)
pub struct AntigravityAdapter {
    config: ProviderAdapterConfig,
}

impl AntigravityAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("antigravity"),
                base_url: "https://daily-cloudcode-pa.googleapis.com".into(),
                auth_type: AdapterAuthType::Bearer,
                format: AdapterFormat::Gemini,
                extra_headers: vec![],
            },
        }
    }

    /// Remap Antigravity upstream model IDs to client-facing IDs.
    fn remap_antigravity_model_id(upstream_id: &str) -> String {
        match upstream_id {
            "gemini-3.5-flash-extra-low" => "gemini-3.5-flash-low",
            "gemini-3.5-flash-low" => "gemini-3.5-flash-medium",
            "gemini-3.5-flash-medium" => "gemini-3.5-flash-high",
            "gemini-3.5-flash-high" => "gemini-3.5-flash-high",
            "gemini-3-flash-agent" => "gemini-3.5-flash-high",
            _ => upstream_id,
        }.to_string()
    }

    /// Parse fetchAvailableModels response into DiscoveredModel list.
    fn parse_models_response(&self, body: &serde_json::Value) -> Option<Vec<DiscoveredModel>> {
        let models_obj = body.get("models")?.as_object()?;

        let mut models = Vec::new();
        for (model_id, model_data) in models_obj {
            if model_data
                .get("isInternal")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                continue;
            }

            let upstream_id = model_id.clone();
            let client_id = Self::remap_antigravity_model_id(&upstream_id);

            let display_name = model_data
                .get("displayName")
                .and_then(|d| d.as_str())
                .map(|s| s.to_string());

            // Read maxTokens as context_length (fallback to contextLength)
            let context_length = model_data
                .get("maxTokens")
                .and_then(|c| c.as_u64())
                .or_else(|| model_data.get("contextLength").and_then(|c| c.as_u64()))
                .map(|v| v as i64);

            // Read maxOutputTokens as max_output_tokens
            let max_output_tokens = model_data
                .get("maxOutputTokens")
                .and_then(|c| c.as_u64())
                .map(|v| v as i64)
                .or(Some(8192));

            let target_format = if client_id.starts_with("claude") {
                TargetFormat::Anthropic
            } else if client_id.starts_with("gemini") || client_id.starts_with("gpt-oss") {
                TargetFormat::Gemini
            } else {
                TargetFormat::Openai
            };

            // Infer capabilities from upstream fields
            let supports_thinking = model_data
                .get("supportsThinking")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let supports_images = model_data
                .get("supportsImages")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let tool_formatter_type = model_data
                .get("toolFormatterType")
                .and_then(|v| v.as_str())
                .is_some();
            let supports_cumulative_context = model_data
                .get("supportsCumulativeContext")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let capabilities = crate::capabilities::ModelCapabilities {
                vision: Some(supports_images),
                tool_calling: Some(tool_formatter_type || supports_cumulative_context),
                reasoning: Some(supports_thinking),
                thinking: Some(supports_thinking),
                attachment: Some(supports_images),
                structured_output: None,
                temperature: None,
            };

            models.push(DiscoveredModel {
                model_id: ModelId::new(client_id),
                display_name,
                target_format,
                context_length,
                max_output_tokens,
                input_modalities: None,
                output_modalities: None,
                model_type: Some("chat".to_string()),
                family: None,
                capabilities: Some(capabilities),
            });
        }

        if models.is_empty() {
            None
        } else {
            Some(models)
        }
    }

    /// Hardcoded model catalog for when no OAuth token is available.
    fn hardcoded_models(&self) -> Vec<DiscoveredModel> {
        vec![
            DiscoveredModel {
                model_id: ModelId::new("gemini-2.5-pro"),
                display_name: Some("Gemini 2.5 Pro".to_string()),
                target_format: TargetFormat::Gemini,
                context_length: Some(1_048_576),
                max_output_tokens: Some(8192),
                input_modalities: None,
                output_modalities: None,
                model_type: Some("chat".to_string()),
                family: None,
                capabilities: Some(crate::capabilities::ModelCapabilities {
                    vision: Some(true),
                    tool_calling: Some(true),
                    reasoning: None,
                    thinking: None,
                    attachment: None,
                    structured_output: None,
                    temperature: None,
                }),
            },
            DiscoveredModel {
                model_id: ModelId::new("gemini-2.5-flash"),
                display_name: Some("Gemini 2.5 Flash".to_string()),
                target_format: TargetFormat::Gemini,
                context_length: Some(1_048_576),
                max_output_tokens: Some(8192),
                input_modalities: None,
                output_modalities: None,
                model_type: Some("chat".to_string()),
                family: None,
                capabilities: Some(crate::capabilities::ModelCapabilities {
                    vision: Some(true),
                    tool_calling: Some(true),
                    reasoning: None,
                    thinking: None,
                    attachment: None,
                    structured_output: None,
                    temperature: None,
                }),
            },
            DiscoveredModel {
                model_id: ModelId::new("claude-sonnet-4-6"),
                display_name: Some("Claude Sonnet 4.6".to_string()),
                target_format: TargetFormat::Anthropic,
                context_length: Some(200_000),
                max_output_tokens: Some(8192),
                input_modalities: None,
                output_modalities: None,
                model_type: Some("chat".to_string()),
                family: None,
                capabilities: Some(crate::capabilities::ModelCapabilities {
                    vision: Some(true),
                    tool_calling: Some(true),
                    reasoning: None,
                    thinking: None,
                    attachment: None,
                    structured_output: None,
                    temperature: None,
                }),
            },
            DiscoveredModel {
                model_id: ModelId::new("claude-opus-4-6-thinking"),
                display_name: Some("Claude Opus 4.6 (Thinking)".to_string()),
                target_format: TargetFormat::Anthropic,
                context_length: Some(200_000),
                max_output_tokens: Some(8192),
                input_modalities: None,
                output_modalities: None,
                model_type: Some("chat".to_string()),
                family: None,
                capabilities: Some(crate::capabilities::ModelCapabilities {
                    vision: Some(true),
                    tool_calling: Some(true),
                    reasoning: None,
                    thinking: None,
                    attachment: None,
                    structured_output: None,
                    temperature: None,
                }),
            },
        ]
    }
}

impl Default for AntigravityAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ProviderAdapter for AntigravityAdapter {
    fn id(&self) -> &ProviderId {
        &self.config.id
    }

    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn build_chat_url(&self, _target_format: TargetFormat, _model: &ModelId) -> String {
        // Antigravity uses the Cloud Code endpoint; model goes in the body.
        format!("{}/v1internal:generateContent", self.config.base_url)
    }

    fn build_auth_header(&self, api_key: &str) -> (String, String) {
        ("Authorization".into(), format!("Bearer {}", api_key))
    }

    fn build_headers(
        &self,
        api_key: &str,
        _target_format: TargetFormat,
        _model: &ModelId,
    ) -> Vec<(String, String)> {
        let (name, value) = self.build_auth_header(api_key);
        vec![
            (name, value),
            ("Content-Type".into(), "application/json".into()),
        ]
    }

    fn models_url(&self) -> Option<String> {
        // Antigravity does not expose a /models endpoint.
        None
    }

    async fn fetch_models(
        &self,
        http_client: &reqwest::Client,
        api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        if api_key.is_empty() {
            return Ok(self.hardcoded_models());
        }

        let endpoints = [
            "https://daily-cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels",
            "https://cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels",
        ];

        let ua = "Antigravity/4.2.0 (X11; Linux x86_64) Chrome/132.0.6834.160 Electron/39.2.3";

        for endpoint in &endpoints {
            let response = http_client
                .post(*endpoint)
                .header("Authorization", format!("Bearer {api_key}"))
                .header("Content-Type", "application/json")
                .header("User-Agent", ua)
                .header("X-Goog-Api-Client", "gl-node/22.21.1")
                .body("{}".to_string())
                .send()
                .await;

            if let Ok(resp) = response {
                if resp.status().is_success() {
                    if let Ok(body) = resp.json::<serde_json::Value>().await {
                        if let Some(models) = self.parse_models_response(&body) {
                            return Ok(models);
                        }
                    }
                }
            }
        }

        Ok(self.hardcoded_models())
    }
}

// =====================================================================
// Antigravity CLI
// =====================================================================

/// Adapter for Google's Antigravity CLI (Cloud Code Assist).
///
/// Same backend as Antigravity but with a different (larger) model catalog.
/// Uses the same Cloud Code envelope and OAuth Bearer auth.
pub struct AntigravityCliAdapter {
    config: ProviderAdapterConfig,
}

impl AntigravityCliAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("antigravity-cli"),
                base_url: "https://daily-cloudcode-pa.googleapis.com".into(),
                auth_type: AdapterAuthType::Bearer,
                format: AdapterFormat::Gemini,
                extra_headers: vec![],
            },
        }
    }

    /// Remap Antigravity upstream model IDs to client-facing IDs.
    fn remap_antigravity_model_id(upstream_id: &str) -> String {
        match upstream_id {
            "gemini-3.5-flash-extra-low" => "gemini-3.5-flash-low",
            "gemini-3.5-flash-low" => "gemini-3.5-flash-medium",
            "gemini-3.5-flash-medium" => "gemini-3.5-flash-high",
            "gemini-3.5-flash-high" => "gemini-3.5-flash-high",
            "gemini-3-flash-agent" => "gemini-3.5-flash-high",
            _ => upstream_id,
        }.to_string()
    }

    /// Parse fetchAvailableModels response into DiscoveredModel list.
    fn parse_models_response(&self, body: &serde_json::Value) -> Option<Vec<DiscoveredModel>> {
        let models_obj = body.get("models")?.as_object()?;

        let mut models = Vec::new();
        for (model_id, model_data) in models_obj {
            if model_data
                .get("isInternal")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                continue;
            }

            let upstream_id = model_id.clone();
            let client_id = Self::remap_antigravity_model_id(&upstream_id);

            let display_name = model_data
                .get("displayName")
                .and_then(|d| d.as_str())
                .map(|s| s.to_string());

            // Read maxTokens as context_length (fallback to contextLength)
            let context_length = model_data
                .get("maxTokens")
                .and_then(|c| c.as_u64())
                .or_else(|| model_data.get("contextLength").and_then(|c| c.as_u64()))
                .map(|v| v as i64);

            // Read maxOutputTokens as max_output_tokens
            let max_output_tokens = model_data
                .get("maxOutputTokens")
                .and_then(|c| c.as_u64())
                .map(|v| v as i64)
                .or(Some(8192));

            let target_format = if client_id.starts_with("claude") {
                TargetFormat::Anthropic
            } else if client_id.starts_with("gemini") || client_id.starts_with("gpt-oss") {
                TargetFormat::Gemini
            } else {
                TargetFormat::Openai
            };

            // Infer capabilities from upstream fields
            let supports_thinking = model_data
                .get("supportsThinking")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let supports_images = model_data
                .get("supportsImages")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let tool_formatter_type = model_data
                .get("toolFormatterType")
                .and_then(|v| v.as_str())
                .is_some();
            let supports_cumulative_context = model_data
                .get("supportsCumulativeContext")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let capabilities = crate::capabilities::ModelCapabilities {
                vision: Some(supports_images),
                tool_calling: Some(tool_formatter_type || supports_cumulative_context),
                reasoning: Some(supports_thinking),
                thinking: Some(supports_thinking),
                attachment: Some(supports_images),
                structured_output: None,
                temperature: None,
            };

            models.push(DiscoveredModel {
                model_id: ModelId::new(client_id),
                display_name,
                target_format,
                context_length,
                max_output_tokens,
                input_modalities: None,
                output_modalities: None,
                model_type: Some("chat".to_string()),
                family: None,
                capabilities: Some(capabilities),
            });
        }

        if models.is_empty() {
            None
        } else {
            Some(models)
        }
    }

    /// Hardcoded model catalog for when no OAuth token is available.
    fn hardcoded_models(&self) -> Vec<DiscoveredModel> {
        vec![
            DiscoveredModel {
                model_id: ModelId::new("gemini-2.5-pro"),
                display_name: Some("Gemini 2.5 Pro".to_string()),
                target_format: TargetFormat::Gemini,
                context_length: Some(1_048_576),
                max_output_tokens: Some(8192),
                input_modalities: None,
                output_modalities: None,
                model_type: Some("chat".to_string()),
                family: None,
                capabilities: Some(crate::capabilities::ModelCapabilities {
                    vision: Some(true),
                    tool_calling: Some(true),
                    reasoning: None,
                    thinking: None,
                    attachment: None,
                    structured_output: None,
                    temperature: None,
                }),
            },
            DiscoveredModel {
                model_id: ModelId::new("gemini-2.5-flash"),
                display_name: Some("Gemini 2.5 Flash".to_string()),
                target_format: TargetFormat::Gemini,
                context_length: Some(1_048_576),
                max_output_tokens: Some(8192),
                input_modalities: None,
                output_modalities: None,
                model_type: Some("chat".to_string()),
                family: None,
                capabilities: Some(crate::capabilities::ModelCapabilities {
                    vision: Some(true),
                    tool_calling: Some(true),
                    reasoning: None,
                    thinking: None,
                    attachment: None,
                    structured_output: None,
                    temperature: None,
                }),
            },
            DiscoveredModel {
                model_id: ModelId::new("claude-sonnet-4-6"),
                display_name: Some("Claude Sonnet 4.6".to_string()),
                target_format: TargetFormat::Anthropic,
                context_length: Some(200_000),
                max_output_tokens: Some(8192),
                input_modalities: None,
                output_modalities: None,
                model_type: Some("chat".to_string()),
                family: None,
                capabilities: Some(crate::capabilities::ModelCapabilities {
                    vision: Some(true),
                    tool_calling: Some(true),
                    reasoning: None,
                    thinking: None,
                    attachment: None,
                    structured_output: None,
                    temperature: None,
                }),
            },
            DiscoveredModel {
                model_id: ModelId::new("claude-opus-4-6-thinking"),
                display_name: Some("Claude Opus 4.6 (Thinking)".to_string()),
                target_format: TargetFormat::Anthropic,
                context_length: Some(200_000),
                max_output_tokens: Some(8192),
                input_modalities: None,
                output_modalities: None,
                model_type: Some("chat".to_string()),
                family: None,
                capabilities: Some(crate::capabilities::ModelCapabilities {
                    vision: Some(true),
                    tool_calling: Some(true),
                    reasoning: None,
                    thinking: None,
                    attachment: None,
                    structured_output: None,
                    temperature: None,
                }),
            },
        ]
    }
}

impl Default for AntigravityCliAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ProviderAdapter for AntigravityCliAdapter {
    fn id(&self) -> &ProviderId {
        &self.config.id
    }

    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn build_chat_url(&self, _target_format: TargetFormat, _model: &ModelId) -> String {
        format!("{}/v1internal:generateContent", self.config.base_url)
    }

    fn build_auth_header(&self, api_key: &str) -> (String, String) {
        ("Authorization".into(), format!("Bearer {}", api_key))
    }

    fn build_headers(
        &self,
        api_key: &str,
        _target_format: TargetFormat,
        _model: &ModelId,
    ) -> Vec<(String, String)> {
        let (name, value) = self.build_auth_header(api_key);
        vec![
            (name, value),
            ("Content-Type".into(), "application/json".into()),
        ]
    }

    fn models_url(&self) -> Option<String> {
        None
    }

    async fn fetch_models(
        &self,
        http_client: &reqwest::Client,
        api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        if api_key.is_empty() {
            return Ok(self.hardcoded_models());
        }

        let endpoints = [
            "https://daily-cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels",
            "https://cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels",
        ];

        let ua = "Antigravity/4.2.0 (X11; Linux x86_64) Chrome/132.0.6834.160 Electron/39.2.3";

        for endpoint in &endpoints {
            let response = http_client
                .post(*endpoint)
                .header("Authorization", format!("Bearer {api_key}"))
                .header("Content-Type", "application/json")
                .header("User-Agent", ua)
                .header("X-Goog-Api-Client", "gl-node/22.21.1")
                .body("{}".to_string())
                .send()
                .await;

            if let Ok(resp) = response {
                if resp.status().is_success() {
                    if let Ok(body) = resp.json::<serde_json::Value>().await {
                        if let Some(models) = self.parse_models_response(&body) {
                            return Ok(models);
                        }
                    }
                }
            }
        }

        Ok(self.hardcoded_models())
    }
}

// =====================================================================
// Factory
// =====================================================================

/// Return a `Vec<Arc<dyn ProviderAdapter>>` containing every built-in adapter.
///
/// The order matches the expected "popularity" order: OpenRouter, then
/// MiniMax, then OpenCode Zen, then Ollama Cloud, then the remaining
/// OpenAI-compatible providers, then Gemini and Antigravity. Callers may
/// reorder, filter, or wrap the results.
pub fn builtin_adapters() -> Vec<Arc<dyn ProviderAdapter>> {
    vec![
        Arc::new(OpenRouterAdapter::new()),
        Arc::new(MiniMaxAdapter::new()),
        Arc::new(OpenCodeZenAdapter::new()),
        Arc::new(OllamaCloudAdapter::new()),
        Arc::new(NousResearchAdapter::new()),
        Arc::new(NvidiaNimAdapter::new()),
        Arc::new(KilocodeAdapter::new()),
        Arc::new(GeminiAdapter::new()),
        Arc::new(AntigravityAdapter::new()),
        Arc::new(AntigravityCliAdapter::new()),
    ]
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn first_header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn has_header(headers: &[(String, String)], name: &str) -> bool {
        headers.iter().any(|(k, _)| k == name)
    }

    // ---- OpenRouter -----------------------------------------------------

    #[test]
    fn openrouter_builds_correct_url() {
        let a = OpenRouterAdapter::new();
        let url = a.build_chat_url(TargetFormat::Openai, &ModelId::new("any"));
        assert_eq!(url, "https://openrouter.ai/api/v1/chat/completions");
        // target_format is ignored: still /chat/completions.
        let url2 = a.build_chat_url(TargetFormat::Anthropic, &ModelId::new("any"));
        assert_eq!(url2, url);
    }

    #[test]
    fn openrouter_builds_bearer_auth() {
        let a = OpenRouterAdapter::new();
        let (name, value) = a.build_auth_header("sk-test-123");
        assert_eq!(name, "Authorization");
        assert_eq!(value, "Bearer sk-test-123");
    }

    #[test]
    fn openrouter_models_url() {
        let a = OpenRouterAdapter::new();
        assert_eq!(
            a.models_url().as_deref(),
            Some("https://openrouter.ai/api/v1/models")
        );
    }

    #[test]
    fn openrouter_headers_include_referer_and_content_type() {
        let a = OpenRouterAdapter::new();
        let headers = a.build_headers("k", TargetFormat::Openai, &ModelId::new("any"));
        assert_eq!(first_header(&headers, "Authorization"), Some("Bearer k"));
        assert_eq!(first_header(&headers, "Content-Type"), Some("application/json"));
        assert_eq!(first_header(&headers, "HTTP-Referer"), Some("https://openproxy.local"));
        assert_eq!(first_header(&headers, "X-Title"), Some("openproxy"));
    }

    // ---- MiniMax -----------------------------------------------------

    #[test]
    fn minimax_builds_messages_url_with_beta() {
        let a = MiniMaxAdapter::new();
        let url = a.build_chat_url(TargetFormat::Anthropic, &ModelId::new("m"));
        assert_eq!(url, "https://api.minimax.io/anthropic/v1/messages?beta=true");
    }

    #[test]
    fn minimax_models_url_points_at_v1_models() {
        let a = MiniMaxAdapter::new();
        assert_eq!(
            a.models_url().as_deref(),
            Some("https://api.minimax.io/v1/models")
        );
    }

    #[test]
    fn minimax_builds_anthropic_headers() {
        let a = MiniMaxAdapter::new();
        let headers = a.build_headers("k", TargetFormat::Anthropic, &ModelId::new("m"));
        assert_eq!(first_header(&headers, "Authorization"), Some("Bearer k"));
        assert_eq!(first_header(&headers, "Content-Type"), Some("application/json"));
        assert_eq!(first_header(&headers, "Anthropic-Version"), Some("2023-06-01"));
    }

    // ---- OpenCode Zen ------------------------------------------------

    #[test]
    fn opencode_zen_routes_anthropic_to_messages() {
        let a = OpenCodeZenAdapter::new();
        let url = a.build_chat_url(TargetFormat::Anthropic, &ModelId::new("m"));
        assert_eq!(url, "https://opencode.ai/zen/v1/messages");
    }

    #[test]
    fn opencode_zen_routes_openai_to_chat_completions() {
        let a = OpenCodeZenAdapter::new();
        let url = a.build_chat_url(TargetFormat::Openai, &ModelId::new("m"));
        assert_eq!(url, "https://opencode.ai/zen/v1/chat/completions");
    }

    #[test]
    fn opencode_zen_uses_x_api_key_for_anthropic() {
        let a = OpenCodeZenAdapter::new();
        let headers = a.build_headers("k-anthropic", TargetFormat::Anthropic, &ModelId::new("m"));
        assert_eq!(first_header(&headers, "x-api-key"), Some("k-anthropic"));
        // No Bearer auth on the Anthropic branch.
        assert!(first_header(&headers, "Authorization").is_none());
        // Anthropic-Version must be present.
        assert_eq!(first_header(&headers, "Anthropic-Version"), Some("2023-06-01"));
    }

    #[test]
    fn opencode_zen_uses_bearer_for_openai() {
        let a = OpenCodeZenAdapter::new();
        let headers = a.build_headers("k-openai", TargetFormat::Openai, &ModelId::new("m"));
        assert_eq!(first_header(&headers, "Authorization"), Some("Bearer k-openai"));
        // No x-api-key on the OpenAI branch.
        assert!(first_header(&headers, "x-api-key").is_none());
        // No Anthropic-Version on the OpenAI branch.
        assert!(first_header(&headers, "Anthropic-Version").is_none());
    }

    #[test]
    fn opencode_zen_skips_auth_when_key_empty() {
        let a = OpenCodeZenAdapter::new();
        let headers = a.build_headers("", TargetFormat::Openai, &ModelId::new("m"));
        // No auth headers when key is empty.
        assert!(first_header(&headers, "Authorization").is_none());
        assert!(first_header(&headers, "x-api-key").is_none());
        // Content-Type and User-Agent are still present.
        assert_eq!(first_header(&headers, "Content-Type"), Some("application/json"));
        assert_eq!(first_header(&headers, "User-Agent"), Some("openproxy/0.1"));
    }

    #[test]
    fn opencode_zen_headers_have_user_agent_and_content_type() {
        let a = OpenCodeZenAdapter::new();
        for fmt in [TargetFormat::Openai, TargetFormat::Anthropic] {
            let headers = a.build_headers("k", fmt, &ModelId::new("m"));
            assert_eq!(first_header(&headers, "User-Agent"), Some("openproxy/0.1"));
            assert_eq!(first_header(&headers, "Content-Type"), Some("application/json"));
            assert!(has_header(&headers, "Content-Type"));
        }
    }

    #[test]
    fn opencode_zen_models_url() {
        let a = OpenCodeZenAdapter::new();
        assert_eq!(
            a.models_url().as_deref(),
            Some("https://opencode.ai/zen/v1/models")
        );
    }

    #[test]
    fn classify_zen_target_format_heuristic() {
        assert_eq!(
            classify_zen_target_format("claude-sonnet-4"),
            TargetFormat::Anthropic
        );
        assert_eq!(
            classify_zen_target_format("MiniMax-M2"),
            TargetFormat::Anthropic
        );
        assert_eq!(
            classify_zen_target_format("gpt-4o"),
            TargetFormat::Openai
        );
        assert_eq!(
            classify_zen_target_format("llama-3.1-70b"),
            TargetFormat::Openai
        );
    }

    // ---- Factory -----------------------------------------------------

    #[test]
    fn builtin_adapters_returns_ten() {
        let v = builtin_adapters();
        assert_eq!(v.len(), 10);
        let ids: Vec<&str> = v.iter().map(|a| a.id().as_str()).collect();
        assert!(ids.contains(&"openrouter"));
        assert!(ids.contains(&"minimax"));
        assert!(ids.contains(&"opencode-zen"));
        assert!(ids.contains(&"ollama-cloud"));
        assert!(ids.contains(&"nous-research"));
        assert!(ids.contains(&"nvidia-nim"));
        assert!(ids.contains(&"kilocode"));
        assert!(ids.contains(&"gemini"));
        assert!(ids.contains(&"antigravity"));
        assert!(ids.contains(&"antigravity-cli"));
    }

    // ---- Ollama Cloud ------------------------------------------------

    #[test]
    fn ollama_cloud_builds_correct_url() {
        let a = OllamaCloudAdapter::new();
        let url = a.build_chat_url(TargetFormat::Openai, &ModelId::new("any"));
        assert_eq!(url, "https://ollama.com/v1/chat/completions");
    }

    #[test]
    fn ollama_cloud_builds_bearer_auth() {
        let a = OllamaCloudAdapter::new();
        let (name, value) = a.build_auth_header("test-key");
        assert_eq!(name, "Authorization");
        assert_eq!(value, "Bearer test-key");
    }

    #[test]
    fn ollama_cloud_models_url() {
        let a = OllamaCloudAdapter::new();
        assert_eq!(
            a.models_url().as_deref(),
            Some("https://ollama.com/api/tags")
        );
    }

    #[test]
    fn ollama_cloud_headers() {
        let a = OllamaCloudAdapter::new();
        let headers = a.build_headers("k", TargetFormat::Openai, &ModelId::new("any"));
        assert_eq!(first_header(&headers, "Authorization"), Some("Bearer k"));
        assert_eq!(first_header(&headers, "Content-Type"), Some("application/json"));
    }

    // ---- Nous Research ------------------------------------------------

    #[test]
    fn nous_research_builds_correct_url() {
        let a = NousResearchAdapter::new();
        let url = a.build_chat_url(TargetFormat::Openai, &ModelId::new("Hermes-4-405B"));
        assert_eq!(url, "https://inference-api.nousresearch.com/v1/chat/completions");
    }

    #[test]
    fn nous_research_builds_bearer_auth() {
        let a = NousResearchAdapter::new();
        let (name, value) = a.build_auth_header("nr-key");
        assert_eq!(name, "Authorization");
        assert_eq!(value, "Bearer nr-key");
    }

    #[test]
    fn nous_research_models_url() {
        let a = NousResearchAdapter::new();
        assert_eq!(
            a.models_url().as_deref(),
            Some("https://inference-api.nousresearch.com/v1/models")
        );
    }

    #[test]
    fn nous_research_headers() {
        let a = NousResearchAdapter::new();
        let headers = a.build_headers("k", TargetFormat::Openai, &ModelId::new("any"));
        assert_eq!(first_header(&headers, "Authorization"), Some("Bearer k"));
        assert_eq!(first_header(&headers, "Content-Type"), Some("application/json"));
    }

    // ---- NVIDIA NIM ---------------------------------------------------

    #[test]
    fn nvidia_nim_builds_correct_url() {
        let a = NvidiaNimAdapter::new();
        let url = a.build_chat_url(TargetFormat::Openai, &ModelId::new("nvidia/nemotron-3-super-120b-a12b"));
        assert_eq!(url, "https://integrate.api.nvidia.com/v1/chat/completions");
    }

    #[test]
    fn nvidia_nim_builds_bearer_auth() {
        let a = NvidiaNimAdapter::new();
        let (name, value) = a.build_auth_header("nvapi-test");
        assert_eq!(name, "Authorization");
        assert_eq!(value, "Bearer nvapi-test");
    }

    #[test]
    fn nvidia_nim_models_url() {
        let a = NvidiaNimAdapter::new();
        assert_eq!(
            a.models_url().as_deref(),
            Some("https://integrate.api.nvidia.com/v1/models")
        );
    }

    #[test]
    fn nvidia_nim_headers() {
        let a = NvidiaNimAdapter::new();
        let headers = a.build_headers("k", TargetFormat::Openai, &ModelId::new("any"));
        assert_eq!(first_header(&headers, "Authorization"), Some("Bearer k"));
        assert_eq!(first_header(&headers, "Content-Type"), Some("application/json"));
    }

    // ---- Kilocode -----------------------------------------------------

    #[test]
    fn kilocode_builds_correct_url() {
        let a = KilocodeAdapter::new();
        let url = a.build_chat_url(TargetFormat::Openai, &ModelId::new("openai/gpt-5.5"));
        assert_eq!(url, "https://api.kilo.ai/api/openrouter/v1/chat/completions");
    }

    #[test]
    fn kilocode_builds_bearer_auth() {
        let a = KilocodeAdapter::new();
        let (name, value) = a.build_auth_header("kl-key");
        assert_eq!(name, "Authorization");
        assert_eq!(value, "Bearer kl-key");
    }

    #[test]
    fn kilocode_models_url() {
        let a = KilocodeAdapter::new();
        assert_eq!(
            a.models_url().as_deref(),
            Some("https://api.kilo.ai/api/openrouter/models")
        );
    }

    #[test]
    fn kilocode_headers() {
        let a = KilocodeAdapter::new();
        let headers = a.build_headers("k", TargetFormat::Openai, &ModelId::new("any"));
        assert_eq!(first_header(&headers, "Authorization"), Some("Bearer k"));
        assert_eq!(first_header(&headers, "Content-Type"), Some("application/json"));
    }

    // ---- Gemini -------------------------------------------------------

    #[test]
    fn gemini_builds_correct_url() {
        let a = GeminiAdapter::new();
        let url = a.build_chat_url(TargetFormat::Gemini, &ModelId::new("gemini-2.5-flash"));
        assert_eq!(
            url,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:generateContent"
        );
    }

    #[test]
    fn gemini_builds_goog_api_key_auth() {
        let a = GeminiAdapter::new();
        let (name, value) = a.build_auth_header("AIzaSyTest123");
        assert_eq!(name, "x-goog-api-key");
        assert_eq!(value, "AIzaSyTest123");
    }

    #[test]
    fn gemini_models_url() {
        let a = GeminiAdapter::new();
        assert_eq!(
            a.models_url().as_deref(),
            Some("https://generativelanguage.googleapis.com/v1beta/models")
        );
    }

    #[test]
    fn gemini_headers_include_content_type() {
        let a = GeminiAdapter::new();
        let headers = a.build_headers("k", TargetFormat::Gemini, &ModelId::new("any"));
        assert_eq!(first_header(&headers, "x-goog-api-key"), Some("k"));
        assert_eq!(first_header(&headers, "Content-Type"), Some("application/json"));
    }

    // ---- Antigravity ---------------------------------------------------

    #[test]
    fn antigravity_builds_correct_url() {
        let a = AntigravityAdapter::new();
        let url = a.build_chat_url(TargetFormat::Gemini, &ModelId::new("claude-opus-4-6"));
        assert_eq!(
            url,
            "https://daily-cloudcode-pa.googleapis.com/v1internal:generateContent"
        );
    }

    #[test]
    fn antigravity_builds_bearer_auth() {
        let a = AntigravityAdapter::new();
        let (name, value) = a.build_auth_header("ya29.test-token");
        assert_eq!(name, "Authorization");
        assert_eq!(value, "Bearer ya29.test-token");
    }

    #[test]
    fn antigravity_has_no_models_url() {
        let a = AntigravityAdapter::new();
        assert!(a.models_url().is_none());
    }

    // ---- Antigravity CLI -----------------------------------------------

    #[test]
    fn antigravity_cli_builds_correct_url() {
        let a = AntigravityCliAdapter::new();
        let url = a.build_chat_url(TargetFormat::Gemini, &ModelId::new("gemini-3.5-flash"));
        assert_eq!(
            url,
            "https://daily-cloudcode-pa.googleapis.com/v1internal:generateContent"
        );
    }

    #[test]
    fn antigravity_cli_builds_bearer_auth() {
        let a = AntigravityCliAdapter::new();
        let (name, value) = a.build_auth_header("ya29.cli-token");
        assert_eq!(name, "Authorization");
        assert_eq!(value, "Bearer ya29.cli-token");
    }

    #[test]
    fn antigravity_cli_has_no_models_url() {
        let a = AntigravityCliAdapter::new();
        assert!(a.models_url().is_none());
    }
}
