use std::sync::Arc;

use super::{
    AccountQuota, OpenCodeAdapter, OpenCodeFlavor, ProviderMetadata, Result, UpstreamClient,
    build_opencode_headers, fetch_opencode_go_quota, fetch_opencode_models,
};
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct OpenCodeGoAdapter(pub OpenCodeAdapter);

impl OpenCodeGoAdapter {
    pub fn new() -> Self {
        Self(OpenCodeAdapter::new(OpenCodeFlavor::Go))
    }
}
crate::adapters::derive_default_from_new!(OpenCodeGoAdapter);

macro_rules! delegate_opencode_adapter {
    ($adapter:ident) => {
        impl crate::adapters::ProviderAdapter for $adapter {
            fn config(&self) -> &crate::adapters::ProviderAdapterConfig {
                self.0.config()
            }
            fn config_mut(&mut self) -> Option<&mut crate::adapters::ProviderAdapterConfig> {
                self.0.config_mut()
            }
            fn is_anonymous_fallback(&self) -> bool {
                self.0.is_anonymous_fallback()
            }
            fn models_dev_canonical_ids(&self) -> &'static [&'static str] {
                self.0.models_dev_canonical_ids()
            }
            fn build_headers(
                &self,
                api_key: &str,
                target_format: openproxy_types::TargetFormat,
                model: &openproxy_types::ModelId,
            ) -> Vec<(String, String)> {
                self.0.build_headers(api_key, target_format, model)
            }
            fn build_chat_url(
                &self,
                target_format: openproxy_types::TargetFormat,
                model: &openproxy_types::ModelId,
            ) -> String {
                self.0.build_chat_url(target_format, model)
            }
            async fn fetch_models(
                &self,
                upstream_client: &std::sync::Arc<crate::upstream::UpstreamClient>,
                api_key: &str,
            ) -> openproxy_types::Result<Vec<openproxy_types::DiscoveredModel>> {
                self.0.fetch_models(upstream_client, api_key).await
            }
            fn metadata(&self) -> openproxy_types::ProviderMetadata {
                self.0.metadata()
            }
            fn fetch_quota_with_proxy(
                &self,
                upstream_client: &std::sync::Arc<crate::upstream::UpstreamClient>,
                api_key: &str,
                access_token: Option<&str>,
                provider_specific: Option<&str>,
                proxy_url: Option<&str>,
            ) -> impl std::future::Future<
                Output = Option<openproxy_types::Result<openproxy_types::AccountQuota>>,
            > + Send {
                self.0.fetch_quota_with_proxy(
                    upstream_client,
                    api_key,
                    access_token,
                    provider_specific,
                    proxy_url,
                )
            }
            fn wrap_request_body(
                &self,
                body: bytes::Bytes,
                target_format: openproxy_types::TargetFormat,
                model: &openproxy_types::ModelId,
                resolved_target: &openproxy_types::context::ResolvedTarget,
            ) -> std::result::Result<bytes::Bytes, openproxy_types::error::CoreError> {
                self.0
                    .wrap_request_body(body, target_format, model, resolved_target)
            }
        }
    };
}

delegate_opencode_adapter!(OpenCodeGoAdapter);

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct OpenCodeZenAdapter(pub OpenCodeAdapter);

impl OpenCodeZenAdapter {
    pub fn new() -> Self {
        Self(OpenCodeAdapter::new(OpenCodeFlavor::Zen))
    }
}
crate::adapters::derive_default_from_new!(OpenCodeZenAdapter);

delegate_opencode_adapter!(OpenCodeZenAdapter);

impl crate::adapters::ProviderAdapter for OpenCodeAdapter {
    fn config(&self) -> &crate::adapters::ProviderAdapterConfig {
        &self.config
    }

    fn config_mut(&mut self) -> Option<&mut crate::adapters::ProviderAdapterConfig> {
        Some(&mut self.config)
    }

    fn is_anonymous_fallback(&self) -> bool {
        true
    }

    fn models_dev_canonical_ids(&self) -> &'static [&'static str] {
        match self.flavor {
            OpenCodeFlavor::Zen => &["opencode"],
            OpenCodeFlavor::Go => &["opencode-go"],
        }
    }

    fn build_headers(
        &self,
        api_key: &str,
        target_format: openproxy_types::TargetFormat,
        _model: &openproxy_types::ModelId,
    ) -> Vec<(String, String)> {
        build_opencode_headers(self, api_key, target_format)
    }

    fn build_chat_url(
        &self,
        target_format: openproxy_types::TargetFormat,
        model: &openproxy_types::ModelId,
    ) -> String {
        let base_url = &self.config.base_url;
        if target_format == openproxy_types::TargetFormat::Gemini {
            format!(
                "{base_url}/models/{}:streamGenerateContent?alt=sse",
                model.as_str()
            )
        } else {
            let eff_format =
                crate::adapters::traits::resolve_target_format(self.config.format, target_format);
            format!(
                "{base_url}{}",
                crate::adapters::traits::target_format_path(eff_format)
            )
        }
    }

    async fn fetch_models(
        &self,
        upstream_client: &std::sync::Arc<crate::upstream::UpstreamClient>,
        api_key: &str,
    ) -> openproxy_types::Result<Vec<openproxy_types::DiscoveredModel>> {
        fetch_opencode_models(self, self.flavor, upstream_client, api_key).await
    }

    fn metadata(&self) -> ProviderMetadata {
        let supports_quota = matches!(self.flavor, OpenCodeFlavor::Go);
        ProviderMetadata {
            built_in: true,
            deletable: false,
            supports_quota,
            quota_refresh_supported: supports_quota,
            requires_oauth: false,
            oauth_refresh_lead_seconds: None,
        }
    }

    async fn fetch_quota(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
        access_token: Option<&str>,
        _provider_specific: Option<&str>,
    ) -> Option<Result<AccountQuota>> {
        self.fetch_quota_with_proxy(
            upstream_client,
            api_key,
            access_token,
            _provider_specific,
            None,
        )
        .await
    }

    async fn fetch_quota_with_proxy(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
        access_token: Option<&str>,
        _provider_specific: Option<&str>,
        proxy_url: Option<&str>,
    ) -> Option<Result<AccountQuota>> {
        if self.flavor != OpenCodeFlavor::Go {
            return None;
        }

        let token = access_token.unwrap_or(api_key).trim();
        if token.is_empty() {
            return Some(Ok(AccountQuota::with_error(
                "opencode-go requires API key for quota",
            )));
        }

        Some(fetch_opencode_go_quota(upstream_client, token, proxy_url).await)
    }

    fn wrap_request_body(
        &self,
        body: bytes::Bytes,
        target_format: openproxy_types::TargetFormat,
        model: &openproxy_types::ModelId,
        resolved_target: &openproxy_types::context::ResolvedTarget,
    ) -> std::result::Result<bytes::Bytes, openproxy_types::error::CoreError> {
        self.wrap_request_body(body, target_format, model, resolved_target)
    }
}
