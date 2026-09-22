use super::{
    AdapterAuthType, AdapterFormat, Arc, DiscoveredModel, ModelId, ProviderAdapter,
    ProviderAdapterConfig, ProviderId, Result, TargetFormat, UpstreamClient, UpstreamRequest,
};
use crate::upstream::{CancellationToken, TimeoutProfile};

pub mod quota;
#[cfg(test)]
mod tests;

pub use crate::spoofer::{
    CODEBUDDY_SPOOFING_HEADERS, DEFAULT_CODEBUDDY_VERSION, current_codebuddy_ua,
    current_codebuddy_version, reset_dynamic_codebuddy_overrides,
    set_dynamic_codebuddy_extra_header, set_dynamic_codebuddy_ua, set_dynamic_codebuddy_version,
};
use crate::spoofer::{ClientSpoofer, CodeBuddySpoofer};
pub use quota::{
    CODEBUDDY_ACCOUNTS_URL, CODEBUDDY_GET_USER_RESOURCE_SUMMARY_URL,
    CODEBUDDY_GET_USER_RESOURCE_URL, CODEBUDDY_MODEL_CREDIT_COSTS, CodeBuddyModelCreditCost,
    build_codebuddy_accounts_request, build_codebuddy_quota_model_details,
    build_codebuddy_resource_request, calculate_next_midnight_cst_unix_secs,
    fetch_codebuddy_quota_unified, parse_codebuddy_accounts_quota, parse_codebuddy_resource_quota,
    parse_cst_datetime_to_unix_secs,
};

pub fn apply_codebuddy_spoofing_headers(req: &mut UpstreamRequest) {
    CodeBuddySpoofer.apply_to_request(req);
}

/// Default NPM registry metadata URL for CodeBuddy CLI package `@tencent-ai/codebuddy-code`.
pub const NPM_CODEBUDDY_METADATA_URL: &str =
    "https://registry.npmjs.org/@tencent-ai/codebuddy-code/latest";

/// Canonical NPM registry URL for CodeBuddy CLI, configurable via `OPENPROXY_CODEBUDDY_NPM_METADATA_URL`.
pub fn codebuddy_npm_metadata_url() -> String {
    std::env::var("OPENPROXY_CODEBUDDY_NPM_METADATA_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| NPM_CODEBUDDY_METADATA_URL.to_string())
}

/// Returns the current dynamic CodeBuddy version.
/// Respects `OPENPROXY_CODEBUDDY_VERSION` env var if set.
pub fn get_codebuddy_version() -> String {
    current_codebuddy_version()
}

/// Updates the dynamic CodeBuddy version in memory.
pub fn set_codebuddy_version(version: String) {
    set_dynamic_codebuddy_version(version);
}

/// Dynamic CodeBuddy User-Agent string.
pub fn get_codebuddy_ua() -> String {
    current_codebuddy_ua()
}

/// Updates the dynamic CodeBuddy User-Agent in memory.
pub fn set_codebuddy_ua(ua: impl Into<String>) {
    set_dynamic_codebuddy_ua(ua);
}

/// Asynchronously queries npm registry for the latest `@tencent-ai/codebuddy-code` CLI version
/// and updates the in-memory dynamic header state if changed.
pub async fn refresh_codebuddy_version(upstream_client: &Arc<UpstreamClient>) -> Option<String> {
    let url = codebuddy_npm_metadata_url();
    let mut req = UpstreamRequest::get(&url);
    if let Ok(accept) = http::HeaderValue::from_str("application/json") {
        req.headers.insert(http::header::ACCEPT, accept);
    }
    if let Ok(token) = std::env::var("OPENPROXY_CODEBUDDY_NPM_AUTH_TOKEN") {
        let trimmed = token.trim();
        if !trimmed.is_empty() {
            let auth_val = if trimmed.starts_with("Bearer ") {
                trimmed.to_string()
            } else {
                format!("Bearer {trimmed}")
            };
            if let Ok(hv) = http::HeaderValue::from_str(&auth_val) {
                req.headers.insert(http::header::AUTHORIZATION, hv);
            }
        }
    }
    let cancel = CancellationToken::new();
    let Ok(resp) = upstream_client
        .call(req, TimeoutProfile::Quota, cancel)
        .await
    else {
        return None;
    };
    if !resp.status.is_success() {
        return None;
    }
    let Ok(body) = resp.collect().await else {
        return None;
    };
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&body) {
        let ver_opt = v
            .get("version")
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                v.get("dist-tags")
                    .and_then(|dt| dt.get("latest"))
                    .and_then(serde_json::Value::as_str)
            });
        if let Some(ver) = ver_opt {
            let ver_trimmed = ver.trim();
            if !ver_trimmed.is_empty() {
                let ver_string = ver_trimmed.to_string();
                set_codebuddy_version(ver_string.clone());
                return Some(ver_string);
            }
        }
    }
    None
}

