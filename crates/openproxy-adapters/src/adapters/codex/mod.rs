use super::{
    AdapterAuthType, AdapterFormat, Arc, CancellationToken, CoreError, DiscoveredModel, ModelId,
    ProviderAdapter, ProviderAdapterConfig, ProviderId, Result, TargetFormat, TimeoutProfile,
    UpstreamClient, UpstreamRequest,
};
use openproxy_types::ResultExt;

pub mod models;
pub mod quota;

#[cfg(test)]
mod tests;

pub use models::{
    CODEX_MODELS_URL, CODEX_UPSTREAM_MODELS_RAW_URL, codex_static_models, merge_codex_models,
    parse_codex_models_json,
};
pub use quota::parse_codex_usage_quota;

pub use crate::spoofer::CODEX_SPOOFING_HEADERS;
use crate::spoofer::{
    ClientSpoofer, CodexSpoofer, current_codex_ua, current_codex_version, refresh_codex_version,
};

pub fn codex_client_version() -> String {
    current_codex_version()
}

pub fn codex_user_agent() -> String {
    current_codex_ua()
}

pub fn codex_client_version_str() -> &'static str {
    crate::spoofer::DEFAULT_CODEX_VERSION
}

pub fn codex_user_agent_str() -> &'static str {
    "codex-cli/0.159.0 (Windows 10.0.26200; x64)"
}

pub fn apply_codex_spoofing_headers(req: &mut UpstreamRequest) {
    CodexSpoofer.apply_to_request(req);
}

pub async fn refresh_codex_cli_version(upstream_client: &Arc<UpstreamClient>) -> Option<String> {
    refresh_codex_version(upstream_client).await
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CodexAdapter {
    config: ProviderAdapterConfig,
}

impl CodexAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("codex"),
                name: "Codex".into(),
                anonymous_fallback: false,
                rate_limit_scope: "account".into(),
                base_url: "https://chatgpt.com/backend-api/codex".into(),
                auth_type: AdapterAuthType::OAuth,
                format: AdapterFormat::Responses,
                extra_headers: vec![],
            },
        }
    }

    pub fn build_hardcoded_codex_model(t: (&str, &str, i64, i64, bool)) -> DiscoveredModel {
        models::build_hardcoded_codex_model(t)
    }

    pub fn hardcoded_models() -> Vec<DiscoveredModel> {
        models::hardcoded_models()
    }
}

impl Default for CodexAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderAdapter for CodexAdapter {
    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn config_mut(&mut self) -> Option<&mut ProviderAdapterConfig> {
        Some(&mut self.config)
    }

    fn metadata(&self) -> openproxy_types::ProviderMetadata {
        openproxy_types::ProviderMetadata {
            built_in: true,
            deletable: false,
            supports_quota: true,
            quota_refresh_supported: true,
            requires_oauth: true,
            oauth_refresh_lead_seconds: Some(300),
        }
    }

    fn build_headers(
        &self,
        api_key: &str,
        _target_format: TargetFormat,
        _model: &ModelId,
    ) -> Vec<(String, String)> {
        super::build_spoofer_headers(
            self.build_auth_header(api_key),
            &CodexSpoofer,
            &self.config.extra_headers,
        )
    }

    fn wrap_request_body(
        &self,
        body: bytes::Bytes,
        _target_format: TargetFormat,
        _model: &ModelId,
        _resolved_target: &openproxy_types::context::ResolvedTarget,
    ) -> std::result::Result<bytes::Bytes, openproxy_types::error::CoreError> {
        crate::adapters::traits::patch_json_request_body(body, patch_codex_request_object)
    }

    fn models_url(&self) -> Option<String> {
        Some(CODEX_MODELS_URL.to_string())
    }

    async fn fetch_models(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        models::fetch_codex_models_pipeline(upstream_client, api_key).await
    }

    async fn fetch_quota(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        _: &str,
        access_token: Option<&str>,
        provider_specific: Option<&str>,
    ) -> Option<Result<openproxy_types::AccountQuota>> {
        if let Some(token) = access_token {
            Some(
                self.fetch_codex_quota_local(upstream_client, token, provider_specific)
                    .await,
            )
        } else {
            Some(Ok(openproxy_types::AccountQuota::with_error(
                "codex requires OAuth access token",
            )))
        }
    }
}

