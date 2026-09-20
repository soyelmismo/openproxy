use super::{
    AdapterAuthType, AdapterFormat, Arc, CancellationToken, DiscoveredModel, ModelId,
    ProviderAdapter, ProviderAdapterConfig, ProviderId, Result, TargetFormat, UpstreamClient,
    UpstreamRequest,
};
use bytes::Bytes;
use openproxy_types::ImageGenerationRequest;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

pub mod generation;
pub mod interrogate;
pub mod models;
pub mod prompt;

#[cfg(test)]
mod tests;

pub use generation::*;
pub use interrogate::*;
pub use models::*;
pub use prompt::*;

pub use openproxy_types::images::{
    HordeAsyncSubmitResponse, HordeCheckResponse, HordeGenerationItem, HordeStatusResponse,
};

pub(crate) static HORDE_CLIENT_AGENT: LazyLock<http::HeaderValue> = LazyLock::new(|| {
    http::HeaderValue::from_static(concat!("openproxy:", env!("CARGO_PKG_VERSION")))
});

pub(crate) fn apply_horde_auth_headers(
    req: &mut UpstreamRequest,
    api_key: &str,
    include_bearer: bool,
) {
    let key = if api_key.trim().is_empty() {
        "0000000000"
    } else {
        api_key.trim()
    };
    if let Ok(val) = http::HeaderValue::from_str(key) {
        req.headers
            .insert(http::header::HeaderName::from_static("apikey"), val);
    }
    req.headers.insert(
        http::header::HeaderName::from_static("client-agent"),
        HORDE_CLIENT_AGENT.clone(),
    );
    if include_bearer {
        let mut bytes = bytes::BytesMut::with_capacity(7 + key.len());
        bytes.extend_from_slice(b"Bearer ");
        bytes.extend_from_slice(key.as_bytes());
        if let Ok(val) = http::HeaderValue::from_maybe_shared(bytes.freeze()) {
            req.headers.insert(http::header::AUTHORIZATION, val);
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HordeAdapter {
    config: ProviderAdapterConfig,
}

impl HordeAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("horde"),
                name: "AI Horde".into(),
                anonymous_fallback: true,
                rate_limit_scope: "account".into(),
                base_url: "https://aihorde.net/api/v2".into(),
                auth_type: AdapterAuthType::Bearer,
                format: AdapterFormat::Openai,
                extra_headers: vec![],
            },
        }
    }

    pub fn build_horde_payload(
        &self,
        req: &ImageGenerationRequest,
        upstream_model_id: &str,
        source_image_b64: Option<String>,
        source_mask_b64: Option<String>,
        source_processing: Option<&str>,
        denoising_strength: Option<f32>,
    ) -> Result<Bytes> {
        generation::build_horde_payload(
            req,
            upstream_model_id,
            source_image_b64,
            source_mask_b64,
            source_processing,
            denoising_strength,
        )
    }

    pub fn is_vision_model(model_name: &str) -> bool {
        interrogate::is_vision_model(model_name)
    }

    pub fn build_interrogate_payload(source_image: &str, forms: &[&str]) -> Result<Bytes> {
        interrogate::build_interrogate_payload(source_image, forms)
    }

    pub fn extract_image_from_messages(
        messages: &[openproxy_types::OpenAIMessage],
    ) -> Option<String> {
        interrogate::extract_image_from_messages(messages)
    }

    pub fn parse_interrogate_status_caption(status_json: &serde_json::Value) -> Option<String> {
        interrogate::parse_interrogate_status_caption(status_json)
    }

    pub fn is_interrogate_done(status_json: &serde_json::Value) -> (bool, bool) {
        interrogate::is_interrogate_done(status_json)
    }

    pub async fn submit_interrogate_job(
        upstream_client: &Arc<UpstreamClient>,
        base_url: &str,
        api_key: &str,
        source_image: &str,
        cancel_token: CancellationToken,
    ) -> Result<String> {
        interrogate::submit_interrogate_job(
            upstream_client,
            base_url,
            api_key,
            source_image,
            cancel_token,
        )
        .await
    }

    pub async fn poll_interrogate_job(
        upstream_client: &Arc<UpstreamClient>,
        base_url: &str,
        api_key: &str,
        job_id: &str,
        cancel_token: CancellationToken,
    ) -> Result<String> {
        interrogate::poll_interrogate_job(upstream_client, base_url, api_key, job_id, cancel_token)
            .await
    }

    pub async fn execute_interrogate(
        upstream_client: &Arc<UpstreamClient>,
        base_url: &str,
        api_key: &str,
        source_image: &str,
        cancel_token: CancellationToken,
    ) -> Result<String> {
        interrogate::execute_interrogate(
            upstream_client,
            base_url,
            api_key,
            source_image,
            cancel_token,
        )
        .await
    }

    async fn fetch_horde_quota_local(
        &self,
        upstream: &Arc<UpstreamClient>,
        api_key: &str,
    ) -> Result<openproxy_types::AccountQuota> {
        let key = if api_key.trim().is_empty() {
            "0000000000"
        } else {
            api_key.trim()
        };

        match query_horde_user(upstream, &self.config.base_url, key).await {
            Ok(json) => Ok(parse_horde_quota(
                &json,
                &openproxy_types::quota::now_unix_secs_str(),
            )),
            Err(err) => Ok(openproxy_types::AccountQuota::with_error(err)),
        }
    }
}