/// Alias for `refresh_codebuddy_version` following the CLI refresh naming convention.
pub async fn refresh_codebuddy_cli_version(
    upstream_client: &Arc<UpstreamClient>,
) -> Option<String> {
    refresh_codebuddy_version(upstream_client).await
}

/// Adapter for CodeBuddy (<https://www.codebuddy.ai/v2>).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CodeBuddyAdapter {
    config: ProviderAdapterConfig,
}

impl CodeBuddyAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("codebuddy"),
                name: "CodeBuddy".into(),
                anonymous_fallback: false,
                rate_limit_scope: "account".into(),
                base_url: "https://www.codebuddy.ai/v2".into(),
                auth_type: AdapterAuthType::OAuth,
                format: AdapterFormat::Openai,
                extra_headers: vec![],
            },
        }
    }
}

crate::adapters::derive_default_from_new!(CodeBuddyAdapter);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CodeBuddyModelDef {
    pub id: &'static str,
    pub name: &'static str,
    pub context_length: u32,
    pub max_output: u32,
    pub tools: bool,
    pub images: bool,
    pub reasoning: bool,
    pub model_type: &'static str,
    pub credit_cost: f64,
}

pub const CODEBUDDY_MODELS: &[CodeBuddyModelDef] = &[
    CodeBuddyModelDef { id: "default-model", name: "Auto", context_length: 176_000, max_output: 24_000, tools: true, images: true, reasoning: true, model_type: "chat", credit_cost: 0.79 },
    CodeBuddyModelDef { id: "default-model-lite", name: "Default-Lite", context_length: 176_000, max_output: 24_000, tools: true, images: true, reasoning: true, model_type: "chat", credit_cost: 0.67 },
    CodeBuddyModelDef { id: "fast-model", name: "Fast", context_length: 200_000, max_output: 32_000, tools: true, images: true, reasoning: true, model_type: "chat", credit_cost: 0.34 },
    CodeBuddyModelDef { id: "balanced-model", name: "Balanced", context_length: 256_000, max_output: 32_000, tools: true, images: true, reasoning: true, model_type: "chat", credit_cost: 0.59 },
    CodeBuddyModelDef { id: "primary-model", name: "Primary", context_length: 272_000, max_output: 72_000, tools: true, images: true, reasoning: true, model_type: "chat", credit_cost: 3.31 },
    CodeBuddyModelDef { id: "deep-model", name: "Deep", context_length: 176_000, max_output: 24_000, tools: true, images: true, reasoning: false, model_type: "chat", credit_cost: 3.33 },
    CodeBuddyModelDef { id: "gpt-5.5", name: "GPT-5.5", context_length: 1_000_000, max_output: 72_000, tools: true, images: true, reasoning: true, model_type: "chat", credit_cost: 3.31 },
    CodeBuddyModelDef { id: "gpt-5.4", name: "GPT-5.4", context_length: 272_000, max_output: 128_000, tools: true, images: true, reasoning: true, model_type: "chat", credit_cost: 1.65 },
    CodeBuddyModelDef { id: "gpt-5.3-codex", name: "GPT-5.3-Codex", context_length: 272_000, max_output: 128_000, tools: true, images: true, reasoning: true, model_type: "chat", credit_cost: 1.25 },
    CodeBuddyModelDef { id: "gpt-5.1-codex", name: "GPT-5.1-Codex", context_length: 272_000, max_output: 128_000, tools: true, images: true, reasoning: true, model_type: "chat", credit_cost: 0.90 },
    CodeBuddyModelDef { id: "gpt-5.1-codex-mini", name: "GPT-5.1-Codex-Mini", context_length: 272_000, max_output: 128_000, tools: true, images: true, reasoning: true, model_type: "chat", credit_cost: 0.18 },
    CodeBuddyModelDef { id: "gemini-3.1-pro", name: "Gemini-3.1-Pro", context_length: 400_000, max_output: 64_000, tools: true, images: true, reasoning: true, model_type: "chat", credit_cost: 1.32 },
    CodeBuddyModelDef { id: "gemini-3.0-flash", name: "Gemini-3.0-Flash", context_length: 400_000, max_output: 64_000, tools: true, images: true, reasoning: true, model_type: "chat", credit_cost: 0.33 },
    CodeBuddyModelDef { id: "gemini-3.5-flash", name: "Gemini-3.5-Flash", context_length: 1_000_000, max_output: 65_536, tools: true, images: true, reasoning: true, model_type: "chat", credit_cost: 0.99 },
    CodeBuddyModelDef { id: "gemini-2.5-flash", name: "Gemini-2.5-Flash", context_length: 400_000, max_output: 64_000, tools: true, images: true, reasoning: true, model_type: "chat", credit_cost: 0.22 },
    CodeBuddyModelDef { id: "gemini-3.1-flash-lite", name: "Gemini-3.1-flash-lite", context_length: 200_000, max_output: 65_536, tools: true, images: true, reasoning: true, model_type: "chat", credit_cost: 0.17 },
    CodeBuddyModelDef { id: "gemini-2.5-pro", name: "Gemini-2.5-Pro", context_length: 400_000, max_output: 64_000, tools: true, images: true, reasoning: true, model_type: "chat", credit_cost: 0.90 },
    CodeBuddyModelDef { id: "deepseek-v3-2-volc", name: "DeepSeek-V3.2", context_length: 96_000, max_output: 32_000, tools: true, images: false, reasoning: true, model_type: "chat", credit_cost: 0.29 },
    CodeBuddyModelDef { id: "glm-5.0", name: "GLM-5.0", context_length: 200_000, max_output: 48_000, tools: true, images: false, reasoning: true, model_type: "chat", credit_cost: 0.80 },
    CodeBuddyModelDef { id: "kimi-k2.5", name: "Kimi-K2.5", context_length: 164_000, max_output: 32_000, tools: true, images: true, reasoning: true, model_type: "chat", credit_cost: 0.45 },
    CodeBuddyModelDef { id: "gemini-3.0-pro-image", name: "Gemini-3.0-Pro-Image", context_length: 0, max_output: 0, tools: false, images: false, reasoning: false, model_type: "image", credit_cost: 4.96 },
    CodeBuddyModelDef { id: "gemini-3.1-flash-image", name: "Gemini-3.1-Flash-Image", context_length: 0, max_output: 0, tools: false, images: false, reasoning: false, model_type: "image", credit_cost: 1.78 },
    CodeBuddyModelDef { id: "gemini-2.5-flash-image", name: "Gemini-2.5-Flash-Image", context_length: 0, max_output: 0, tools: false, images: false, reasoning: false, model_type: "image", credit_cost: 1.14 },
    CodeBuddyModelDef { id: "hunyuan-image-v3.0", name: "Hunyuan-Image-V3", context_length: 0, max_output: 0, tools: false, images: false, reasoning: false, model_type: "image", credit_cost: 5.00 },
    CodeBuddyModelDef { id: "hunyuan-image-v2.0-general-edit", name: "Hunyuan-Image-Edit", context_length: 0, max_output: 0, tools: false, images: false, reasoning: false, model_type: "image", credit_cost: 5.00 },
    CodeBuddyModelDef { id: "hunyuan-video-art", name: "Hunyuan-Video-Art", context_length: 0, max_output: 0, tools: false, images: false, reasoning: false, model_type: "video", credit_cost: 10.00 },
    CodeBuddyModelDef { id: "gpt-5.6-sol", name: "GPT-5.6-Sol", context_length: 1_000_000, max_output: 128_000, tools: true, images: true, reasoning: true, model_type: "chat", credit_cost: 3.47 },
    CodeBuddyModelDef { id: "gpt-5.6-terra", name: "GPT-5.6-Terra", context_length: 1_000_000, max_output: 128_000, tools: true, images: true, reasoning: true, model_type: "chat", credit_cost: 1.39 },
    CodeBuddyModelDef { id: "gpt-5.6-luna", name: "GPT-5.6-Luna", context_length: 1_000_000, max_output: 128_000, tools: true, images: true, reasoning: true, model_type: "chat", credit_cost: 0.14 },
    CodeBuddyModelDef { id: "glm-5.3", name: "GLM-5.3", context_length: 1_000_000, max_output: 48_000, tools: true, images: true, reasoning: true, model_type: "chat", credit_cost: 0.79 },
    CodeBuddyModelDef { id: "glm-5.2", name: "GLM-5.2", context_length: 1_000_000, max_output: 48_000, tools: true, images: true, reasoning: true, model_type: "chat", credit_cost: 0.79 },
    CodeBuddyModelDef { id: "hy3", name: "Hy3", context_length: 192_000, max_output: 64_000, tools: true, images: true, reasoning: true, model_type: "chat", credit_cost: 0.00 },
    CodeBuddyModelDef { id: "kimi-k3", name: "Kimi-K3", context_length: 1_000_000, max_output: 32_000, tools: true, images: true, reasoning: true, model_type: "chat", credit_cost: 1.62 },
    CodeBuddyModelDef { id: "kimi-k2.6", name: "Kimi-K2.6", context_length: 256_000, max_output: 32_000, tools: true, images: true, reasoning: true, model_type: "chat", credit_cost: 0.52 },
    CodeBuddyModelDef { id: "minimax-m3", name: "MiniMax-M3", context_length: 512_000, max_output: 128_000, tools: true, images: true, reasoning: true, model_type: "chat", credit_cost: 0.25 },
];

