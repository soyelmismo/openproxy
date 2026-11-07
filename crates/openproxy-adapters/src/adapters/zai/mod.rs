//! Z.ai (ZCode / GLM) provider adapter.
//!
//! Connects to ZCode's Anthropic-compatible coding plan endpoint:
//! - Base URL: `https://zcode.z.ai/api/v1/zcode-plan/anthropic`
//! - Chat URL: `https://zcode.z.ai/api/v1/zcode-plan/anthropic/v1/messages`
//! - Quota / Balance (Start Plan): `https://zcode.z.ai/api/v1/zcode-plan/billing/balance?app_version=3.14.0`
//! - Coding Plan Subscriptions: `https://api.z.ai/api/biz/subscription/list`
//! - Coding Plan Usage / Limits: `https://api.z.ai/api/monitor/usage/quota/limit`

pub mod api_key;
pub mod md5;
pub mod models;
pub mod quota;
pub use quota::autoclaw::{AutoclawCheckinResult, perform_autoclaw_checkin};
pub mod routing;
pub mod routing_refresh;

pub use api_key::*;
pub use models::*;
pub use quota::*;
pub use routing::*;
pub use routing_refresh::*;

use super::{
    AdapterAuthType, AdapterFormat, Arc, DiscoveredModel, ModelId, ProviderAdapterConfig,
    ProviderId, Result, TargetFormat, UpstreamClient,
};
use crate::adapters::ProviderAdapter;
use openproxy_types::AccountQuota;
use serde::{Deserialize, Serialize};

pub const ZAI_APP_VERSION: &str = "3.14.0";
pub const ZAI_DEFAULT_BASE_URL: &str = "https://api.z.ai/api/anthropic";
pub const ZAI_BUSINESS_LOGIN_URL: &str = "https://api.z.ai/api/auth/z/login";
pub const ZAI_SUBSCRIPTION_LIST_URL: &str = "https://api.z.ai/api/biz/subscription/list";
pub const ZAI_QUOTA_LIMIT_URL: &str = "https://api.z.ai/api/monitor/usage/quota/limit";
pub const ZAI_ANTHROPIC_VERSION: &str = "2023-06-01";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ZaiAdapter {
    pub(crate) config: ProviderAdapterConfig,
}

impl ZaiAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("zai"),
                name: "Z.ai (ZCode)".into(),
                anonymous_fallback: false,
                rate_limit_scope: "account".into(),
                base_url: ZAI_DEFAULT_BASE_URL.into(),
                auth_type: AdapterAuthType::OAuth,
                format: AdapterFormat::Anthropic,
                extra_headers: vec![],
            },
        }
    }

    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        let mut adapter = Self::new();
        adapter.config.base_url = base_url.into();
        adapter
    }
}

crate::adapters::derive_default_from_new!(ZaiAdapter);

impl ProviderAdapter for ZaiAdapter {
    crate::adapters::impl_adapter_config!();

