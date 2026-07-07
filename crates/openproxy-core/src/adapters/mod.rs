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
use crate::upstream::{CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest};
use async_trait::async_trait;
use bytes::Bytes;
use http::HeaderValue;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Extra state available to provider-specific executors.
///
/// Standard upstream dispatch persists cooldowns and usage centrally.
/// Custom executors bypass that path, so providers that learn quota state
/// from proprietary response headers need this small persistence hook.
#[derive(Clone)]
pub struct CustomExecutionContext {
    pub conn: Arc<parking_lot::Mutex<rusqlite::Connection>>,
    pub cooldown_mode: crate::combos::CooldownMode,
    pub cooldown_base_secs: u64,
    pub cooldown_max_secs: u64,
    pub cooldown_factor: u32,
}

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
///
/// # Why `#[async_trait]` is still here (edition 2024 migration)
///
/// The workspace was upgraded to Rust edition 2024 / rustc 1.96+, which
/// supports native `async fn` in traits. We intentionally keep
/// `#[async_trait]` on this trait (and every `impl ProviderAdapter for ...`
/// block in the codebase) because the trait is used as a *trait object* —
/// `Arc<dyn ProviderAdapter>` shows up in `PipelineState.adapters`,
/// `DiscoveryScheduler` task arguments, `admin::refresh_models`, the
/// Gate-C E2E test, and roughly 60+ other call sites.
///
/// Native `async fn` in a trait is **not** dyn-safe: the compiler cannot
/// vtable a method whose return type is an `impl Future` (the future's
/// size is unbounded). The `#[async_trait]` macro desugars each
/// `async fn foo(&self, ...) -> T` into
/// `fn foo(&self, ...) -> Pin<Box<dyn Future<Output = T> + Send + '_>>`,
/// which IS dyn-safe at the cost of one heap allocation per call (the
/// "Box pinning + dynamic dispatch" overhead).
///
/// Eliminating that overhead would require either:
/// - **Enum dispatch** (a `BuiltinAdapter` enum wrapping the 12 concrete
///   impls) — not viable here because `CustomAdapter` is constructed from
///   runtime config and registered dynamically, so the set of adapters
///   is not closed at compile time.
/// - **Generic dispatch** at every call site (`impl ProviderAdapter`) —
///   would require refactoring 60+ sites and breaking the
///   `Arc<Vec<Arc<dyn ProviderAdapter>>>` registry pattern that
///   `PipelineState` and `DiscoveryScheduler` rely on for runtime
///   iteration over a heterogeneous adapter collection.
///
/// Both alternatives are large architectural refactors outside the scope
/// of the edition-2024 / `#[async_trait]` migration. The runtime cost
/// (one Box per `fetch_models` call, which already does network I/O)
/// is negligible relative to the work each call performs.
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

    /// Build the chat URL with account-level context (label).
    /// Default: ignores label and delegates to build_chat_url.
    fn build_chat_url_for_account(
        &self,
        target_format: TargetFormat,
        model: &ModelId,
        _account_label: &str,
    ) -> String {
        self.build_chat_url(target_format, model)
    }

    /// Build the URL for audio transcription (Whisper). Only
    /// OpenAI-compatible providers that mirror OpenAI's
    /// `/v1/audio/transcriptions` endpoint support this.
    ///
    /// Default: `{base_url}/audio/transcriptions`.
    ///
    /// The handler does NOT route through the chat `Pipeline` (which
    /// is deeply coupled to JSON/SSE/tokens); it builds its own
    /// upstream call and uses this URL. Providers that don't expose a
    /// Whisper endpoint can override this to return a clearly-marked
    /// sentinel URL — the upstream will 404 and the error will
    /// surface to the client.
    fn build_transcription_url(&self) -> String {
        format!("{}/audio/transcriptions", self.config().base_url)
    }

    /// Build the URL for embeddings. Default: `{base_url}/embeddings`.
    ///
    /// The handler does NOT route through the chat `Pipeline`; it builds
    /// its own upstream call and uses this URL. Providers that don't
    /// expose an embeddings endpoint can override this to return a
    /// clearly-marked sentinel URL — the upstream will 404 and the
    /// error will surface to the client.
    fn build_embeddings_url(&self) -> String {
        format!("{}/embeddings", self.config().base_url)
    }

    /// Build the URL for image generation. Default:
    /// `{base_url}/images/generations`.
    ///
    /// The handler does NOT route through the chat `Pipeline`; it builds
    /// its own upstream call and uses this URL. Providers that don't
    /// expose an image endpoint can override this to return a
    /// clearly-marked sentinel URL — the upstream will 404 and the
    /// error will surface to the client.
    fn build_image_url(&self) -> String {
        format!("{}/images/generations", self.config().base_url)
    }

    /// Build the URL for video generation. Default:
    /// `{base_url}/video/generations`.
    ///
    /// The handler does NOT route through the chat `Pipeline`; it builds
    /// its own upstream call and uses this URL. Providers that don't
    /// expose a video endpoint can override this to return a
    /// clearly-marked sentinel URL — the upstream will 404 and the
    /// error will surface to the client.
    fn build_video_url(&self) -> String {
        format!("{}/video/generations", self.config().base_url)
    }

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

    /// Models URL with account-level context (label).
    /// Default: ignores label and delegates to models_url.
    fn models_url_for_account(&self, _account_label: &str) -> Option<String> {
        self.models_url()
    }

    /// Fetch the live model list using the provided hyper-based
    /// upstream client and API key.
    ///
    /// The default implementation GETs [`Self::models_url`] with a
    /// `Bearer` auth header and parses an OpenRouter-style
    /// `{"data": [{...}]}` payload. Providers with a different
    /// response shape override this method. As of Gate 6 the
    /// HTTP transport is the [`UpstreamClient`] (hyper-based, with
    /// per-phase timeouts); the legacy `reqwest::Client` is no
    /// longer threaded through this trait.
    async fn fetch_models(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
    ) -> Result<Vec<DiscoveredModel>>;

    /// Fetch models with account-level context (label).
    /// Default: ignores label and delegates to fetch_models.
    async fn fetch_models_for_account(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
        _account_label: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        self.fetch_models(upstream_client, api_key).await
    }

    /// Normalize an OpenAI request view before serialization.
    /// Default: pass through unchanged.
    fn normalize_openai_request(&self, _view: &mut crate::translation::OpenAIRequestView) {}

    /// Execute the request completely within the adapter.
    /// If this returns `Some(...)`, the standard pipeline dispatch is skipped.
    /// Default implementation returns `None`.
    async fn execute_custom(
        &self,
        _upstream_client: &Arc<crate::upstream::UpstreamClient>,
        _req: Arc<crate::pipeline::PipelineRequest>,
        _resolved_target: &crate::pipeline::context::ResolvedTarget,
        _ctx: Option<CustomExecutionContext>,
    ) -> Option<std::result::Result<crate::translation::OpenAIResponse, crate::error::CoreError>> {
        None
    }
}


