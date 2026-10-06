use crate::upstream::UpstreamClient;
use bytes::Bytes;
use openproxy_types::{
    CoreError, DiscoveredModel, ModelId, ProviderId, ProviderMetadata, Result, TargetFormat,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

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

impl ProviderAdapterConfig {
    /// Returns `Some(true)` to force streaming, `Some(false)` to force non-streaming (unary),
    /// or `None` for default/automatic behavior.
    pub fn forced_streaming(&self) -> Option<bool> {
        for (k, v) in &self.extra_headers {
            let k_lower = k.to_ascii_lowercase();
            if k_lower == "x-openproxy-stream"
                || k_lower == "x-openproxy-streaming"
                || k_lower == "x-openproxy-stream-mode"
                || k_lower == "x-openproxy-force-stream"
            {
                let v_lower = v.trim().to_ascii_lowercase();
                if v_lower == "false" || v_lower == "0" || v_lower == "off" || v_lower == "unary" || v_lower == "never" {
                    return Some(false);
                } else if v_lower == "true" || v_lower == "1" || v_lower == "on" || v_lower == "stream" || v_lower == "streaming" || v_lower == "always" {
                    return Some(true);
                }
            }
        }
        None
    }
}

pub type AdapterAuthType = openproxy_types::AuthType;
pub type AdapterFormat = openproxy_types::ProviderFormat;

thread_local! {
    static SERIALIZE_BUF: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

pub fn inject_model_and_serialize<T: Serialize>(
    req: &T,
    upstream_model: &str,
) -> std::result::Result<Bytes, CoreError> {
    let mut val = serde_json::to_value(req).map_err(|e| CoreError::Validation(e.to_string()))?;
    if let Some(obj) = val.as_object_mut() {
        obj.insert(
            "model".to_string(),
            serde_json::Value::String(upstream_model.to_string()),
        );
    }
    SERIALIZE_BUF.with_borrow_mut(|buf| {
        buf.clear();
        serde_json::to_writer(&mut *buf, &val).map_err(|e| CoreError::Validation(e.to_string()))?;
        let bytes = Bytes::copy_from_slice(buf);
        if buf.capacity() > 64 * 1024 {
            buf.shrink_to(16 * 1024);
        }
        Ok(bytes)
    })
}

pub fn patch_json_request_body<F>(
    body: bytes::Bytes,
    patcher: F,
) -> std::result::Result<bytes::Bytes, openproxy_types::error::CoreError>
where
    F: FnOnce(&mut serde_json::Map<String, serde_json::Value>),
{
    if body.is_empty() {
        return Ok(body);
    }
    let mut val: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|e| openproxy_types::error::CoreError::Parse(e.to_string()))?;

    if let Some(obj) = val.as_object_mut() {
        patcher(obj);
    }

    let new_body = serde_json::to_vec(&val)
        .map_err(|e| openproxy_types::error::CoreError::Parse(e.to_string()))?;
    Ok(bytes::Bytes::from(new_body))
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
        AdapterFormat::SystemOne => TargetFormat::SystemOne,
    }
}

pub(crate) fn target_format_path(target_format: TargetFormat) -> &'static str {
    match target_format {
        TargetFormat::Openai | TargetFormat::Gemini => "/chat/completions",
        TargetFormat::Anthropic => "/messages",
        TargetFormat::Responses => "/responses",
        TargetFormat::Atomesus => "/chat/atomesus",
        TargetFormat::CommandCodeGo => "/alpha/generate",
        TargetFormat::SystemOne => "/systemone",
    }
}

pub trait ProviderAdapter: Send + Sync {
    fn id(&self) -> &ProviderId {
        &self.config().id
    }

    fn config(&self) -> &ProviderAdapterConfig;

    fn config_mut(&mut self) -> Option<&mut ProviderAdapterConfig> {
        None
    }

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

