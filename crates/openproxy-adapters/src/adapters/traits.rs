use crate::upstream::UpstreamClient;
use bytes::Bytes;
use openproxy_types::{
    CoreError, DiscoveredModel, ModelId, ProviderId, ProviderMetadata, Result, TargetFormat,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Static configuration for a single provider adapter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderAdapterConfig {
    pub id: ProviderId,
    pub name: String,
    pub base_url: String,
    pub auth_type: AdapterAuthType,
    pub format: AdapterFormat,
    pub extra_headers: Vec<(String, String)>,
    pub anonymous_fallback: bool,
    pub rate_limit_scope: String,
}

// Re-export / alias types from `openproxy_types` to eliminate duplicated enum definitions
pub type AdapterAuthType = openproxy_types::AuthType;
pub type AdapterFormat = openproxy_types::ProviderFormat;

thread_local! {
    static SERIALIZE_BUF: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[derive(Serialize)]
struct ModelInjector<'a, T> {
    #[serde(flatten)]
    req: &'a T,
    model: &'a str,
}

/// Helper to serialize a request object, overwrite its `model` field, and return `Bytes`.
pub fn inject_model_and_serialize<T: Serialize>(
    req: &T,
    upstream_model: &str,
) -> std::result::Result<Bytes, CoreError> {
    let injector = ModelInjector {
        req,
        model: upstream_model,
    };
    SERIALIZE_BUF.with_borrow_mut(|buf| {
        buf.clear();
        serde_json::to_writer(&mut *buf, &injector)
            .map_err(|e| CoreError::Validation(e.to_string()))?;
        let bytes = Bytes::copy_from_slice(buf);
        if buf.capacity() > 64 * 1024 {
            buf.shrink_to(16 * 1024);
        }
        Ok(bytes)
    })
}

pub(crate) fn resolve_target_format(format: AdapterFormat, fallback: TargetFormat) -> TargetFormat {
    match format {
        AdapterFormat::Mixed => fallback,
        AdapterFormat::Openai => TargetFormat::Openai,
        AdapterFormat::Anthropic => TargetFormat::Anthropic,
        AdapterFormat::Responses => TargetFormat::Responses,
        AdapterFormat::Atomesus => TargetFormat::Atomesus,
        AdapterFormat::CommandCodeGo => TargetFormat::CommandCodeGo,
        AdapterFormat::Gemini => TargetFormat::Gemini,
    }
}

pub(crate) fn target_format_path(target_format: TargetFormat) -> &'static str {
    match target_format {
        TargetFormat::Openai | TargetFormat::Gemini => "/chat/completions",
        TargetFormat::Anthropic => "/messages",
        TargetFormat::Responses => "/responses",
        TargetFormat::Atomesus => "/chat/atomesus",
        TargetFormat::CommandCodeGo => "/alpha/generate",
    }
}

/// Per-provider adapter. One concrete impl per upstream.
pub trait ProviderAdapter: Send + Sync {
    /// Stable identifier of this provider (e.g. `"openrouter"`).
    fn id(&self) -> &ProviderId {
        &self.config().id
    }

    /// Static configuration snapshot.
    fn config(&self) -> &ProviderAdapterConfig;

    /// Mutable configuration access (for dynamic extra headers / overrides).
    fn config_mut(&mut self) -> Option<&mut ProviderAdapterConfig> {
        None
    }

    /// Provider metadata for frontend/admin
    fn metadata(&self) -> ProviderMetadata {
        let built_in = true;
        ProviderMetadata {
            built_in,
            deletable: !built_in,
            supports_quota: false,
            quota_refresh_supported: false,
            requires_oauth: false,
            oauth_refresh_lead_seconds: None,
        }
    }

