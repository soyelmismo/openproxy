//! Z.ai (ZCode / GLM) provider adapter.
//!
//! Connects to ZCode's Anthropic-compatible coding plan endpoint:
//! - Base URL: `https://zcode.z.ai/api/v1/zcode-plan/anthropic`
//! - Chat URL: `https://zcode.z.ai/api/v1/zcode-plan/anthropic/v1/messages`
//! - Quota / Balance (Start Plan): `https://zcode.z.ai/api/v1/zcode-plan/billing/balance?app_version=3.14.0`
//! - Coding Plan Subscriptions: `https://api.z.ai/api/biz/subscription/list`
//! - Coding Plan Usage / Limits: `https://api.z.ai/api/monitor/usage/quota/limit`

pub mod api_key;
pub mod models;
pub mod quota;

pub use api_key::*;
pub use models::*;
pub use quota::*;

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
    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn config_mut(&mut self) -> Option<&mut ProviderAdapterConfig> {
        Some(&mut self.config)
    }

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
            headers.push(("Authorization".into(), format!("Bearer {token}")));
        }
        headers.push(("anthropic-version".into(), ZAI_ANTHROPIC_VERSION.into()));
        headers.push(("Content-Type".into(), "application/json".into()));
        headers.push(("User-Agent".into(), format!("ZCode/{ZAI_APP_VERSION}")));
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