pub mod openrouter;
pub mod minimax;
pub mod opencode_zen;
pub mod ollama_cloud;
pub mod nous_research;
pub mod nvidia_nim;
pub mod kilocode;
pub mod cloudflare_workers_ai;
pub mod gemini;
pub mod antigravity;
pub mod codex;
pub mod kiro_ai;
pub mod custom_adapter;

pub use openrouter::OpenRouterAdapter;
pub use minimax::MiniMaxAdapter;
pub use opencode_zen::OpenCodeZenAdapter;
pub use ollama_cloud::OllamaCloudAdapter;
pub use nous_research::NousResearchAdapter;
pub use nvidia_nim::NvidiaNimAdapter;
pub use kilocode::KilocodeAdapter;
pub use cloudflare_workers_ai::CloudflareWorkersAIAdapter;
pub use gemini::GeminiAdapter;
pub use antigravity::AntigravityAdapter;
pub use codex::CodexAdapter;
pub use kiro_ai::KiroAdapter;
pub use custom_adapter::CustomAdapter;

// =====================================================================
// Shared upstream helpers
// =====================================================================

/// GET `url` via the [`UpstreamClient`] with the given `(header, value)`
/// pairs attached, then collect and parse the body as a JSON value.
///
/// The previous helper chain
/// (`client.get(...).header(...).send().await` + `resp.json().await`)
/// was the same across eight of the ten adapters; consolidating it here
/// drops ~25 lines per impl and keeps the
/// `TimeoutProfile::ModelDiscovery` knob in one place.
///
/// The caller is responsible for mapping
/// [`crate::upstream::UpstreamError`] into the provider-specific
/// [`CoreError`] (most call sites return `CoreError::UpstreamConnection`
/// on transport failure and `CoreError::Parse` on JSON failure).
pub(crate) async fn upstream_get_json(
    upstream_client: &Arc<UpstreamClient>,
    url: &str,
    headers: &[(&str, String)],
) -> std::result::Result<serde_json::Value, String> {
    let mut req = UpstreamRequest::get(url);
    for (k, v) in headers {
        if let Ok(hv) = HeaderValue::from_str(v) {
            // Map common header names to typed `http::header` constants
            // so case-insensitive matching works; fall back to a raw
            // insertion when the name is non-standard.
            if let Some(name) = header_name(k) {
                req.headers.insert(name, hv);
            } else {
                req.headers.insert(
                    http::header::HeaderName::from_bytes(k.as_bytes())
                        .map_err(|e| format!("invalid header name '{}': {}", k, e))?,
                    hv,
                );
            }
        }
    }
    let cancel = CancellationToken::new();
    let response = upstream_client
        .call(req, TimeoutProfile::ModelDiscovery, cancel)
        .await
        .map_err(|e| format!("{}: {}", url, e))?;

    if !response.status.is_success() {
        let status = response.status.as_u16();
        let body = response
            .collect()
            .await
            .map_err(|e| format!("{}: failed to read error body: {}", url, e))?;
        return Err(format!(
            "{}: status {}: {}",
            url,
            status,
            String::from_utf8_lossy(&body)
        ));
    }

    let bytes = response
        .collect()
        .await
        .map_err(|e| format!("{}: {}", url, e))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("{}: parse: {}", url, e))
}