    fn models_dev_canonical_ids(&self) -> &'static [&'static str] {
        &["zai", "zcode", "z.ai"]
    }

    fn metadata(&self) -> openproxy_types::ProviderMetadata {
        let mut meta = openproxy_types::ProviderMetadata::custom_default();
        meta.built_in = true;
        meta.deletable = false;
        meta.supports_quota = true;
        meta.quota_refresh_supported = true;
        meta.requires_oauth = true;
        meta
    }

    fn build_chat_url(&self, _target_format: TargetFormat, _model: &ModelId) -> String {
        let base = self.config.base_url.trim_end_matches('/');
        if base.ends_with("/messages") {
            base.to_string()
        } else if base.ends_with("/v1") {
            format!("{base}/messages")
        } else {
            format!("{base}/v1/messages")
        }
    }

    fn wrap_request_body(
        &self,
        body: bytes::Bytes,
        target_format: TargetFormat,
        model: &ModelId,
        resolved_target: &openproxy_types::context::ResolvedTarget,
    ) -> std::result::Result<bytes::Bytes, openproxy_types::error::CoreError> {
        let account = resolved_target.target.account_id.map(|id| id.0);
        if target_format == TargetFormat::Anthropic {
            return Ok(body);
        }
        let _ = (target_format, model, account);
        // If the body is being sent as an OpenAI completions request to AutoClaw:
        crate::adapters::traits::patch_json_request_body(body, |obj| {
            if let Some(m) = obj.get("model").and_then(|v| v.as_str()) {
                let m_lower = m.to_ascii_lowercase();
                #[allow(clippy::if_same_then_else)]
                let target_model = if m_lower.contains("flash") {
                    "zai_glm-5.3-flash"
                } else {
                    "zai_glm-5.3-flash"
                };
                obj.insert(
                    "model".to_string(),
                    serde_json::Value::String(target_model.into()),
                );
            }
        })
    }

    fn finalize_request(
        &self,
        body: bytes::Bytes,
        headers: &mut Vec<(String, String)>,
        _target_format: TargetFormat,
        model: &ModelId,
        _resolved_target: &openproxy_types::context::ResolvedTarget,
    ) -> Result<bytes::Bytes> {
        let is_autoclaw = headers.iter().any(|(k, v)| {
            (k.eq_ignore_ascii_case("authorization") || k.eq_ignore_ascii_case("x-authorization"))
                && (v.contains("eyJ") || v.starts_with("Bearer eyJ"))
        });
        if is_autoclaw {
            let m_lower = model.as_str().to_ascii_lowercase();
            let req_model = if m_lower.contains("flash") {
                "zai_glm-5.3-flash"
            } else {
                // Upstream AutoClaw free quota for zaicoding_glm-5.3 is exhausted (810000).
                // AutoClaw maps both zai_glm-5.3-flash and zai_auto into the active GLM model family
                // and accepts requests consuming Reward Points without returning 403 / 810000.
                "zai_glm-5.3-flash"
            };
            headers.push(("X-Request-Model".into(), req_model.into()));
            headers.push(("X-Product".into(), "autoclaw".into()));
            headers.push(("X-Version".into(), "2.0.3".into()));
            headers.push(("X-Tm".into(), "linux".into()));
            headers.push(("X-Channel".into(), "official".into()));
            headers.push(("X-Lang".into(), "zh-CN".into()));
            headers.push(("X-Client-Type".into(), "pc".into()));
            headers.push(("x_trace_id".into(), "autoclaw-desktop".into()));
            let req_id = uuid::Uuid::new_v4().to_string();
            headers.push(("X-Trace-Id".into(), req_id.clone()));
            headers.push(("X-Request-Id".into(), req_id));
            let session_id = uuid::Uuid::new_v4().to_string();
            headers.push(("X-Session-Id".into(), session_id));

            let app_id = "100003";
            let app_key = "6c79a4059ae078650bc7886b627cf9f9";
            let now = openproxy_types::now_unix_secs_str();
            let sign = md5::md5::md5_hex(format!("{app_id}&{now}&{app_key}"));
            headers.push(("X-Auth-Appid".into(), app_id.into()));
            headers.push(("x-auth-appid".into(), app_id.into()));
            headers.push(("X-Auth-TimeStamp".into(), now.clone()));
            headers.push(("x-auth-timestamp".into(), now));
            headers.push(("X-Auth-Sign".into(), sign.clone()));
            headers.push(("x-auth-sign".into(), sign));

            if let Some(pos) = headers
                .iter()
                .position(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            {
                let token = headers[pos].1.clone();
                headers.push(("X-Authorization".into(), token));
            }
            headers.retain(|(k, _)| !k.eq_ignore_ascii_case("authorization"));

            // Fix body for AutoClaw Stainless/OpenAI schema:
            // 1. Convert max_tokens -> max_completion_tokens (AutoClaw returns 406 if max_tokens is present)
            // 2. Strict system message: AutoClaw WAF rejects custom system instructions
            //    (e.g. mentions of "Hermes", "Nous Research", or other agent personas return 406).
            //    We replace the system prompt with the exact required AutoClaw system prompt
            //    and relocate the caller's system instructions into the first user message.
            const REQUIRED_PREFIX: &str =
                "You are AutoClaw. Answer the user directly and concisely.";
            let patched_body = crate::adapters::traits::patch_json_request_body(body, |obj| {
                if let Some(max_tok) = obj.remove("max_tokens") {
                    obj.insert("max_completion_tokens".to_string(), max_tok);
                }
                if let Some(m) = obj.get("model").and_then(|v| v.as_str()) {
                    let m_lower = m.to_ascii_lowercase();
                    #[allow(clippy::if_same_then_else)]
                    let target_model = if m_lower.contains("flash") {
                        "zai_glm-5.3-flash"
                    } else {
                        "zai_glm-5.3-flash"
                    };
                    obj.insert(
                        "model".to_string(),
                        serde_json::Value::String(target_model.into()),
                    );
                }
                if let Some(messages) = obj.get_mut("messages").and_then(|v| v.as_array_mut()) {
                    let mut orig_sys_instructions = Vec::new();

                    for msg in messages.iter_mut() {
                        if msg.get("role").and_then(|r| r.as_str()) == Some("system")
                            && let Some(c) = msg.get("content").and_then(|v| v.as_str())
                        {
                            let trimmed = c.trim();
                            if !trimmed.is_empty()
                                && trimmed != REQUIRED_PREFIX
                                && !trimmed.starts_with(REQUIRED_PREFIX)
                            {
                                orig_sys_instructions.push(trimmed.to_string());
                            }
                        }
                    }

                    // Keep only non-system messages
                    messages.retain(|m| m.get("role").and_then(|r| r.as_str()) != Some("system"));

                    // If caller had custom system instructions, inject them into first user turn
                    if !orig_sys_instructions.is_empty() {
                        let sys_block = format!(
                            "[System Instructions & Context:\n{}\n]",
                            orig_sys_instructions.join("\n\n")
                        );
                        if let Some(first_user) = messages
                            .iter_mut()
                            .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
                        {
                            if let Some(content) = first_user.get_mut("content")
                                && let Some(orig_u) = content.as_str()
                            {
                                *content =
                                    serde_json::Value::String(format!("{sys_block}\n\n{orig_u}"));
                            }
                        } else {
                            messages.insert(
                                0,
                                serde_json::json!({
                                    "role": "user",
                                    "content": sys_block,
                                }),
                            );
                        }
                    }

                    // Prepend the exact system prompt required by AutoClaw upstream
                    messages.insert(
                        0,
                        serde_json::json!({
                            "role": "system",
                            "content": REQUIRED_PREFIX,
                        }),
                    );
                }
            })?;

            // Fix double-Bearer in X-Authorization if present
            for (k, v) in headers.iter_mut() {
                if k.eq_ignore_ascii_case("x-authorization") || k.eq_ignore_ascii_case("x-api-key")
                {
                    while let Some(stripped) = v.strip_prefix("Bearer ") {
                        if v.starts_with("Bearer Bearer ") {
                            *v = stripped.to_string();
                        } else {
                            break;
                        }
                    }
                }
            }

            return Ok(patched_body);
        }
        Ok(body)
    }

    fn build_headers(
        &self,
        api_key: &str,
        _target_format: TargetFormat,
        _model: &ModelId,
    ) -> Vec<(String, String)> {
        let mut headers = Vec::with_capacity(5 + self.config.extra_headers.len());
        let token = api_key.trim();
        if !token.is_empty() {
            headers.push(("x-api-key".into(), token.to_string()));
            headers.push(crate::upstream::bearer_auth_header(token));
            let bearer = if token.starts_with("Bearer ") {
                token.to_string()
            } else {
                format!("Bearer {token}")
            };
            headers.push(("X-Authorization".into(), bearer));
        }
        headers.push(("anthropic-version".into(), ZAI_ANTHROPIC_VERSION.into()));
        headers.push(("Content-Type".into(), "application/json".into()));
        let ua = if api_key.contains("eyJ") {
            "AutoClaw/2.0.3"
        } else {
            "ZCode/1.0.0"
        };
        headers.push(("User-Agent".into(), ua.into()));
        for (k, v) in &self.config.extra_headers {
            headers.push((k.clone(), v.clone()));
        }
        headers
    }

    fn models_url(&self) -> Option<String> {
        None
    }

    fn fetch_models(
        &self,
        _upstream_client: &Arc<UpstreamClient>,
        _api_key: &str,
    ) -> impl std::future::Future<Output = Result<Vec<DiscoveredModel>>> + Send {
        std::future::ready(Ok(zai_builtin_models()))
    }

    async fn fetch_quota(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
        access_token: Option<&str>,
        provider_specific: Option<&str>,
    ) -> Option<Result<AccountQuota>> {
        self.fetch_quota_with_proxy(
            upstream_client,
            api_key,
            access_token,
            provider_specific,
            None,
        )
        .await
    }

    async fn fetch_quota_with_proxy(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
        access_token: Option<&str>,
        provider_specific: Option<&str>,
        proxy_url: Option<&str>,
    ) -> Option<Result<AccountQuota>> {
        Some(
            fetch_zai_quota_unified(
                upstream_client,
                api_key,
                access_token,
                provider_specific,
                proxy_url,
            )
            .await,
        )
    }
}

#[cfg(test)]
mod tests;
