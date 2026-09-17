//! Shared OpenCode logic (Zen & Go variants).
//!
//! OpenCode Zen and OpenCode Go share identical model classification heuristics,
//! request headers (including client spoofing and per-format auth branching),
//! and model list discovery format.

use super::{
    Arc, CoreError, DiscoveredModel, OpenAIModelsResponse, ProviderAdapter, Result, TargetFormat,
    UpstreamClient, upstream_get_json,
};
use crate::spoofer::{ClientSpoofer, OpenCodeSpoofer};
use openproxy_types::ResultExt;

/// Heuristic for picking the wire format of a model in OpenCode's catalogue.
///
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum OpenCodeFlavor {
    Zen,
    Go,
}

/// Classify wire target format for OpenCode Zen (`https://opencode.ai/zen/v1`).
pub fn classify_zen_target_format(id: &str) -> TargetFormat {
    classify_opencode_target_format(OpenCodeFlavor::Zen, id)
}

/// Classify wire target format for OpenCode Go (`https://opencode.ai/zen/go/v1`).
pub fn classify_go_target_format(id: &str) -> TargetFormat {
    classify_opencode_target_format(OpenCodeFlavor::Go, id)
}

/// Classify wire target format for OpenCode models (Zen & Go).
///
/// Per OpenCode Console routing:
/// - Anthropic (/messages): claude, minimax, qwen, union-alpha
/// - Gemini (/models/{model}:streamGenerateContent?alt=sse): gemini
/// - Responses (/responses): gpt-5, gpt-6, grok
/// - OpenAI (/chat/completions): deepseek, glm, kimi, mimo, ling, nemotron, big-pickle, muse-spark, etc.
///
/// Zen and Go expose different backends under the same alias, and the catalogue
/// mixes wire formats *inside* a family (Zen answers `minimax-m3` on
/// `/chat/completions` but `minimax-m3-free` only on `/messages`), so exact-ID
/// overrides are checked before the family heuristic.
///
/// Sources: the endpoints tables at <https://opencode.ai/docs/zen> and
/// <https://opencode.ai/docs/go>, plus the per-model `provider.npm` metadata of
/// the `opencode` / `opencode-go` entries in models.dev (the routing metadata
/// OpenCode itself selects the endpoint from).
pub fn classify_opencode_target_format(flavor: OpenCodeFlavor, id: &str) -> TargetFormat {
    let lower = id.to_ascii_lowercase();
    // Compile-time jump table over the aliases the family heuristic misroutes.
    match (flavor, lower.as_str()) {
        // `messages`-only stealth model; no Anthropic substring in the ID.
        (_, "union-alpha") => TargetFormat::Anthropic,
        (
            OpenCodeFlavor::Zen,
            "minimax-m2.1" | "minimax-m2.5" | "minimax-m2.7" | "minimax-m3" | "qwen3-coder"
            | "grok-code",
        ) => TargetFormat::Openai,
        _ => family_target_format(&lower),
    }
}

/// Substring heuristic used when no exact-ID override matches.
fn family_target_format(lower: &str) -> TargetFormat {
    if lower.contains("claude") || lower.contains("minimax") || lower.contains("qwen") {
        TargetFormat::Anthropic
    } else if lower.contains("gemini") {
        TargetFormat::Gemini
    } else if lower.contains("gpt-5") || lower.contains("gpt-6") || lower.contains("grok") {
        TargetFormat::Responses
    } else {
        TargetFormat::Openai
    }
}

fn append_format_auth_headers(
    headers: &mut Vec<(String, String)>,
    adapter: &impl ProviderAdapter,
    api_key: &str,
    target_format: TargetFormat,
) {
    if target_format == TargetFormat::Anthropic {
        headers.push(("x-api-key".into(), api_key.to_string()));
        headers.push(("Anthropic-Version".into(), "2023-06-01".into()));
    } else if target_format == TargetFormat::Gemini {
        headers.push(("x-goog-api-key".into(), api_key.to_string()));
    } else if let Some(auth) = adapter.build_auth_header(api_key) {
        headers.push(auth);
    }
}

/// Build headers for OpenCode requests (Anthropic vs OpenAI/Gemini branching).
pub fn build_opencode_headers(
    adapter: &impl ProviderAdapter,
    api_key: &str,
    target_format: TargetFormat,
) -> Vec<(String, String)> {
    let mut headers = vec![("Content-Type".into(), "application/json".into())];

    // Only add auth headers if we have an API key.
    if !api_key.is_empty() {
        append_format_auth_headers(&mut headers, adapter, api_key, target_format);
    }

    headers.extend(OpenCodeSpoofer.headers());
    headers
}

