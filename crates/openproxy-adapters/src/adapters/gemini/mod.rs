use super::{
    AdapterAuthType, AdapterFormat, Arc, CoreError, DiscoveredModel, ModelId, ProviderAdapter,
    ProviderAdapterConfig, ProviderId, Result, TargetFormat, UpstreamClient,
    build_discovered_model_full, fetch_models_with_auth,
};

pub mod media;
pub mod translate;
pub mod types;

#[cfg(test)]
mod tests;

pub use media::*;
pub use translate::*;
pub use types::*;

/// Adapter for Google's Gemini API (`generativelanguage.googleapis.com`).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct GeminiAdapter {
    config: ProviderAdapterConfig,
}

impl GeminiAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("gemini"),
                name: "Google Gemini".into(),
                anonymous_fallback: false,
                rate_limit_scope: "account".into(),
                base_url: "https://generativelanguage.googleapis.com/v1beta".into(),
                auth_type: AdapterAuthType::GoogApiKey,
                format: AdapterFormat::Gemini,
                extra_headers: vec![],
            },
        }
    }
}

crate::adapters::derive_default_from_new!(GeminiAdapter);

impl ProviderAdapter for GeminiAdapter {
    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn models_dev_canonical_ids(&self) -> &'static [&'static str] {
        &["google"]
    }

    fn build_chat_url(&self, _target_format: TargetFormat, model: &ModelId) -> String {
        let model_str = model.as_str();
        if model_str.contains('/') {
            let safe_model = model_str.replace('/', "");
            format!(
                "{}/models/{}:streamGenerateContent?alt=sse",
                self.config.base_url, safe_model
            )
        } else {
            format!(
                "{}/models/{}:streamGenerateContent?alt=sse",
                self.config.base_url, model_str
            )
        }
    }

    fn models_url(&self) -> Option<String> {
        Some(format!("{}/models", self.config.base_url))
    }

    async fn fetch_models(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        let url = self
            .models_url()
            .ok_or_else(|| CoreError::Internal("gemini: models_url is None (impossible)".into()))?;

        fetch_models_with_auth(
            &url,
            upstream_client,
            &[("x-goog-api-key", api_key)],
            "models",
            "gemini",
            |m| {
                let full_name = m.get("name").and_then(|v| v.as_str())?;
                let id = full_name.strip_prefix("models/").unwrap_or(full_name);
                let display_name = m
                    .get("displayName")
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string);
                let ctx = m.get("inputTokenLimit").and_then(serde_json::Value::as_i64);
                let out = m
                    .get("outputTokenLimit")
                    .and_then(serde_json::Value::as_i64);
                Some(build_discovered_model_full(
                    id.to_string(),
                    display_name,
                    TargetFormat::Gemini,
                    ctx,
                    out,
                ))
            },
        )
        .await
    }

    fn format_request(
        &self,
        _target_format: TargetFormat,
        req: &openproxy_types::OpenAIRequest,
        _model: &ModelId,
        messages: &[openproxy_types::OpenAIMessage],
        _stream: bool,
    ) -> std::result::Result<bytes::Bytes, CoreError> {
        serialize_gemini_request(req, messages)
    }

    fn translate_non_streaming_response(
        &self,
        _target_format: TargetFormat,
        response_body: serde_json::Value,
    ) -> std::result::Result<openproxy_types::OpenAIResponse, CoreError> {
        deserialize_gemini_response(&response_body)
    }
}