/// Map a header name to its typed `http::header::HeaderName` constant
/// when one exists; return `None` for non-standard names. This keeps
/// the common cases (`Authorization`, `Content-Type`, `User-Agent`)
/// case-insensitive without paying the cost of `HeaderName::from_bytes`
/// for every call.
pub(crate) fn header_name(name: &str) -> Option<http::header::HeaderName> {
    use http::header;
    match name.to_ascii_lowercase().as_str() {
        "authorization" => Some(header::AUTHORIZATION),
        "content-type" => Some(header::CONTENT_TYPE),
        "user-agent" => Some(header::USER_AGENT),
        "x-api-key" => Some(http::HeaderName::from_static("x-api-key")),
        "x-goog-api-key" => Some(http::HeaderName::from_static("x-goog-api-key")),
        _ => None,
    }
}




// Extracted OpenAI models structs
#[derive(Debug, Deserialize)]
pub(crate) struct OpenAIModelsResponse {
    #[serde(default)]
    data: Vec<OpenAIModelEntry>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OpenAIModelEntry {
    id: String,
}

// =====================================================================
// Shared OpenAI model-list fetcher-list fetcher
// =====================================================================

/// Fetch and parse an OpenAI-shaped `GET /models` response.
///
/// All three new OpenAI-compatible providers (Nous Research, NVIDIA NIM,
/// Kilocode) return `{"data": [{"id": "...", ...}]}`. This helper
/// avoids duplicating the HTTP + deserialization boilerplate.
pub(crate) async fn fetch_openai_models(
    url: &str,
    upstream_client: &Arc<UpstreamClient>,
    api_key: &str,
    provider_name: &str,
) -> Result<Vec<DiscoveredModel>> {
    let body = upstream_get_json(
        upstream_client,
        url,
        &[("Authorization", format!("Bearer {api_key}"))],
    )
    .await
    .map_err(|e| CoreError::UpstreamConnection(format!("{provider_name} /models: {e}")))?;

    let payload: OpenAIModelsResponse = serde_json::from_value(body)
        .map_err(|e| CoreError::Parse(format!("{provider_name} /models parse: {e}")))?;

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
        Arc::new(CloudflareWorkersAIAdapter::new()),
        Arc::new(GeminiAdapter::new()),
        Arc::new(AntigravityAdapter::new()),
        Arc::new(CodexAdapter::new()),
        Arc::new(KiroAdapter::new()),
    ]
}


// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use crate::adapters::opencode_zen::classify_zen_target_format;
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
        assert_eq!(
            first_header(&headers, "Content-Type"),
            Some("application/json")
        );
        assert_eq!(
            first_header(&headers, "HTTP-Referer"),
            Some("https://openproxy.local")
        );
        assert_eq!(first_header(&headers, "X-Title"), Some("openproxy"));
    }

    // ---- MiniMax -----------------------------------------------------

    #[test]
    fn minimax_builds_messages_url_with_beta() {
        let a = MiniMaxAdapter::new();
        let url = a.build_chat_url(TargetFormat::Anthropic, &ModelId::new("m"));
        assert_eq!(
            url,
            "https://api.minimax.io/anthropic/v1/messages?beta=true"
        );
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
        assert_eq!(
            first_header(&headers, "Content-Type"),
            Some("application/json")
        );
        assert_eq!(
            first_header(&headers, "Anthropic-Version"),
            Some("2023-06-01")
        );
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
        assert_eq!(
            first_header(&headers, "Anthropic-Version"),
            Some("2023-06-01")
        );
    }

    #[test]
    fn opencode_zen_uses_bearer_for_openai() {
        let a = OpenCodeZenAdapter::new();
        let headers = a.build_headers("k-openai", TargetFormat::Openai, &ModelId::new("m"));
        assert_eq!(
            first_header(&headers, "Authorization"),
            Some("Bearer k-openai")
        );
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
        assert_eq!(
            first_header(&headers, "Content-Type"),
            Some("application/json")
        );
        assert_eq!(first_header(&headers, "User-Agent"), Some("openproxy/0.1"));
    }

    #[test]
    fn opencode_zen_headers_have_user_agent_and_content_type() {
        let a = OpenCodeZenAdapter::new();
        for fmt in [TargetFormat::Openai, TargetFormat::Anthropic] {
            let headers = a.build_headers("k", fmt, &ModelId::new("m"));
            assert_eq!(first_header(&headers, "User-Agent"), Some("openproxy/0.1"));
            assert_eq!(
                first_header(&headers, "Content-Type"),
                Some("application/json")
            );
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
        assert_eq!(classify_zen_target_format("gpt-4o"), TargetFormat::Openai);
        assert_eq!(
            classify_zen_target_format("llama-3.1-70b"),
            TargetFormat::Openai
        );
    }

    // ---- Factory -----------------------------------------------------

    #[test]
    fn builtin_adapters_returns_twelve() {
        let v = builtin_adapters();
        assert_eq!(v.len(), 12);
        let ids: Vec<&str> = v.iter().map(|a| a.id().as_str()).collect();
        assert!(ids.contains(&"openrouter"));
        assert!(ids.contains(&"minimax"));
        assert!(ids.contains(&"opencode-zen"));
        assert!(ids.contains(&"ollama-cloud"));
        assert!(ids.contains(&"nous-research"));
        assert!(ids.contains(&"nvidia-nim"));
        assert!(ids.contains(&"kilocode"));
        assert!(ids.contains(&"cloudflare-workers-ai"));
        assert!(ids.contains(&"gemini"));
        assert!(ids.contains(&"antigravity"));
        assert!(ids.contains(&"codex"));
        assert!(ids.contains(&"kiro"));
    }

    // ---- Cloudflare Workers AI ---------------------------------------

    // B1 (Bug 2): the discovery scheduler used to pass an empty
    // `account_label` to `fetch_models_for_account`, which produced
    // URLs like `accounts//ai/models/search` and 404'd every tick.
    // The adapter now validates the label up-front and returns a
    // `Validation` error instead of building a broken URL — the
    // error is logged at WARN by the discovery scheduler and surfaces
    // in the dashboard's Debug Logs view with a clear root-cause
    // message ("account label is empty — set the account's `label`
    // field to the Cloudflare account ID").
    #[tokio::test]
    async fn cloudflare_fetch_models_for_account_rejects_empty_label() {
        let a = CloudflareWorkersAIAdapter::new();
        let upstream = Arc::new(UpstreamClient::new());
        let res = a
            .fetch_models_for_account(&upstream, "cf-test-key", "")
            .await;
        assert!(res.is_err(), "expected Validation error for empty label");
        let msg = match res.unwrap_err() {
            CoreError::Validation(s) => s,
            other => panic!("expected CoreError::Validation, got {other:?}"),
        };
        assert!(
            msg.contains("account label is empty"),
            "error message should explain the empty-label root cause, got: {msg}",
        );
    }

    // The non-empty path builds a URL of the form
    // `https://api.cloudflare.com/client/v4/accounts/{label}/ai/models/search`.
    // We don't hit the network here — we just exercise the
    // `models_url_for_account` helper, which is the URL the
    // `fetch_models_for_account` body would also build (kept in
    // sync by the adapter).
    #[test]
    fn cloudflare_models_url_for_account_builds_expected_path() {
        let a = CloudflareWorkersAIAdapter::new();
        assert_eq!(
            a.models_url_for_account("abc123").as_deref(),
            Some("https://api.cloudflare.com/client/v4/accounts/abc123/ai/models/search"),
        );
    }

    #[test]
    fn cloudflare_models_url_for_account_rejects_empty_label() {
        // The trait default for `models_url_for_account` would just
        // delegate to `models_url`, which returns `None` for the
        // Cloudflare adapter. We override it to mirror the
        // `fetch_models_for_account` validation: an empty label is a
        // configuration error, not a "no models URL" case.
        let a = CloudflareWorkersAIAdapter::new();
        assert_eq!(a.models_url_for_account("").as_deref(), None);
    }

    #[test]
    fn cloudflare_build_chat_url_for_account_substitutes_label() {
        let a = CloudflareWorkersAIAdapter::new();
        let url = a.build_chat_url_for_account(
            TargetFormat::Openai,
            &ModelId::new("@cf/meta/llama-3.1-8b-instruct"),
            "abc123",
        );
        assert_eq!(
            url,
            "https://api.cloudflare.com/client/v4/accounts/abc123/ai/v1/chat/completions",
        );
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
        assert_eq!(
            first_header(&headers, "Content-Type"),
            Some("application/json")
        );
    }

    // ---- Nous Research ------------------------------------------------

    #[test]
    fn nous_research_builds_correct_url() {
        let a = NousResearchAdapter::new();
        let url = a.build_chat_url(TargetFormat::Openai, &ModelId::new("Hermes-4-405B"));
        assert_eq!(
            url,
            "https://inference-api.nousresearch.com/v1/chat/completions"
        );
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
        assert_eq!(
            first_header(&headers, "Content-Type"),
            Some("application/json")
        );
    }

    // ---- NVIDIA NIM ---------------------------------------------------

    #[test]
    fn nvidia_nim_builds_correct_url() {
        let a = NvidiaNimAdapter::new();
        let url = a.build_chat_url(
            TargetFormat::Openai,
            &ModelId::new("nvidia/nemotron-3-super-120b-a12b"),
        );
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
        assert_eq!(
            first_header(&headers, "Content-Type"),
            Some("application/json")
        );
    }

    // ---- Kilocode -----------------------------------------------------

    #[test]
    fn kilocode_builds_correct_url() {
        let a = KilocodeAdapter::new();
        let url = a.build_chat_url(TargetFormat::Openai, &ModelId::new("openai/gpt-5.5"));
        assert_eq!(
            url,
            "https://api.kilo.ai/api/openrouter/v1/chat/completions"
        );
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
        assert_eq!(
            first_header(&headers, "Content-Type"),
            Some("application/json")
        );
    }

    // ---- Gemini -------------------------------------------------------

    #[test]
    fn gemini_builds_correct_url() {
        let a = GeminiAdapter::new();
        let url = a.build_chat_url(TargetFormat::Gemini, &ModelId::new("gemini-2.5-flash"));
        assert_eq!(
            url,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
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
        assert_eq!(
            first_header(&headers, "Content-Type"),
            Some("application/json")
        );
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


    // ---- CustomAdapter ---------------------------------------------------

    fn make_custom_provider(
        id: &str,
        base_url: &str,
        auth_type: crate::providers::AuthType,
        format: crate::providers::ProviderFormat,
    ) -> crate::providers::Provider {
        crate::providers::Provider {
            id: ProviderId::new(id),
            name: format!("Test {}", id),
            base_url: base_url.into(),
            auth_type,
            format,
            extra_headers_json: None,
            auto_activate_keyword: None,
            active: true,
            created_at: "2026-01-01T00:00:00Z".into(),
            use_proxies: false,
            current_proxy_id: None,
            proxy_rotation_errors: "429,connect_error,timeout".to_string(),
        }
    }

    #[test]
    fn custom_openai_adapter_builds_correct_url() {
        let p = make_custom_provider(
            "zenmux",
            "https://zenmux.example.com/v1",
            crate::providers::AuthType::Bearer,
            crate::providers::ProviderFormat::Openai,
        );
        let a = CustomAdapter::from_provider_row(&p);
        let url = a.build_chat_url(TargetFormat::Openai, &ModelId::new("gpt-4o"));
        assert_eq!(url, "https://zenmux.example.com/v1/chat/completions");
    }

    #[test]
    fn custom_anthropic_adapter_builds_correct_url() {
        let p = make_custom_provider(
            "my-anthropic",
            "https://api.example.com",
            crate::providers::AuthType::XApiKey,
            crate::providers::ProviderFormat::Anthropic,
        );
        let a = CustomAdapter::from_provider_row(&p);
        let url = a.build_chat_url(TargetFormat::Anthropic, &ModelId::new("claude-4"));
        assert_eq!(url, "https://api.example.com/messages");
    }

    #[test]
    fn custom_gemini_adapter_builds_correct_url() {
        let p = make_custom_provider(
            "my-gemini",
            "https://gemini.example.com/v1beta",
            crate::providers::AuthType::GoogApiKey,
            crate::providers::ProviderFormat::Gemini,
        );
        let a = CustomAdapter::from_provider_row(&p);
        let url = a.build_chat_url(TargetFormat::Gemini, &ModelId::new("gemini-2.5-pro"));
        assert_eq!(
            url,
            "https://gemini.example.com/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse"
        );
    }

    #[test]
    fn custom_mixed_adapter_routes_by_target_format() {
        let p = make_custom_provider(
            "my-aggregator",
            "https://agg.example.com/v1",
            crate::providers::AuthType::Bearer,
            crate::providers::ProviderFormat::Mixed,
        );
        let a = CustomAdapter::from_provider_row(&p);
        let openai_url = a.build_chat_url(TargetFormat::Openai, &ModelId::new("gpt-4o"));
        assert_eq!(openai_url, "https://agg.example.com/v1/chat/completions");
        let anthropic_url = a.build_chat_url(TargetFormat::Anthropic, &ModelId::new("claude-4"));
        assert_eq!(anthropic_url, "https://agg.example.com/v1/messages");
    }

    #[test]
    fn custom_bearer_auth_header() {
        let p = make_custom_provider(
            "zenmux",
            "https://zenmux.example.com/v1",
            crate::providers::AuthType::Bearer,
            crate::providers::ProviderFormat::Openai,
        );
        let a = CustomAdapter::from_provider_row(&p);
        let (name, value) = a.build_auth_header("sk-test-123");
        assert_eq!(name, "Authorization");
        assert_eq!(value, "Bearer sk-test-123");
    }

    #[test]
    fn custom_x_api_key_auth_header() {
        let p = make_custom_provider(
            "my-anthropic",
            "https://api.example.com",
            crate::providers::AuthType::XApiKey,
            crate::providers::ProviderFormat::Anthropic,
        );
        let a = CustomAdapter::from_provider_row(&p);
        let (name, value) = a.build_auth_header("sk-ant-test");
        assert_eq!(name, "x-api-key");
        assert_eq!(value, "sk-ant-test");
    }

    #[test]
    fn custom_no_auth_header() {
        let p = make_custom_provider(
            "local-ollama",
            "http://localhost:11434/v1",
            crate::providers::AuthType::None,
            crate::providers::ProviderFormat::Openai,
        );
        let a = CustomAdapter::from_provider_row(&p);
        let (name, value) = a.build_auth_header("");
        assert!(name.is_empty());
        assert!(value.is_empty());
    }

    #[test]
    fn custom_models_url() {
        let p = make_custom_provider(
            "zenmux",
            "https://zenmux.example.com/v1",
            crate::providers::AuthType::Bearer,
            crate::providers::ProviderFormat::Openai,
        );
        let a = CustomAdapter::from_provider_row(&p);
        assert_eq!(
            a.models_url(),
            Some("https://zenmux.example.com/v1/models".into())
        );
    }

    #[test]
    fn custom_adapter_id_matches_provider() {
        let p = make_custom_provider(
            "zenmux",
            "https://zenmux.example.com/v1",
            crate::providers::AuthType::Bearer,
            crate::providers::ProviderFormat::Openai,
        );
        let a = CustomAdapter::from_provider_row(&p);
        assert_eq!(a.id().as_str(), "zenmux");
    }

    #[test]
    fn custom_adapter_includes_extra_headers() {
        let mut p = make_custom_provider(
            "zenmux",
            "https://zenmux.example.com/v1",
            crate::providers::AuthType::Bearer,
            crate::providers::ProviderFormat::Openai,
        );
        p.extra_headers_json = Some(r#"{"X-Custom":"value1"}"#.into());
        let a = CustomAdapter::from_provider_row(&p);
        let headers = a.build_headers("sk-test", TargetFormat::Openai, &ModelId::new("gpt-4o"));
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "X-Custom" && v == "value1")
        );
    }
}