/// Fetch and parse models from an OpenCode endpoint.
pub async fn fetch_opencode_models(
    adapter: &impl ProviderAdapter,
    flavor: OpenCodeFlavor,
    upstream_client: &Arc<UpstreamClient>,
    api_key: &str,
) -> Result<Vec<DiscoveredModel>> {
    let url = adapter
        .models_url()
        .ok_or_else(|| CoreError::Validation(format!("{}: models_url is None", adapter.id())))?;

    let auth = format!("Bearer {api_key}");
    let body = upstream_get_json(upstream_client, &url, &[("Authorization", &auth)])
        .await
        .ctx_upstream(format!("{} /models", adapter.id()))?;

    let payload: OpenAIModelsResponse =
        <OpenAIModelsResponse as serde::Deserialize>::deserialize(&body)
            .ctx_validation(format!("{} /models parse", adapter.id()))?;

    let out = payload
        .data
        .into_iter()
        .map(|m| {
            let target_format = classify_opencode_target_format(flavor, &m.id);
            super::build_discovered_model_with(m.id, target_format)
        })
        .collect();
    Ok(out)
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct OpenCodeAdapter {
    flavor: OpenCodeFlavor,
    config: crate::adapters::ProviderAdapterConfig,
}

impl OpenCodeAdapter {
    pub fn new(flavor: OpenCodeFlavor) -> Self {
        let (id, name, base_url) = match flavor {
            OpenCodeFlavor::Zen => ("opencode-zen", "OpenCode Zen", "https://opencode.ai/zen/v1"),
            OpenCodeFlavor::Go => (
                "opencode-go",
                "OpenCode Go",
                "https://opencode.ai/zen/go/v1",
            ),
        };

        Self {
            flavor,
            config: crate::adapters::ProviderAdapterConfig {
                id: openproxy_types::ProviderId::new(id),
                name: name.into(),
                anonymous_fallback: true,
                rate_limit_scope: "account".into(),
                base_url: base_url.into(),
                auth_type: crate::adapters::AdapterAuthType::Bearer,
                format: crate::adapters::AdapterFormat::Mixed,
                extra_headers: vec![],
            },
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct OpenCodeGoAdapter(pub OpenCodeAdapter);

impl OpenCodeGoAdapter {
    pub fn new() -> Self {
        Self(OpenCodeAdapter::new(OpenCodeFlavor::Go))
    }
}
crate::adapters::derive_default_from_new!(OpenCodeGoAdapter);

impl crate::adapters::ProviderAdapter for OpenCodeGoAdapter {
    fn config(&self) -> &crate::adapters::ProviderAdapterConfig {
        self.0.config()
    }
    fn is_anonymous_fallback(&self) -> bool {
        self.0.is_anonymous_fallback()
    }
    fn models_dev_canonical_ids(&self) -> &'static [&'static str] {
        self.0.models_dev_canonical_ids()
    }
    fn build_headers(
        &self,
        api_key: &str,
        target_format: openproxy_types::TargetFormat,
        model: &openproxy_types::ModelId,
    ) -> Vec<(String, String)> {
        self.0.build_headers(api_key, target_format, model)
    }
    async fn fetch_models(
        &self,
        upstream_client: &std::sync::Arc<crate::upstream::UpstreamClient>,
        api_key: &str,
    ) -> openproxy_types::Result<Vec<openproxy_types::DiscoveredModel>> {
        self.0.fetch_models(upstream_client, api_key).await
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct OpenCodeZenAdapter(pub OpenCodeAdapter);

impl OpenCodeZenAdapter {
    pub fn new() -> Self {
        Self(OpenCodeAdapter::new(OpenCodeFlavor::Zen))
    }
}
crate::adapters::derive_default_from_new!(OpenCodeZenAdapter);

impl crate::adapters::ProviderAdapter for OpenCodeZenAdapter {
    fn config(&self) -> &crate::adapters::ProviderAdapterConfig {
        self.0.config()
    }
    fn is_anonymous_fallback(&self) -> bool {
        self.0.is_anonymous_fallback()
    }
    fn models_dev_canonical_ids(&self) -> &'static [&'static str] {
        self.0.models_dev_canonical_ids()
    }
    fn build_headers(
        &self,
        api_key: &str,
        target_format: openproxy_types::TargetFormat,
        model: &openproxy_types::ModelId,
    ) -> Vec<(String, String)> {
        self.0.build_headers(api_key, target_format, model)
    }
    async fn fetch_models(
        &self,
        upstream_client: &std::sync::Arc<crate::upstream::UpstreamClient>,
        api_key: &str,
    ) -> openproxy_types::Result<Vec<openproxy_types::DiscoveredModel>> {
        self.0.fetch_models(upstream_client, api_key).await
    }
}

impl crate::adapters::ProviderAdapter for OpenCodeAdapter {
    fn config(&self) -> &crate::adapters::ProviderAdapterConfig {
        &self.config
    }

    fn is_anonymous_fallback(&self) -> bool {
        true
    }

    fn models_dev_canonical_ids(&self) -> &'static [&'static str] {
        match self.flavor {
            OpenCodeFlavor::Zen => &["opencode"],
            OpenCodeFlavor::Go => &["opencode-go"],
        }
    }

    fn build_headers(
        &self,
        api_key: &str,
        target_format: openproxy_types::TargetFormat,
        _model: &openproxy_types::ModelId,
    ) -> Vec<(String, String)> {
        build_opencode_headers(self, api_key, target_format)
    }

    async fn fetch_models(
        &self,
        upstream_client: &std::sync::Arc<crate::upstream::UpstreamClient>,
        api_key: &str,
    ) -> openproxy_types::Result<Vec<openproxy_types::DiscoveredModel>> {
        fetch_opencode_models(self, self.flavor, upstream_client, api_key).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ZEN: OpenCodeFlavor = OpenCodeFlavor::Zen;
    const GO: OpenCodeFlavor = OpenCodeFlavor::Go;

    #[test]
    fn test_classify_opencode_target_format() {
        assert_eq!(
            classify_opencode_target_format(ZEN, "claude-3-opus"),
            TargetFormat::Anthropic
        );
        assert_eq!(
            classify_opencode_target_format(ZEN, "minimax-abab6"),
            TargetFormat::Anthropic
        );
        assert_eq!(
            classify_opencode_target_format(ZEN, "CLAUDE-3-SONNET"),
            TargetFormat::Anthropic
        );
        assert_eq!(
            classify_opencode_target_format(ZEN, "gpt-4-turbo"),
            TargetFormat::Openai
        );
        assert_eq!(
            classify_opencode_target_format(ZEN, "gemini-3-flash"),
            TargetFormat::Gemini
        );
        assert_eq!(
            classify_opencode_target_format(ZEN, "qwen3.6-plus"),
            TargetFormat::Anthropic
        );
        assert_eq!(
            classify_opencode_target_format(ZEN, "muse-spark-1.3-contributor-free"),
            TargetFormat::Openai
        );
        assert_eq!(
            classify_opencode_target_format(ZEN, "gpt-5.6-terra"),
            TargetFormat::Responses
        );
        assert_eq!(
            classify_opencode_target_format(ZEN, "grok-4.6"),
            TargetFormat::Responses
        );
        assert_eq!(
            classify_opencode_target_format(ZEN, "mimo-v2.5-free"),
            TargetFormat::Openai
        );
        assert_eq!(
            classify_opencode_target_format(ZEN, "unknown-model"),
            TargetFormat::Openai
        );
    }

    /// `union-alpha` is only served by `/zen/v1/messages` on both flavors; the
    /// family heuristic has no substring to key on and used to route it to
    /// `/chat/completions`, which answers 500 from upstream.
    #[test]
    fn union_alpha_uses_anthropic_messages_on_both_flavors() {
        assert_eq!(
            classify_opencode_target_format(ZEN, "union-alpha"),
            TargetFormat::Anthropic
        );
        assert_eq!(
            classify_opencode_target_format(GO, "union-alpha"),
            TargetFormat::Anthropic
        );
        assert_eq!(
            classify_opencode_target_format(ZEN, "UNION-ALPHA"),
            TargetFormat::Anthropic
        );
    }

    /// Zen answers the paid MiniMax variants and the `-coder`/`-code` extras on
    /// OpenAI-compatible `/chat/completions`; Go keeps them on `/messages`.
    #[test]
    fn zen_openai_compatible_exceptions() {
        for id in [
            "minimax-m3",
            "minimax-m2.7",
            "minimax-m2.5",
            "minimax-m2.1",
            "qwen3-coder",
            "grok-code",
        ] {
            assert_eq!(
                classify_opencode_target_format(ZEN, id),
                TargetFormat::Openai,
                "{id}"
            );
        }

        // Free MiniMax tiers stay on the Anthropic Messages API.
        assert_eq!(
            classify_opencode_target_format(ZEN, "minimax-m3-free"),
            TargetFormat::Anthropic
        );
        // Go routes the same aliases through `/messages`.
        assert_eq!(
            classify_opencode_target_format(GO, "minimax-m3"),
            TargetFormat::Anthropic
        );
        assert_eq!(
            classify_opencode_target_format(GO, "qwen3.7-max"),
            TargetFormat::Anthropic
        );
    }

    #[test]
    fn flavor_aliases_match_core_classifier() {
        assert_eq!(
            classify_zen_target_format("minimax-m3"),
            TargetFormat::Openai
        );
        assert_eq!(
            classify_go_target_format("minimax-m3"),
            TargetFormat::Anthropic
        );
        assert_eq!(
            classify_zen_target_format("union-alpha"),
            TargetFormat::Anthropic
        );
    }
}