pub fn codebuddy_static_models() -> Vec<DiscoveredModel> {
    CODEBUDDY_MODELS
        .iter()
        .map(|def| {
            let caps = openproxy_types::ModelCapabilities {
                vision: Some(def.images),
                tool_calling: Some(def.tools),
                reasoning: Some(def.reasoning),
                thinking: Some(def.reasoning),
                ..Default::default()
            };
            DiscoveredModel {
                model_id: ModelId::new(def.id),
                display_name: Some(def.name.to_string()),
                target_format: TargetFormat::Openai,
                context_length: if def.context_length > 0 { Some(i64::from(def.context_length)) } else { None },
                max_output_tokens: if def.max_output > 0 { Some(i64::from(def.max_output)) } else { None },
                input_modalities: None,
                output_modalities: None,
                model_type: Some(def.model_type.to_string()),
                family: None,
                capabilities: Some(caps),
            }
        })
        .collect()
}

impl ProviderAdapter for CodeBuddyAdapter {
    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn config_mut(&mut self) -> Option<&mut ProviderAdapterConfig> {
        Some(&mut self.config)
    }

    fn metadata(&self) -> openproxy_types::ProviderMetadata {
        let mut meta = openproxy_types::ProviderMetadata::custom_default();
        meta.built_in = true;
        meta.deletable = false;
        meta.supports_quota = true;
        meta.quota_refresh_supported = true;
        meta.requires_oauth = true;
        meta.oauth_refresh_lead_seconds = Some(300);
        meta
    }