impl CodexAdapter {
    async fn fetch_codex_quota_local(
        &self,
        upstream: &Arc<UpstreamClient>,
        access_token: &str,
        workspace_id: Option<&str>,
    ) -> Result<openproxy_types::AccountQuota> {
        let req = quota::build_codex_quota_request(access_token, workspace_id);
        let cancel = CancellationToken::new();
        let response = upstream
            .call(req, TimeoutProfile::Chat, cancel)
            .await
            .map_err(|e| CoreError::UpstreamConnection(e.to_string()))?;

        let status = response.status.as_u16();
        if !response.status.is_success() {
            let body = response.collect().await.unwrap_or_default();
            let snippet = String::from_utf8_lossy(&body)
                .chars()
                .take(200)
                .collect::<String>();
            return Ok(quota::build_codex_error_quota(status, &snippet));
        }

        let body = response.collect().await.ctx_upstream("codex quota read")?;
        let json: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|e| CoreError::Parse(format!("codex quota parse: {e}")))?;
        quota::parse_codex_usage_quota(&json)
    }
}

pub fn patch_codex_request_object(obj: &mut serde_json::Map<String, serde_json::Value>) {
    // Codex backend ALWAYS requires stream: true, else it returns HTTP 400 {"detail":"Stream must be set to true"}
    obj.insert("stream".to_string(), serde_json::Value::Bool(true));

    // ChatGPT/Codex /backend-api/codex/responses strictly rejects standard Chat Completions
    // sampling and generation parameters with HTTP 400 {"detail":"Unsupported parameter: <name>"}
    const UNSUPPORTED_CODEX_PARAMS: &[&str] = &[
        "temperature",
        "top_p",
        "presence_penalty",
        "frequency_penalty",
        "max_tokens",
        "max_completion_tokens",
        "max_output_tokens",
        "stop",
        "n",
        "seed",
        "modalities",
        "user",
        "logit_bias",
        "logprobs",
        "top_logprobs",
        "stream_options",
        "reasoning_summary",
    ];

    for key in UNSUPPORTED_CODEX_PARAMS {
        obj.remove(*key);
    }

    // Map top-level reasoning_effort into reasoning.effort if not already mapped
    if let Some(effort) = obj.remove("reasoning_effort")
        && !obj.contains_key("reasoning")
    {
        obj.insert(
            "reasoning".to_string(),
            serde_json::json!({
                "effort": effort,
                "summary": "auto"
            }),
        );
    }

    // Map response_format into text.format if present
    if let Some(fmt) = obj.remove("response_format")
        && !obj.contains_key("text")
    {
        obj.insert(
            "text".to_string(),
            serde_json::json!({
                "format": fmt
            }),
        );
    }

    // Sanitize any input items or content parts with image data
    // OpenAI Responses API strictly rejects unknown parameter 'image' or 'mime_type'
    if let Some(input_arr) = obj
        .get_mut("input")
        .and_then(serde_json::Value::as_array_mut)
    {
        for item in input_arr {
            sanitize_codex_input_item(item);
        }
    }
}

fn sanitize_codex_image_part(part_obj: &mut serde_json::Map<String, serde_json::Value>) {
    let is_image = part_obj
        .get("type")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|t| t == "input_image" || t == "image");
    if is_image {
        part_obj.insert(
            "type".to_string(),
            serde_json::Value::String("input_image".to_string()),
        );
        let legacy_image = part_obj.remove("image").and_then(|v| match v {
            serde_json::Value::String(s) => Some(s),
            _ => None,
        });
        let legacy_mime = part_obj.remove("mime_type").and_then(|v| match v {
            serde_json::Value::String(s) => Some(s),
            _ => None,
        });
        if !part_obj.contains_key("image_url")
            && let Some(data) = legacy_image
        {
            let mime = legacy_mime.as_deref().unwrap_or("image/jpeg");
            let url = if data.starts_with("data:") {
                data
            } else {
                format!("data:{mime};base64,{data}")
            };
            part_obj.insert("image_url".to_string(), serde_json::Value::String(url));
        }
    }
}

fn sanitize_codex_input_item(item: &mut serde_json::Value) {
    if let Some(item_obj) = item.as_object_mut() {
        if let Some(content_arr) = item_obj
            .get_mut("content")
            .and_then(serde_json::Value::as_array_mut)
        {
            for part in content_arr {
                if let Some(part_obj) = part.as_object_mut() {
                    sanitize_codex_image_part(part_obj);
                }
            }
        } else if let Some(content_obj) = item_obj
            .get_mut("content")
            .and_then(serde_json::Value::as_object_mut)
        {
            sanitize_codex_image_part(content_obj);
        } else {
            sanitize_codex_image_part(item_obj);
        }
    }
}
