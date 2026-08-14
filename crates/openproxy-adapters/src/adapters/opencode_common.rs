//! Shared OpenCode logic (Zen & Go variants).
//!
//! OpenCode Zen and OpenCode Go share identical model classification heuristics,
//! request headers (including client spoofing and per-format auth branching),
//! and model list discovery format.

use super::*;
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
            TargetFormat::Openai | TargetFormat::Gemini => {
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
            let id = m.id;
            let target_format = classify_opencode_target_format(&id);
            DiscoveredModel {
                display_name: Some(id.clone()),
                model_id: ModelId::new(id),
                target_format,
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