    fn models_url(&self) -> Option<String> {
        None
    }

    fn build_auth_header(&self, access_token: &str) -> Option<(String, String)> {
        let trimmed = access_token.trim();
        if trimmed.is_empty() {
            return None;
        }
        let val = if trimmed.starts_with("Bearer ") {
            trimmed.to_string()
        } else {
            format!("Bearer {trimmed}")
        };
        Some(("Authorization".into(), val))
    }

    fn build_headers(
        &self,
        access_token: &str,
        _target_format: TargetFormat,
        _model: &ModelId,
    ) -> Vec<(String, String)> {
        super::build_spoofer_headers(
            self.build_auth_header(access_token),
            &CodeBuddySpoofer,
            &self.config.extra_headers,
        )
    }

    fn build_chat_url(&self, target_format: TargetFormat, model: &ModelId) -> String {
        let _ = (target_format, model);
        format!("{}/chat/completions", self.config.base_url)
    }

    async fn fetch_models(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        _api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        // Opportunistically trigger a background refresh of the CLI version from npm registry
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let client_clone = Arc::clone(upstream_client);
            handle.spawn(async move {
                let _ = refresh_codebuddy_version(&client_clone).await;
            });
        }
        Ok(codebuddy_static_models())
    }

    fn models_dev_canonical_ids(&self) -> &'static [&'static str] {
        &["codebuddy", "tencent"]
    }

    async fn fetch_quota(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
        access_token: Option<&str>,
        provider_specific: Option<&str>,
    ) -> Option<Result<openproxy_types::AccountQuota>> {
        self.fetch_quota_with_proxy(upstream_client, api_key, access_token, provider_specific, None)
            .await
    }

    async fn fetch_quota_with_proxy(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
        access_token: Option<&str>,
        provider_specific: Option<&str>,
        proxy_url: Option<&str>,
    ) -> Option<Result<openproxy_types::AccountQuota>> {
        let token = access_token.unwrap_or(api_key).trim();
        if token.is_empty() {
            return Some(Ok(openproxy_types::AccountQuota::with_error(
                "codebuddy requires OAuth access token for quota",
            )));
        }

        Some(
            fetch_codebuddy_quota_unified(
                upstream_client,
                token,
                provider_specific,
                proxy_url,
            )
            .await,
        )
    }
    fn normalize_openai_request(&self, view: &mut openproxy_types::OpenAIRequestView) {
        // CodeBuddy upstream rejects non-streaming chat requests with error 11101.
        // Forcing stream = true allows the pipeline unary dispatcher to accumulate
        // the SSE stream into a unary OpenAIResponse seamlessly for non-streaming clients.
        view.stream = true;

        // CodeBuddy upstream security policy requires first message to be role: "system" (error 11128).
        ensure_codebuddy_system_prompt_in_view(view);
    }

    fn wrap_request_body(
        &self,
        body: bytes::Bytes,
        _target_format: TargetFormat,
        _model: &ModelId,
        _resolved_target: &openproxy_types::context::ResolvedTarget,
    ) -> std::result::Result<bytes::Bytes, openproxy_types::error::CoreError> {
        let mut val: serde_json::Value = serde_json::from_slice(&body).map_err(|e| {
            openproxy_types::error::CoreError::Parse(format!(
                "failed to parse request body for codebuddy wrapping: {e}"
            ))
        })?;

        if let Some(obj) = val.as_object_mut() {
            // Guarantee stream: true for CodeBuddy upstream chat completions
            obj.insert("stream".to_string(), serde_json::Value::Bool(true));

            // Guarantee first message is system prompt for CodeBuddy security policy (code 11128)
            if let Some(messages) = obj.get_mut("messages").and_then(|m| m.as_array_mut()) {
                ensure_codebuddy_system_prompt_json(messages);
            }
        }

        let re_encoded = serde_json::to_vec(&val).map_err(|e| {
            openproxy_types::error::CoreError::Parse(format!(
                "failed to re-encode request body for codebuddy wrapping: {e}"
            ))
        })?;

        Ok(bytes::Bytes::from(re_encoded))
    }
}

