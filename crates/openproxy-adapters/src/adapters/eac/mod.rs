//! EAC Gateway provider adapter (<https://ourfreemodel.beichenapi.com/eac/v1>).
//!
//! Features:
//! - OpenAI-compatible `/chat/completions` protocol with Bearer authentication.
//! - HMAC-SHA256 request signing via `x-ofm-timestamp` and `x-ofm-signature`.
//! - Support for deepseek, kimi, and glm frontier models.

pub mod models;
pub mod signing;

#[cfg(test)]
mod tests;

pub use models::eac_builtin_models;
pub use signing::{
    DEFAULT_EAC_SECRET, DEFAULT_EAC_USER_AGENT, HEADER_OFM_SIGNATURE, HEADER_OFM_TIMESTAMP,
    HEADER_OFM_USER, compute_eac_signature, extract_pathname, hmac_sha256, resolve_eac_secret,
    sign_request_headers, to_hex_lower,
};

use super::{
    AdapterAuthType, AdapterFormat, Arc, DiscoveredModel, ProviderAdapterConfig, ProviderId,
    Result, TargetFormat, UpstreamClient,
};
use crate::adapters::ProviderAdapter;
use serde::{Deserialize, Serialize};

pub const EAC_DEFAULT_BASE_URL: &str = "https://ourfreemodel.beichenapi.com/eac/v1";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EacAdapter {
    config: ProviderAdapterConfig,
    #[serde(default)]
    secret: Option<String>,
}

impl EacAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("eac"),
                name: "EAC Gateway".into(),
                anonymous_fallback: false,
                rate_limit_scope: "account".into(),
                base_url: EAC_DEFAULT_BASE_URL.into(),
                auth_type: AdapterAuthType::OAuth,
                format: AdapterFormat::Openai,
                extra_headers: Vec::new(),
            },
            secret: None,
        }
    }

    pub fn with_secret(mut self, secret: impl Into<String>) -> Self {
        self.secret = Some(secret.into());
        self
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.config.base_url = base_url.into();
        self
    }

    pub fn secret(&self) -> &str {
        self.secret.as_deref().unwrap_or(DEFAULT_EAC_SECRET)
    }

    pub fn set_secret(&mut self, secret: Option<String>) {
        self.secret = secret;
    }

    pub fn sign_request_headers(
        &self,
        method: &str,
        url: &str,
        headers: &mut Vec<(String, String)>,
        body: &[u8],
    ) {
        sign_request_headers(self.secret(), method, url, headers, body);
    }
}

crate::adapters::derive_default_from_new!(EacAdapter);

impl ProviderAdapter for EacAdapter {
    crate::adapters::impl_adapter_config!();

    fn models_dev_canonical_ids(&self) -> &'static [&'static str] {
        &["eac", "eac-gateway", "ofm"]
    }

    fn finalize_request(
        &self,
        body: bytes::Bytes,
        headers: &mut Vec<(String, String)>,
        target_format: TargetFormat,
        model: &openproxy_types::ModelId,
        _resolved_target: &openproxy_types::context::ResolvedTarget,
    ) -> Result<bytes::Bytes> {
        let url = self.build_chat_url(target_format, model);
        self.sign_request_headers("POST", &url, headers, &body);
        Ok(body)
    }

    async fn fetch_models(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        let base = self.config.base_url.trim_end_matches('/');
        let url = format!("{base}/models");
        let mut headers = vec![("Accept".to_string(), "application/json".to_string())];
        if !api_key.trim().is_empty() {
            headers.push((
                "Authorization".to_string(),
                format!("Bearer {}", api_key.trim()),
            ));
        }
        self.sign_request_headers("GET", &url, &mut headers, b"");

        let header_refs: Vec<(&str, &str)> = headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        match crate::adapters::discovery::upstream_get_json(upstream_client, &url, &header_refs)
            .await
        {
            Ok(json) => {
                if let Some(arr) = json.get("data").and_then(|v| v.as_array()) {
                    let models: Vec<DiscoveredModel> = arr
                        .iter()
                        .filter_map(|m| {
                            m.get("id").and_then(|id| id.as_str()).map(|id| {
                                crate::adapters::discovery::build_discovered_model_with(
                                    id.to_string(),
                                    TargetFormat::Openai,
                                )
                            })
                        })
                        .collect();
                    if !models.is_empty() {
                        return Ok(models);
                    }
                }
                Ok(eac_builtin_models())
            }
            Err(e) => {
                tracing::debug!(error = %e, "eac: upstream models fetch failed, using built-in catalog");
                Ok(eac_builtin_models())
            }
        }
    }
}