    fn models_dev_canonical_ids(&self) -> &'static [&'static str] {
        &[]
    }

    fn is_anonymous_fallback(&self) -> bool {
        false
    }

    fn auth_type(&self) -> AdapterAuthType {
        self.config().auth_type
    }

    fn format(&self) -> AdapterFormat {
        self.config().format
    }

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

    fn build_chat_url_for_account(
        &self,
        target_format: TargetFormat,
        model: &ModelId,
        _account_label: &str,
    ) -> String {
        self.build_chat_url(target_format, model)
    }

    fn build_transcription_url(&self) -> String {
        format!("{}/audio/transcriptions", self.config().base_url)
    }

    fn build_embeddings_url(&self) -> String {
        format!("{}/embeddings", self.config().base_url)
    }

    fn format_embedding_request(
        &self,
        req: &openproxy_types::embeddings::EmbeddingRequest,
        upstream_model: &str,
    ) -> std::result::Result<bytes::Bytes, openproxy_types::error::CoreError> {
        inject_model_and_serialize(req, upstream_model)
    }

    fn build_image_url(&self) -> String {
        format!("{}/images/generations", self.config().base_url)
    }

    fn build_image_edits_url(&self) -> String {
        format!("{}/images/edits", self.config().base_url)
    }

    fn build_image_variations_url(&self) -> String {
        format!("{}/images/variations", self.config().base_url)
    }

    fn format_image_request(
        &self,
        req: &openproxy_types::images::ImageGenerationRequest,
        upstream_model: &str,
    ) -> std::result::Result<bytes::Bytes, openproxy_types::error::CoreError> {
        inject_model_and_serialize(req, upstream_model)
    }

    fn build_video_url(&self) -> String {
        format!("{}/video/generations", self.config().base_url)
    }

    fn build_system_one_url(&self) -> String {
        format!("{}/systemone", self.config().base_url)
    }

    fn format_system_one_request(
        &self,
        req: &openproxy_types::systemone::SystemOneRequest,
        upstream_model: &str,
    ) -> std::result::Result<bytes::Bytes, openproxy_types::error::CoreError> {
        let mut normalized = req.clone();
        for q in normalized.questions.values_mut() {
            if q.criteria.is_none()
                && let Some(opts) = &q.options
            {
                match q.question_type {
                    openproxy_types::systemone::SystemOneQuestionType::Choice => {
                        let mut map = serde_json::Map::with_capacity(opts.len());
                        for opt in opts {
                            map.insert(opt.clone(), serde_json::Value::String(opt.clone()));
                        }
                        q.criteria = Some(serde_json::Value::Object(map));
                    }
                    openproxy_types::systemone::SystemOneQuestionType::Score => {
                        q.criteria = Some(serde_json::json!(opts));
                    }
                    openproxy_types::systemone::SystemOneQuestionType::Noul => {}
                }
            }
        }
        inject_model_and_serialize(&normalized, upstream_model)
    }

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

    fn forced_streaming(&self) -> Option<bool> {
        self.config().forced_streaming()
    }

    fn models_url(&self) -> Option<String> {
        Some(format!("{}/models", self.config().base_url))
    }

    fn models_url_for_account(&self, _account_label: &str) -> Option<String> {
        self.models_url()
    }

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

    fn fetch_models_for_account(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
        _account_label: &str,
    ) -> impl std::future::Future<Output = Result<Vec<DiscoveredModel>>> + Send {
        async move { self.fetch_models(upstream_client, api_key).await }
    }

    fn fetch_models_with_proxy(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
        _proxy_url: Option<&str>,
    ) -> impl std::future::Future<Output = Result<Vec<DiscoveredModel>>> + Send {
        self.fetch_models(upstream_client, api_key)
    }

    fn fetch_models_for_account_with_proxy(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
        account_label: &str,
        _proxy_url: Option<&str>,
    ) -> impl std::future::Future<Output = Result<Vec<DiscoveredModel>>> + Send {
        self.fetch_models_for_account(upstream_client, api_key, account_label)
    }

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

    fn fetch_quota_with_proxy(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
        access_token: Option<&str>,
        provider_specific: Option<&str>,
        _proxy_url: Option<&str>,
    ) -> impl std::future::Future<Output = Option<Result<openproxy_types::AccountQuota>>> + Send
    {
        self.fetch_quota(upstream_client, api_key, access_token, provider_specific)
    }

    fn normalize_openai_request(&self, view: &mut openproxy_types::OpenAIRequestView) {
        if view.extra.contains_key("disabled") {
            view.extra.to_mut().remove("disabled");
        }
    }

    fn wrap_request_body(
        &self,
        body: bytes::Bytes,
        _target_format: TargetFormat,
        _model: &ModelId,
        _resolved_target: &openproxy_types::context::ResolvedTarget,
    ) -> std::result::Result<bytes::Bytes, openproxy_types::error::CoreError> {
        Ok(body)
    }

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

pub fn build_spoofer_headers(
    auth: Option<(String, String)>,
    spoofer: &impl crate::spoofer::ClientSpoofer,
    extra_headers: &[(String, String)],
) -> Vec<(String, String)> {
    let mut headers = Vec::with_capacity(8 + extra_headers.len());
    if let Some(auth) = auth {
        headers.push(auth);
    }
    headers.push(("Content-Type".into(), "application/json".into()));
    headers.extend(spoofer.headers());
    crate::spoofer::merge_header_refs(&mut headers, extra_headers);
    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_patch_json_request_body_empty() {
        let empty = Bytes::new();
        let res = patch_json_request_body(empty.clone(), |_| {}).unwrap();
        assert_eq!(res, empty);
    }

    #[test]
    fn test_patch_json_request_body_mutates_fields() {
        let input = Bytes::from(r#"{"model":"gpt-4","stream":false}"#);
        let res = patch_json_request_body(input, |obj| {
            obj.insert("stream".to_string(), serde_json::Value::Bool(true));
            obj.insert(
                "model".to_string(),
                serde_json::Value::String("gpt-4o".to_string()),
            );
        })
        .unwrap();
        let val: serde_json::Value = serde_json::from_slice(&res).unwrap();
        assert_eq!(val["stream"], true);
        assert_eq!(val["model"], "gpt-4o");
    }

    #[test]
    fn test_patch_json_request_body_invalid_json_returns_error() {
        let invalid = Bytes::from("not valid json");
        let res = patch_json_request_body(invalid, |_| {});
        assert!(res.is_err());
    }
}