pub const DEFAULT_CODEBUDDY_SYSTEM_PROMPT: &str = "You are CodeBuddy, a helpful AI coding assistant.";

/// Ensures that `messages` in `OpenAIRequestView` starts with a system prompt message.
pub fn ensure_codebuddy_system_prompt_in_view(view: &mut openproxy_types::OpenAIRequestView) {
    let messages = view.messages.to_mut();
    if messages.is_empty() {
        messages.push(openproxy_types::OpenAIMessage {
            role: "system".to_string(),
            content: Some(serde_json::Value::String(DEFAULT_CODEBUDDY_SYSTEM_PROMPT.to_string())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::default(),
        });
        return;
    }

    if messages[0].role == "system" || messages[0].role == "developer" {
        if messages[0].role == "developer" {
            messages[0].role = "system".to_string();
        }
        let is_empty = match &messages[0].content {
            None => true,
            Some(serde_json::Value::Null) => true,
            Some(serde_json::Value::String(s)) => s.trim().is_empty(),
            Some(serde_json::Value::Array(a)) => a.is_empty(),
            _ => false,
        };
        if is_empty {
            messages[0].content = Some(serde_json::Value::String(DEFAULT_CODEBUDDY_SYSTEM_PROMPT.to_string()));
        }
        return;
    }

    if let Some(sys_idx) = messages.iter().position(|m| m.role == "system" || m.role == "developer") {
        let mut sys_msg = messages.remove(sys_idx);
        sys_msg.role = "system".to_string();
        let is_empty = match &sys_msg.content {
            None => true,
            Some(serde_json::Value::Null) => true,
            Some(serde_json::Value::String(s)) => s.trim().is_empty(),
            Some(serde_json::Value::Array(a)) => a.is_empty(),
            _ => false,
        };
        if is_empty {
            sys_msg.content = Some(serde_json::Value::String(DEFAULT_CODEBUDDY_SYSTEM_PROMPT.to_string()));
        }
        messages.insert(0, sys_msg);
    } else {
        messages.insert(0, openproxy_types::OpenAIMessage {
            role: "system".to_string(),
            content: Some(serde_json::Value::String(DEFAULT_CODEBUDDY_SYSTEM_PROMPT.to_string())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::default(),
        });
    }
}

/// Ensures that a raw JSON `messages` array starts with a system prompt message.
pub fn ensure_codebuddy_system_prompt_json(messages: &mut Vec<serde_json::Value>) {
    if messages.is_empty() {
        messages.push(serde_json::json!({
            "role": "system",
            "content": DEFAULT_CODEBUDDY_SYSTEM_PROMPT,
        }));
        return;
    }

    let first_role = messages[0].get("role").and_then(|r| r.as_str()).unwrap_or("");
    if first_role == "system" || first_role == "developer" {
        if first_role == "developer"
            && let Some(obj) = messages[0].as_object_mut()
        {
            obj.insert("role".to_string(), serde_json::Value::String("system".to_string()));
        }
        let content_empty = messages[0].get("content").is_none_or(|c| {
            c.is_null()
                || (c.is_string() && c.as_str().unwrap_or("").trim().is_empty())
                || (c.is_array() && c.as_array().is_some_and(|a| a.is_empty()))
        });
        if content_empty
            && let Some(obj) = messages[0].as_object_mut()
        {
            obj.insert(
                "content".to_string(),
                serde_json::Value::String(DEFAULT_CODEBUDDY_SYSTEM_PROMPT.to_string()),
            );
        }
        return;
    }

    if let Some(sys_idx) = messages.iter().position(|m| {
        let r = m.get("role").and_then(|v| v.as_str()).unwrap_or("");
        r == "system" || r == "developer"
    }) {
        let mut sys_msg = messages.remove(sys_idx);
        if let Some(obj) = sys_msg.as_object_mut() {
            obj.insert("role".to_string(), serde_json::Value::String("system".to_string()));
            let content_empty = obj.get("content").is_none_or(|c| {
                c.is_null()
                    || (c.is_string() && c.as_str().unwrap_or("").trim().is_empty())
                    || (c.is_array() && c.as_array().is_some_and(|a| a.is_empty()))
            });
            if content_empty {
                obj.insert(
                    "content".to_string(),
                    serde_json::Value::String(DEFAULT_CODEBUDDY_SYSTEM_PROMPT.to_string()),
                );
            }
        }
        messages.insert(0, sys_msg);
    } else {
        messages.insert(0, serde_json::json!({
            "role": "system",
            "content": DEFAULT_CODEBUDDY_SYSTEM_PROMPT,
        }));
    }
}

/// Known business error codes from CodeBuddy / Tencent Cloud Copilot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeBuddyErrorCode {
    // Protocol constraints
    NonStreamNotSupported = 11101,
    FirstMessageNotSystemPrompt = 11128,
    // 6000..=6008 rate limits
    CraftRateLimit = 6000,
    CraftRateTPSLimit = 6001,
    CraftRateTPMLimit = 6002,
    CraftRateTPHLimit = 6003,
    CraftRateTPDLimit = 6004,
    CraftRateRPSLimit = 6005,
    CraftRateRPMLimit = 6006,
    CraftRateRPHLimit = 6007,
    CraftRateRPDLimit = 6008,

    // Quota & credits
    UsageLimitExceeded = 14001,
    ConversationChatTooMany = 14002,
    RateLimitError = 14003,
    UsageLimitExceededEnterprise = 14012,
    UsageLimitExceededTencent = 14013,
    UsageLimitEnterpriseExhausted = 14014,
    UsageLimitLicenseExpired = 14015,
    UsageLimitEnterpriseNotActivated = 14016,
    UsageLimitUserNotActivated = 14017,
    UsageLimitUserExhausted = 14018,
    UsageLimitNoTokenBudget = 14019,

    // Session & web search & context
    ConversationLimitExceeded = 10105,
    WebSearchRateLimit = 15001,
    ContextTooLong = 11115,

    // Auth
    AuthForbidden1 = 11140,
    ModelBehaviorError = 11141,
    AuthForbidden2 = 11142,
}

impl CodeBuddyErrorCode {
    pub fn from_code(code: u32) -> Option<Self> {
        match code {
            6000 => Some(Self::CraftRateLimit),
            6001 => Some(Self::CraftRateTPSLimit),
            6002 => Some(Self::CraftRateTPMLimit),
            6003 => Some(Self::CraftRateTPHLimit),
            6004 => Some(Self::CraftRateTPDLimit),
            6005 => Some(Self::CraftRateRPSLimit),
            6006 => Some(Self::CraftRateRPMLimit),
            6007 => Some(Self::CraftRateRPHLimit),
            6008 => Some(Self::CraftRateRPDLimit),
            14001 => Some(Self::UsageLimitExceeded),
            14002 => Some(Self::ConversationChatTooMany),
            14003 => Some(Self::RateLimitError),
            14012 => Some(Self::UsageLimitExceededEnterprise),
            14013 => Some(Self::UsageLimitExceededTencent),
            14014 => Some(Self::UsageLimitEnterpriseExhausted),
            14015 => Some(Self::UsageLimitLicenseExpired),
            14016 => Some(Self::UsageLimitEnterpriseNotActivated),
            14017 => Some(Self::UsageLimitUserNotActivated),
            14018 => Some(Self::UsageLimitUserExhausted),
            14019 => Some(Self::UsageLimitNoTokenBudget),
            10105 => Some(Self::ConversationLimitExceeded),
            15001 => Some(Self::WebSearchRateLimit),
            11101 => Some(Self::NonStreamNotSupported),
            11128 => Some(Self::FirstMessageNotSystemPrompt),
            11115 => Some(Self::ContextTooLong),
            11140 => Some(Self::AuthForbidden1),
            11141 => Some(Self::ModelBehaviorError),
            11142 => Some(Self::AuthForbidden2),
            _ => None,
        }
    }

    pub fn is_rate_limit(self) -> bool {
        matches!(
            self,
            Self::CraftRateLimit
                | Self::CraftRateTPSLimit
                | Self::CraftRateTPMLimit
                | Self::CraftRateTPHLimit
                | Self::CraftRateTPDLimit
                | Self::CraftRateRPSLimit
                | Self::CraftRateRPMLimit
                | Self::CraftRateRPHLimit
                | Self::CraftRateRPDLimit
                | Self::RateLimitError
                | Self::WebSearchRateLimit
        )
    }

    pub fn is_credits_exhausted(self) -> bool {
        matches!(
            self,
            Self::UsageLimitExceeded
                | Self::ConversationChatTooMany
                | Self::UsageLimitExceededEnterprise
                | Self::UsageLimitExceededTencent
                | Self::UsageLimitEnterpriseExhausted
                | Self::UsageLimitUserExhausted
                | Self::UsageLimitNoTokenBudget
        )
    }

    pub fn is_auth_error(self) -> bool {
        matches!(
            self,
            Self::UsageLimitLicenseExpired
                | Self::UsageLimitEnterpriseNotActivated
                | Self::UsageLimitUserNotActivated
                | Self::AuthForbidden1
                | Self::AuthForbidden2
        )
    }

    pub fn error_subcategory(self) -> &'static str {
        match self {
            Self::CraftRateLimit
            | Self::CraftRateTPSLimit
            | Self::CraftRateTPMLimit
            | Self::CraftRateTPHLimit
            | Self::CraftRateTPDLimit => "quota_token_limit",
            Self::CraftRateRPSLimit
            | Self::CraftRateRPMLimit
            | Self::CraftRateRPHLimit
            | Self::CraftRateRPDLimit
            | Self::RateLimitError => "quota_request_limit",
            Self::UsageLimitExceeded
            | Self::ConversationChatTooMany
            | Self::UsageLimitExceededEnterprise
            | Self::UsageLimitExceededTencent
            | Self::UsageLimitEnterpriseExhausted
            | Self::UsageLimitUserExhausted
            | Self::UsageLimitNoTokenBudget => "quota_balance_exhausted",
            Self::UsageLimitLicenseExpired => "auth_expired",
            Self::UsageLimitEnterpriseNotActivated | Self::UsageLimitUserNotActivated => {
                "quota_not_activated"
            }
            Self::ConversationLimitExceeded => "quota_active_session",
            Self::WebSearchRateLimit => "quota_web_search",
            Self::NonStreamNotSupported => "non_stream_not_supported",
            Self::FirstMessageNotSystemPrompt => "first_message_not_system_prompt",
            Self::ContextTooLong => "model_input_too_long",
            Self::AuthForbidden1 | Self::AuthForbidden2 => "auth_forbidden",
            Self::ModelBehaviorError => "model_behavior_error",
        }
    }

    pub fn to_upstream_error_class(self) -> openproxy_types::UpstreamErrorClass {
        if self.is_rate_limit() || self.is_credits_exhausted() {
            openproxy_types::UpstreamErrorClass::ResourceExhausted
        } else if self.is_auth_error() {
            openproxy_types::UpstreamErrorClass::PermissionDenied
        } else if self == Self::ContextTooLong
            || self == Self::NonStreamNotSupported
            || self == Self::FirstMessageNotSystemPrompt
        {
            openproxy_types::UpstreamErrorClass::InvalidPayload
        } else {
            openproxy_types::UpstreamErrorClass::Generic
        }
    }
}

fn extract_biz_code(val: &serde_json::Value, max_depth: usize) -> Option<u32> {
    if max_depth == 0 {
        return None;
    }
    // 1. Direct code or errcode at current level (filtering out HTTP status codes < 1000 and JSON-RPC codes)
    for key in ["code", "errcode"] {
        if let Some(num) = val.get(key).and_then(serde_json::Value::as_i64)
            && (1_000..=(u32::MAX as i64)).contains(&num)
        {
            return Some(num as u32);
        }
        if let Some(s) = val.get(key).and_then(serde_json::Value::as_str)
            && let Ok(num) = s.parse::<u32>()
            && num >= 1_000
        {
            return Some(num);
        }
    }
    // 2. Drill down into data, error, response (handling nested JSON-RPC shells and Axios error envelopes)
    for key in ["data", "error", "response"] {
        if let Some(child) = val.get(key)
            && let Some(code) = extract_biz_code(child, max_depth - 1)
        {
            return Some(code);
        }
    }
    None
}

pub fn parse_codebuddy_error_code(body: &str) -> Option<u32> {
    let val: serde_json::Value = serde_json::from_str(body).ok()?;
    extract_biz_code(&val, 6)
}
