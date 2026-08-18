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

/// Build headers for OpenCode requests (Anthropic vs OpenAI/Gemini branching).
pub fn build_opencode_headers(
    adapter: &impl ProviderAdapter,
    api_key: &str,
    target_format: TargetFormat,
) -> Vec<(String, String)> {
    let mut headers = vec![("Content-Type".into(), "application/json".into())];

    // Only add auth headers if we have an API key.
    if !api_key.is_empty() {
        match target_format {
            TargetFormat::Anthropic => {
                headers.push(("x-api-key".into(), api_key.to_string()));
                headers.push(("Anthropic-Version".into(), "2023-06-01".into()));
            }
            TargetFormat::Openai | TargetFormat::Gemini | TargetFormat::Atomesus => {
                if let Some(auth) = adapter.build_auth_header(api_key) {
                    headers.push(auth);
                }
            }
            TargetFormat::Responses => unreachable!("Responses format handled natively"),
        }
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

#[macro_export]
macro_rules! define_opencode_adapter {
    (
        $(#[$meta:meta])*
        $struct_name:ident,
        id: $id:literal,
        name: $name:literal,
        base_url: $base_url:literal
        $(, models_dev_canonical_ids: $canon_ids:expr)? $(,)?
    ) => {
        $(#[$meta])*
        #[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
        pub struct $struct_name {
            config: $crate::adapters::ProviderAdapterConfig,
        }

        impl $struct_name {
            pub fn new() -> Self {
                Self {
                    config: $crate::adapters::ProviderAdapterConfig {
                        id: openproxy_types::ProviderId::new($id),
                        name: $name.into(),
                        anonymous_fallback: true,
                        rate_limit_scope: "account".into(),
                        base_url: $base_url.into(),
                        auth_type: $crate::adapters::AdapterAuthType::Bearer,
                        format: $crate::adapters::AdapterFormat::Mixed,
                        extra_headers: vec![],
                    },
                }
            }
        }

        $crate::adapters::derive_default_from_new!($struct_name);

        impl $crate::adapters::ProviderAdapter for $struct_name {
            fn config(&self) -> &$crate::adapters::ProviderAdapterConfig {
                &self.config
            }

            fn is_anonymous_fallback(&self) -> bool {
                true
            }

            $(
                fn models_dev_canonical_ids(&self) -> &'static [&'static str] {
                    $canon_ids
                }
            )?

            fn build_headers(
                &self,
                api_key: &str,
                target_format: openproxy_types::TargetFormat,
                _model: &openproxy_types::ModelId,
            ) -> Vec<(String, String)> {
                $crate::adapters::opencode_common::build_opencode_headers(self, api_key, target_format)
            }

            async fn fetch_models(
                &self,
                upstream_client: &std::sync::Arc<$crate::upstream::UpstreamClient>,
                api_key: &str,
            ) -> openproxy_types::Result<Vec<openproxy_types::DiscoveredModel>> {
                $crate::adapters::opencode_common::fetch_opencode_models(self, upstream_client, api_key).await
            }
        }
    };
}
