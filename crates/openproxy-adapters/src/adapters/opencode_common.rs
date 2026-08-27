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

/// Heuristic for picking the wire format of a model in OpenCode's catalogue.
///
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum OpenCodeFlavor {
    Zen,
    Go,
}

pub use classify_opencode_target_format as classify_zen_target_format;
pub use classify_opencode_target_format as classify_go_target_format;

/// Anthropic-family identifiers (`claude`, `minimax`) go to `/messages`; the
/// rest are served as OpenAI on `/chat/completions`. The matching is
/// case-insensitive.
pub fn classify_opencode_target_format(id: &str) -> TargetFormat {
    let lower = id.to_ascii_lowercase();
    if lower.contains("claude") || lower.contains("minimax") {
        TargetFormat::Anthropic
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
    upstream_client: &Arc<UpstreamClient>,
    api_key: &str,
) -> Result<Vec<DiscoveredModel>> {
    let url = adapter
        .models_url()
        .ok_or_else(|| CoreError::Validation(format!("{}: models_url is None", adapter.id())))?;

    let auth = format!("Bearer {api_key}");
    let body = upstream_get_json(upstream_client, &url, &[("Authorization", &auth)])
        .await
        .map_err(|e| CoreError::UpstreamConnection(format!("{} /models: {e}", adapter.id())))?;

    let payload: OpenAIModelsResponse =
        <OpenAIModelsResponse as serde::Deserialize>::deserialize(&body)
            .map_err(|e| CoreError::Validation(format!("{} /models parse: {e}", adapter.id())))?;

    let out = payload
        .data
        .into_iter()
        .map(|m| {
            let target_format = classify_opencode_target_format(&m.id);
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
        fetch_opencode_models(self, upstream_client, api_key).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_opencode_target_format() {
        assert_eq!(
            classify_opencode_target_format("claude-3-opus"),
            TargetFormat::Anthropic
        );
        assert_eq!(
            classify_opencode_target_format("minimax-abab6"),
            TargetFormat::Anthropic
        );
        assert_eq!(
            classify_opencode_target_format("CLAUDE-3-SONNET"),
            TargetFormat::Anthropic
        );
        assert_eq!(
            classify_opencode_target_format("gpt-4-turbo"),
            TargetFormat::Openai
        );
        assert_eq!(
            classify_opencode_target_format("gemini-pro"),
            TargetFormat::Openai
        );
        assert_eq!(
            classify_opencode_target_format("unknown-model"),
            TargetFormat::Openai
        );
    }
}