    /// Canonical provider IDs in models.dev that map to this adapter.
    fn models_dev_canonical_ids(&self) -> &'static [&'static str] {
        &[]
    }

    /// Whether this provider supports anonymous fallback requests without an API key.
    fn is_anonymous_fallback(&self) -> bool {
        false
    }

    /// Shortcut for `self.config().auth_type`.
    fn auth_type(&self) -> AdapterAuthType {
        self.config().auth_type
    }

    /// Shortcut for `self.config().format`.
    fn format(&self) -> AdapterFormat {
        self.config().format
    }

    /// Build the URL to POST a chat completion to.
    fn build_chat_url(&self, target_format: TargetFormat, model: &ModelId) -> String {
        let base_url = &self.config().base_url;
        if self.format() == AdapterFormat::Gemini {
            return format!(
                "{base_url}/models/{}:streamGenerateContent?alt=sse",
                model.as_str()
            );
        }
        let eff_format = resolve_target_format(self.format(), target_format);
        format!("{base_url}{}", target_format_path(eff_format))
    }

    /// Build the chat URL with account-level context (label).
    fn build_chat_url_for_account(
        &self,
        target_format: TargetFormat,
        model: &ModelId,
        _account_label: &str,
    ) -> String {
        self.build_chat_url(target_format, model)
    }

    /// Build the URL for audio transcription (Whisper).
    fn build_transcription_url(&self) -> String {
        format!("{}/audio/transcriptions", self.config().base_url)
    }

    /// Build the URL for embeddings.
    fn build_embeddings_url(&self) -> String {
        format!("{}/embeddings", self.config().base_url)
    }

    /// Format an embedding request for upstream OpenAI-compatible embeddings endpoints.
    fn format_embedding_request(
        &self,
        req: &openproxy_types::embeddings::EmbeddingRequest,
        upstream_model: &str,
    ) -> std::result::Result<bytes::Bytes, openproxy_types::error::CoreError> {
        inject_model_and_serialize(req, upstream_model)
    }

    /// Build the URL for image generation.
    fn build_image_url(&self) -> String {
        format!("{}/images/generations", self.config().base_url)
    }

    /// Build the URL for image edits.
    fn build_image_edits_url(&self) -> String {
        format!("{}/images/edits", self.config().base_url)
    }

    /// Build the URL for image variations.
    fn build_image_variations_url(&self) -> String {
        format!("{}/images/variations", self.config().base_url)
    }

    /// Format an image generation request for upstream OpenAI-compatible image endpoints.
    fn format_image_request(
        &self,
        req: &openproxy_types::images::ImageGenerationRequest,
        upstream_model: &str,
    ) -> std::result::Result<bytes::Bytes, openproxy_types::error::CoreError> {
        inject_model_and_serialize(req, upstream_model)
    }

    /// Build the URL for video generation.
    fn build_video_url(&self) -> String {
        format!("{}/video/generations", self.config().base_url)
    }

    /// Build the auth header pair `(header_name, header_value)` for the given API key.
    fn build_auth_header(&self, api_key: &str) -> Option<(String, String)> {
        match self.config().auth_type {
            AdapterAuthType::Bearer | AdapterAuthType::OAuth => {
                Some(("Authorization".into(), format!("Bearer {api_key}")))
            }
            AdapterAuthType::GoogApiKey => Some(("x-goog-api-key".into(), api_key.to_string())),
            AdapterAuthType::XApiKey => Some(("x-api-key".into(), api_key.to_string())),
            AdapterAuthType::None => None,
        }
    }

    /// Build the full set of request headers for a chat completion call.
    fn build_headers(
        &self,
        api_key: &str,
        _target_format: TargetFormat,
        _model: &ModelId,
    ) -> Vec<(String, String)> {
        let mut headers = Vec::with_capacity(2 + self.config().extra_headers.len());
        if let Some((name, value)) = self.build_auth_header(api_key) {
            headers.push((name, value));
        }
        headers.push(("Content-Type".into(), "application/json".into()));
        for (k, v) in &self.config().extra_headers {
            headers.push((k.clone(), v.clone()));
        }
        headers
    }

    /// URL of the provider's `/models` endpoint for live discovery.
    fn models_url(&self) -> Option<String> {
        Some(format!("{}/models", self.config().base_url))
    }

    /// Models URL with account-level context (label).
    fn models_url_for_account(&self, _account_label: &str) -> Option<String> {
        self.models_url()
    }

    /// Fetch the live model list using the provided hyper-based upstream client and API key.
    fn fetch_models(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
    ) -> impl std::future::Future<Output = Result<Vec<DiscoveredModel>>> + Send {
        async move {
            let url = self
                .models_url()
                .ok_or_else(|| CoreError::Internal(format!("{}: models_url is None", self.id())))?;
            let target_format = self.format().default_target_format();
            super::discovery::fetch_openai_models(
                &url,
                upstream_client,
                api_key,
                self.id().as_str(),
                target_format,
            )
            .await
        }
    }

    /// Fetch models with account-level context (label).
    fn fetch_models_for_account(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
        _account_label: &str,
    ) -> impl std::future::Future<Output = Result<Vec<DiscoveredModel>>> + Send {
        async move { self.fetch_models(upstream_client, api_key).await }
    }

    /// Fetch account quota from the provider.
    fn fetch_quota(
        &self,
        _: &Arc<UpstreamClient>,
        _: &str,
        _: Option<&str>,
        _: Option<&str>,
    ) -> impl std::future::Future<Output = Option<Result<openproxy_types::AccountQuota>>> + Send
    {
        std::future::ready(None)
    }

    /// Normalize an OpenAI request view before serialization.
    fn normalize_openai_request(&self, view: &mut openproxy_types::OpenAIRequestView) {
        if view.extra.contains_key("disabled") {
            view.extra.to_mut().remove("disabled");
        }
    }

    /// Allows the adapter to wrap or mutate the final request body before it is dispatched upstream.
    fn wrap_request_body(
        &self,
        body: bytes::Bytes,
        _target_format: TargetFormat,
        _model: &ModelId,
        _resolved_target: &openproxy_types::context::ResolvedTarget,
    ) -> std::result::Result<bytes::Bytes, openproxy_types::error::CoreError> {
        Ok(body)
    }

    /// Format/translate an OpenAI request into native request body bytes for this adapter.
    fn format_request(
        &self,
        target_format: TargetFormat,
        req: &openproxy_types::OpenAIRequest,
        model: &ModelId,
        messages: &[openproxy_types::OpenAIMessage],
        stream: bool,
    ) -> std::result::Result<bytes::Bytes, openproxy_types::error::CoreError> {
        let _ = target_format;
        let mut view =
            openproxy_types::OpenAIRequestView::new(req, model.as_str(), messages, stream);
        self.normalize_openai_request(&mut view);
        serde_json::to_vec(&view)
            .map(bytes::Bytes::from)
            .map_err(|e| {
                openproxy_types::error::CoreError::Parse(format!("serialize openai request: {e}"))
            })
    }

    /// Translate a non-streaming response JSON Value into an OpenAIResponse.
    fn translate_non_streaming_response(
        &self,
        target_format: TargetFormat,
        response_body: serde_json::Value,
    ) -> std::result::Result<openproxy_types::OpenAIResponse, openproxy_types::error::CoreError>
    {
        let _ = target_format;
        <openproxy_types::OpenAIResponse as serde::Deserialize>::deserialize(&response_body)
            .map_err(|e| {
                openproxy_types::error::CoreError::Parse(format!("parse openai response: {e}"))
            })
    }
}