crate::adapters::derive_default_from_new!(HordeAdapter);

impl ProviderAdapter for HordeAdapter {
    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn metadata(&self) -> openproxy_types::ProviderMetadata {
        let mut meta = openproxy_types::ProviderMetadata::custom_default();
        meta.built_in = true;
        meta.deletable = false;
        meta.supports_quota = true;
        meta.quota_refresh_supported = true;
        meta
    }

    fn is_anonymous_fallback(&self) -> bool {
        true
    }

    fn build_auth_header(&self, api_key: &str) -> Option<(String, String)> {
        let key = if api_key.trim().is_empty() {
            "0000000000"
        } else {
            api_key.trim()
        };
        Some(("Authorization".into(), format!("Bearer {key}")))
    }

    fn build_headers(
        &self,
        api_key: &str,
        _target_format: TargetFormat,
        _model: &ModelId,
    ) -> Vec<(String, String)> {
        let key = if api_key.trim().is_empty() {
            "0000000000"
        } else {
            api_key.trim()
        };
        let mut headers = Vec::with_capacity(4);
        headers.push(("Authorization".into(), format!("Bearer {key}")));
        headers.push(("apikey".into(), key.to_string()));
        headers.push((
            "Client-Agent".into(),
            concat!("openproxy:", env!("CARGO_PKG_VERSION")).into(),
        ));
        headers.push(("Content-Type".into(), "application/json".into()));
        headers
    }

    fn build_chat_url(&self, _target_format: TargetFormat, _model: &ModelId) -> String {
        "https://oai.aihorde.net/v1/chat/completions".to_string()
    }

    fn models_url(&self) -> Option<String> {
        Some("https://oai.aihorde.net/v1/models".to_string())
    }

    async fn fetch_models(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        let headers = self.build_headers(api_key, TargetFormat::Openai, &ModelId::new(""));
        let header_refs: Vec<(&str, &str)> = headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        let mut discovered = fetch_horde_cluster_models(
            upstream_client,
            &self.config.base_url,
            &header_refs,
            "text",
        )
        .await;
        discovered.extend(
            fetch_horde_cluster_models(
                upstream_client,
                &self.config.base_url,
                &header_refs,
                "image",
            )
            .await,
        );

        // Synthetic Vision / Interrogation model
        discovered.push(DiscoveredModel {
            model_id: ModelId::new("horde/vision"),
            display_name: Some("Horde Vision (CLIP/BLIP Interrogator)".into()),
            target_format: TargetFormat::Openai,
            context_length: None,
            max_output_tokens: None,
            input_modalities: Some(vec!["text".into(), "image".into()].into()),
            output_modalities: Some(vec!["text".into()].into()),
            model_type: Some("chat".into()),
            family: Some("vision".into()),
            capabilities: None,
        });

        Ok(discovered)
    }

    fn build_image_url(&self) -> String {
        format!("{}/generate/async", self.config.base_url)
    }

    fn build_image_edits_url(&self) -> String {
        self.build_image_url()
    }

    fn build_image_variations_url(&self) -> String {
        self.build_image_url()
    }

    fn format_image_request(
        &self,
        req: &ImageGenerationRequest,
        upstream_model_id: &str,
    ) -> Result<Bytes> {
        self.build_horde_payload(req, upstream_model_id, None, None, None, None)
    }

    async fn fetch_quota(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Option<Result<openproxy_types::AccountQuota>> {
        Some(self.fetch_horde_quota_local(upstream_client, api_key).await)
    }
}
