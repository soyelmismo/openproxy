/// Defines [`ProviderAdapterEnum`] and every piece of boilerplate:
/// the enum variants, `builtin_adapters()`, the `from_provider_id` jump-table,
/// the per-method inline dispatch `match`, and the `pub use` re-exports for every builtin.
#[macro_export]
macro_rules! define_provider_adapter {
    (
        pub enum ProviderAdapterEnum {
            builtins {
                $( $(#[$b_varmeta:meta])* $b_id:literal => $b_variant:ident($($b_mod:ident)::* , $b_adapter:ident) ),+ $(,)?
            }
            custom {
                $( $(#[$c_varmeta:meta])* $c_variant:ident($($c_mod:ident)::* , $c_adapter:ident) ),+ $(,)?
            }
        }
    ) => {
        #[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
        #[serde(tag = "provider_id", content = "config", rename_all = "kebab-case")]
        pub enum ProviderAdapterEnum {
            $( $(#[$b_varmeta])* $b_variant(Box<$($b_mod)::*::$b_adapter>) ),+,
            $( $(#[$c_varmeta])* $c_variant(Box<$($c_mod)::*::$c_adapter>) ),+
        }

        // Re-export every builtin adapter.
        $( pub use $($b_mod)::*::$b_adapter; )+

        pub fn builtin_adapters() -> Vec<ProviderAdapterEnum> {
            vec![
                $( $(#[$b_varmeta])* ProviderAdapterEnum::$b_variant(Box::default()) ),+
            ]
        }

        impl ProviderAdapterEnum {
            /// Compile-time static O(1) jump table for provider lookup by ID string.
            #[inline]
            pub fn from_provider_id(id: &str) -> Option<Self> {
                match id {
                    $( $(#[$b_varmeta])* $b_id => Some(Self::$b_variant(Box::default())), )+
                    _ => None,
                }
            }
        }

        // Per-method inline dispatch: each match is generated from the same
        // `builtins` + `custom` lists that define the enum, so exhaustiveness
        // is guaranteed by the compiler — no hand-maintained dispatch table.
        impl ProviderAdapterEnum {
            pub fn id(&self) -> &openproxy_types::ProviderId {
                match self {
                    $( Self::$b_variant(inner) => inner.id(), )+
                    $( $(#[$c_varmeta])* Self::$c_variant(inner) => inner.id(), )+
                }
            }
            pub fn config(&self) -> &$crate::adapters::ProviderAdapterConfig {
                match self {
                    $( Self::$b_variant(inner) => inner.config(), )+
                    $( $(#[$c_varmeta])* Self::$c_variant(inner) => inner.config(), )+
                }
            }
            pub fn config_mut(&mut self) -> Option<&mut $crate::adapters::ProviderAdapterConfig> {
                match self {
                    $( Self::$b_variant(inner) => inner.config_mut(), )+
                    $( $(#[$c_varmeta])* Self::$c_variant(inner) => inner.config_mut(), )+
                }
            }
            pub fn append_extra_headers(&mut self, extra: Vec<(String, String)>) {
                if let Some(c) = self.config_mut() {
                    c.extra_headers.extend(extra);
                }
            }
            pub fn metadata(&self) -> openproxy_types::ProviderMetadata {
                match self {
                    $( Self::$b_variant(inner) => inner.metadata(), )+
                    $( $(#[$c_varmeta])* Self::$c_variant(inner) => inner.metadata(), )+
                }
            }
            pub fn is_anonymous_fallback(&self) -> bool {
                match self {
                    $( Self::$b_variant(inner) => inner.is_anonymous_fallback(), )+
                    $( $(#[$c_varmeta])* Self::$c_variant(inner) => inner.is_anonymous_fallback(), )+
                }
            }
            pub fn models_dev_canonical_ids(&self) -> &'static [&'static str] {
                match self {
                    $( Self::$b_variant(inner) => inner.models_dev_canonical_ids(), )+
                    $( $(#[$c_varmeta])* Self::$c_variant(inner) => inner.models_dev_canonical_ids(), )+
                }
            }
            pub fn wrap_request_body(
                &self,
                body: bytes::Bytes,
                target_format: TargetFormat,
                model: &openproxy_types::ModelId,
                resolved_target: &openproxy_types::context::ResolvedTarget,
            ) -> std::result::Result<bytes::Bytes, openproxy_types::error::CoreError> {
                match self {
                    $( Self::$b_variant(inner) => inner.wrap_request_body(body, target_format, model, resolved_target), )+
                    $( $(#[$c_varmeta])* Self::$c_variant(inner) => inner.wrap_request_body(body, target_format, model, resolved_target), )+
                }
            }
            pub fn auth_type(&self) -> $crate::adapters::AdapterAuthType {
                match self {
                    $( Self::$b_variant(inner) => inner.auth_type(), )+
                    $( $(#[$c_varmeta])* Self::$c_variant(inner) => inner.auth_type(), )+
                }
            }
            pub fn format(&self) -> $crate::adapters::AdapterFormat {
                match self {
                    $( Self::$b_variant(inner) => inner.format(), )+
                    $( $(#[$c_varmeta])* Self::$c_variant(inner) => inner.format(), )+
                }
            }
            pub fn build_chat_url(&self, target_format: openproxy_types::TargetFormat, model: &openproxy_types::ModelId) -> String {
                match self {
                    $( Self::$b_variant(inner) => inner.build_chat_url(target_format, model), )+
                    $( $(#[$c_varmeta])* Self::$c_variant(inner) => inner.build_chat_url(target_format, model), )+
                }
            }
            pub fn build_chat_url_for_account(
                &self,
                target_format: openproxy_types::TargetFormat,
                model: &openproxy_types::ModelId,
                account_label: &str,
            ) -> String {
                match self {
                    $( Self::$b_variant(inner) => inner.build_chat_url_for_account(target_format, model, account_label), )+
                    $( $(#[$c_varmeta])* Self::$c_variant(inner) => inner.build_chat_url_for_account(target_format, model, account_label), )+
                }
            }
            pub fn build_transcription_url(&self) -> String {
                match self {
                    $( Self::$b_variant(inner) => inner.build_transcription_url(), )+
                    $( $(#[$c_varmeta])* Self::$c_variant(inner) => inner.build_transcription_url(), )+
                }
            }
            pub fn build_embeddings_url(&self) -> String {
                match self {
                    $( Self::$b_variant(inner) => inner.build_embeddings_url(), )+
                    $( $(#[$c_varmeta])* Self::$c_variant(inner) => inner.build_embeddings_url(), )+
                }
            }
            pub fn format_embedding_request(
                &self,
                req: &openproxy_types::embeddings::EmbeddingRequest,
                upstream_model: &str,
            ) -> std::result::Result<bytes::Bytes, openproxy_types::error::CoreError> {
                match self {
                    $( Self::$b_variant(inner) => inner.format_embedding_request(req, upstream_model), )+
                    $( $(#[$c_varmeta])* Self::$c_variant(inner) => inner.format_embedding_request(req, upstream_model), )+
                }
            }
            pub fn build_image_url(&self) -> String {
                match self {
                    $( Self::$b_variant(inner) => inner.build_image_url(), )+
                    $( $(#[$c_varmeta])* Self::$c_variant(inner) => inner.build_image_url(), )+
                }
            }
            pub fn build_image_edits_url(&self) -> String {
                match self {
                    $( Self::$b_variant(inner) => inner.build_image_edits_url(), )+
                    $( $(#[$c_varmeta])* Self::$c_variant(inner) => inner.build_image_edits_url(), )+
                }
            }
            pub fn build_image_variations_url(&self) -> String {
                match self {
                    $( Self::$b_variant(inner) => inner.build_image_variations_url(), )+
                    $( $(#[$c_varmeta])* Self::$c_variant(inner) => inner.build_image_variations_url(), )+
                }
            }
            pub fn format_image_request(
                &self,
                req: &openproxy_types::images::ImageGenerationRequest,
                upstream_model: &str,
            ) -> std::result::Result<bytes::Bytes, openproxy_types::error::CoreError> {
                match self {
                    $( Self::$b_variant(inner) => inner.format_image_request(req, upstream_model), )+
                    $( $(#[$c_varmeta])* Self::$c_variant(inner) => inner.format_image_request(req, upstream_model), )+
                }
            }
            pub fn build_system_one_url(&self) -> String {
                match self {
                    $( Self::$b_variant(inner) => inner.build_system_one_url(), )+
                    $( $(#[$c_varmeta])* Self::$c_variant(inner) => inner.build_system_one_url(), )+
                }
            }
            pub fn format_system_one_request(
                &self,
                req: &openproxy_types::systemone::SystemOneRequest,
                upstream_model: &str,
            ) -> std::result::Result<bytes::Bytes, openproxy_types::error::CoreError> {
                match self {
                    $( Self::$b_variant(inner) => inner.format_system_one_request(req, upstream_model), )+
                    $( $(#[$c_varmeta])* Self::$c_variant(inner) => inner.format_system_one_request(req, upstream_model), )+
                }
            }
            pub fn build_auth_header(&self, api_key: &str) -> Option<(String, String)> {
                match self {
                    $( Self::$b_variant(inner) => inner.build_auth_header(api_key), )+
                    $( $(#[$c_varmeta])* Self::$c_variant(inner) => inner.build_auth_header(api_key), )+
                }
            }
            pub fn build_headers(
                &self,
                api_key: &str,
                target_format: openproxy_types::TargetFormat,
                model: &openproxy_types::ModelId,
            ) -> Vec<(String, String)> {
                match self {
                    $( Self::$b_variant(inner) => inner.build_headers(api_key, target_format, model), )+
                    $( $(#[$c_varmeta])* Self::$c_variant(inner) => inner.build_headers(api_key, target_format, model), )+
                }
            }
            pub fn models_url(&self) -> Option<String> {
                match self {
                    $( Self::$b_variant(inner) => inner.models_url(), )+
                    $( $(#[$c_varmeta])* Self::$c_variant(inner) => inner.models_url(), )+
                }
            }
            pub fn models_url_for_account(&self, account_label: &str) -> Option<String> {
                match self {
                    $( Self::$b_variant(inner) => inner.models_url_for_account(account_label), )+
                    $( $(#[$c_varmeta])* Self::$c_variant(inner) => inner.models_url_for_account(account_label), )+
                }
            }
            pub async fn fetch_models(
                &self,
                upstream_client: &std::sync::Arc<$crate::upstream::UpstreamClient>,
                api_key: &str,
            ) -> openproxy_types::Result<Vec<openproxy_types::DiscoveredModel>> {
                match self {
                    $( Self::$b_variant(inner) => inner.fetch_models(upstream_client, api_key).await, )+
                    $( $(#[$c_varmeta])* Self::$c_variant(inner) => inner.fetch_models(upstream_client, api_key).await, )+
                }
            }
            pub async fn fetch_models_for_account(
                &self,
                upstream_client: &std::sync::Arc<$crate::upstream::UpstreamClient>,
                api_key: &str,
                account_label: &str,
            ) -> openproxy_types::Result<Vec<openproxy_types::DiscoveredModel>> {
                match self {
                    $( Self::$b_variant(inner) => inner.fetch_models_for_account(upstream_client, api_key, account_label).await, )+
                    $( $(#[$c_varmeta])* Self::$c_variant(inner) => inner.fetch_models_for_account(upstream_client, api_key, account_label).await, )+
                }
            }
            pub async fn fetch_models_with_proxy(
                &self,
                upstream_client: &std::sync::Arc<$crate::upstream::UpstreamClient>,
                api_key: &str,
                proxy_url: Option<&str>,
            ) -> openproxy_types::Result<Vec<openproxy_types::DiscoveredModel>> {
                match self {
                    $( Self::$b_variant(inner) => inner.fetch_models_with_proxy(upstream_client, api_key, proxy_url).await, )+
                    $( $(#[$c_varmeta])* Self::$c_variant(inner) => inner.fetch_models_with_proxy(upstream_client, api_key, proxy_url).await, )+
                }
            }
            pub async fn fetch_models_for_account_with_proxy(
                &self,
                upstream_client: &std::sync::Arc<$crate::upstream::UpstreamClient>,
                api_key: &str,
                account_label: &str,
                proxy_url: Option<&str>,
            ) -> openproxy_types::Result<Vec<openproxy_types::DiscoveredModel>> {
                match self {
                    $( Self::$b_variant(inner) => inner.fetch_models_for_account_with_proxy(upstream_client, api_key, account_label, proxy_url).await, )+
                    $( $(#[$c_varmeta])* Self::$c_variant(inner) => inner.fetch_models_for_account_with_proxy(upstream_client, api_key, account_label, proxy_url).await, )+
                }
            }
            pub fn normalize_openai_request(&self, view: &mut openproxy_types::OpenAIRequestView) {
                match self {
                    $( Self::$b_variant(inner) => inner.normalize_openai_request(view), )+
                    $( $(#[$c_varmeta])* Self::$c_variant(inner) => inner.normalize_openai_request(view), )+
                }
            }
            pub fn format_request(
                &self,
                target_format: openproxy_types::TargetFormat,
                req: &openproxy_types::OpenAIRequest,
                model: &openproxy_types::ModelId,
                messages: &[openproxy_types::OpenAIMessage],
                stream: bool,
            ) -> std::result::Result<bytes::Bytes, openproxy_types::error::CoreError> {
                match self {
                    $( Self::$b_variant(inner) => inner.format_request(target_format, req, model, messages, stream), )+
                    $( $(#[$c_varmeta])* Self::$c_variant(inner) => inner.format_request(target_format, req, model, messages, stream), )+
                }
            }
            pub fn translate_non_streaming_response(
                &self,
                target_format: openproxy_types::TargetFormat,
                response_body: serde_json::Value,
            ) -> std::result::Result<openproxy_types::OpenAIResponse, openproxy_types::error::CoreError> {
                match self {
                    $( Self::$b_variant(inner) => inner.translate_non_streaming_response(target_format, response_body), )+
                    $( $(#[$c_varmeta])* Self::$c_variant(inner) => inner.translate_non_streaming_response(target_format, response_body), )+
                }
            }
            pub async fn fetch_quota(
                &self,
                upstream_client: &std::sync::Arc<$crate::upstream::UpstreamClient>,
                api_key: &str,
                access_token: Option<&str>,
                provider_specific: Option<&str>,
            ) -> Option<openproxy_types::Result<openproxy_types::AccountQuota>> {
                self.fetch_quota_with_proxy(upstream_client, api_key, access_token, provider_specific, None).await
            }
            pub async fn fetch_quota_with_proxy(
                &self,
                upstream_client: &std::sync::Arc<$crate::upstream::UpstreamClient>,
                api_key: &str,
                access_token: Option<&str>,
                provider_specific: Option<&str>,
                proxy_url: Option<&str>,
            ) -> Option<openproxy_types::Result<openproxy_types::AccountQuota>> {
                match self {
                    $( Self::$b_variant(inner) => inner.fetch_quota_with_proxy(upstream_client, api_key, access_token, provider_specific, proxy_url).await, )+
                    $( $(#[$c_varmeta])* Self::$c_variant(inner) => inner.fetch_quota_with_proxy(upstream_client, api_key, access_token, provider_specific, proxy_url).await, )+
                }
            }
        }

        impl $crate::adapters::ProviderAdapter for ProviderAdapterEnum {
            fn config(&self) -> &$crate::adapters::ProviderAdapterConfig {
                self.config()
            }
            fn build_chat_url(&self, target_format: openproxy_types::TargetFormat, model: &openproxy_types::ModelId) -> String {
                self.build_chat_url(target_format, model)
            }
            fn build_chat_url_for_account(
                &self,
                target_format: openproxy_types::TargetFormat,
                model: &openproxy_types::ModelId,
                account_label: &str,
            ) -> String {
                self.build_chat_url_for_account(target_format, model, account_label)
            }
            fn build_transcription_url(&self) -> String {
                self.build_transcription_url()
            }
            fn build_embeddings_url(&self) -> String {
                self.build_embeddings_url()
            }
            fn format_embedding_request(
                &self,
                req: &openproxy_types::embeddings::EmbeddingRequest,
                upstream_model: &str,
            ) -> std::result::Result<bytes::Bytes, openproxy_types::error::CoreError> {
                self.format_embedding_request(req, upstream_model)
            }
            fn build_image_url(&self) -> String {
                self.build_image_url()
            }
            fn build_image_edits_url(&self) -> String {
                self.build_image_edits_url()
            }
            fn build_image_variations_url(&self) -> String {
                self.build_image_variations_url()
            }
            fn format_image_request(
                &self,
                req: &openproxy_types::images::ImageGenerationRequest,
                upstream_model: &str,
            ) -> std::result::Result<bytes::Bytes, openproxy_types::error::CoreError> {
                self.format_image_request(req, upstream_model)
            }
            fn build_auth_header(&self, api_key: &str) -> Option<(String, String)> {
                self.build_auth_header(api_key)
            }
            fn build_headers(
                &self,
                api_key: &str,
                target_format: openproxy_types::TargetFormat,
                model: &openproxy_types::ModelId,
            ) -> Vec<(String, String)> {
                self.build_headers(api_key, target_format, model)
            }
            fn models_url(&self) -> Option<String> {
                self.models_url()
            }
            fn models_url_for_account(&self, account_label: &str) -> Option<String> {
                self.models_url_for_account(account_label)
            }
            fn fetch_models(
                &self,
                upstream_client: &std::sync::Arc<$crate::upstream::UpstreamClient>,
                api_key: &str,
            ) -> impl std::future::Future<Output = openproxy_types::Result<Vec<openproxy_types::DiscoveredModel>>> + Send {
                self.fetch_models(upstream_client, api_key)
            }
            fn fetch_models_for_account(
                &self,
                upstream_client: &std::sync::Arc<$crate::upstream::UpstreamClient>,
                api_key: &str,
                account_label: &str,
            ) -> impl std::future::Future<Output = openproxy_types::Result<Vec<openproxy_types::DiscoveredModel>>> + Send {
                self.fetch_models_for_account(upstream_client, api_key, account_label)
            }
            fn fetch_models_with_proxy(
                &self,
                upstream_client: &std::sync::Arc<$crate::upstream::UpstreamClient>,
                api_key: &str,
                proxy_url: Option<&str>,
            ) -> impl std::future::Future<Output = openproxy_types::Result<Vec<openproxy_types::DiscoveredModel>>> + Send {
                self.fetch_models_with_proxy(upstream_client, api_key, proxy_url)
            }
            fn fetch_models_for_account_with_proxy(
                &self,
                upstream_client: &std::sync::Arc<$crate::upstream::UpstreamClient>,
                api_key: &str,
                account_label: &str,
                proxy_url: Option<&str>,
            ) -> impl std::future::Future<Output = openproxy_types::Result<Vec<openproxy_types::DiscoveredModel>>> + Send {
                self.fetch_models_for_account_with_proxy(upstream_client, api_key, account_label, proxy_url)
            }
            fn fetch_quota(
                &self,
                upstream_client: &std::sync::Arc<$crate::upstream::UpstreamClient>,
                api_key: &str,
                access_token: Option<&str>,
                provider_specific: Option<&str>,
            ) -> impl std::future::Future<Output = Option<openproxy_types::Result<openproxy_types::AccountQuota>>> + Send {
                self.fetch_quota(upstream_client, api_key, access_token, provider_specific)
            }
            fn fetch_quota_with_proxy(
                &self,
                upstream_client: &std::sync::Arc<$crate::upstream::UpstreamClient>,
                api_key: &str,
                access_token: Option<&str>,
                provider_specific: Option<&str>,
                proxy_url: Option<&str>,
            ) -> impl std::future::Future<Output = Option<openproxy_types::Result<openproxy_types::AccountQuota>>> + Send {
                self.fetch_quota_with_proxy(upstream_client, api_key, access_token, provider_specific, proxy_url)
            }
            fn normalize_openai_request(&self, view: &mut openproxy_types::OpenAIRequestView) {
                self.normalize_openai_request(view)
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
            fn format_request(
                &self,
                target_format: openproxy_types::TargetFormat,
                req: &openproxy_types::OpenAIRequest,
                model: &openproxy_types::ModelId,
                messages: &[openproxy_types::OpenAIMessage],
                stream: bool,
            ) -> std::result::Result<bytes::Bytes, openproxy_types::error::CoreError> {
                self.format_request(target_format, req, model, messages, stream)
            }
            fn is_anonymous_fallback(&self) -> bool {
                self.is_anonymous_fallback()
            }
            fn translate_non_streaming_response(
                &self,
                target_format: openproxy_types::TargetFormat,
                response_body: serde_json::Value,
            ) -> std::result::Result<openproxy_types::OpenAIResponse, openproxy_types::error::CoreError> {
                self.translate_non_streaming_response(target_format, response_body)
            }
        }
    };
}

macro_rules! derive_default_from_new {
    ($name:ident) => {
        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
    };
}
pub(crate) use derive_default_from_new;

#[macro_export]
macro_rules! declare_openai_adapter {
    (
        $(#[$meta:meta])*
        $struct_name:ident,
        id: $id:expr,
        name: $name:expr,
        base_url: $base_url:expr
        $(, rate_limit_scope: $rl_scope:expr)?
        $(, anonymous_fallback: $anon:expr)?
        $(, extra_headers: $extra_headers:expr)?
        $(, models_url: $models_url:expr)?
        $(, models_dev_canonical_ids: $canon_ids:expr)?,
        custom_impl: {
            $($custom_items:tt)*
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
        pub struct $struct_name {
            config: $crate::adapters::ProviderAdapterConfig,
        }

        impl $struct_name {
            pub fn new() -> Self {
                let extra_headers = {
                    let val = Vec::new();
                    $(let val = $extra_headers;)?
                    val
                };
                let anonymous_fallback = {
                    let val = false;
                    $(let val = $anon;)?
                    val
                };
                let rate_limit_scope = {
                    let val = "account".to_string();
                    $(let val = $rl_scope.to_string();)?
                    val
                };

                Self {
                    config: $crate::adapters::ProviderAdapterConfig {
                        id: openproxy_types::ProviderId::new($id),
                        name: $name.into(),
                        anonymous_fallback,
                        rate_limit_scope,
                        base_url: $base_url.into(),
                        auth_type: $crate::adapters::AdapterAuthType::Bearer,
                        format: $crate::adapters::AdapterFormat::Openai,
                        extra_headers,
                    },
                }
            }
        }

        $crate::adapters::derive_default_from_new!($struct_name);

        impl $crate::adapters::ProviderAdapter for $struct_name {
            fn config(&self) -> &$crate::adapters::ProviderAdapterConfig {
                &self.config
            }

            $(
                fn models_url(&self) -> Option<String> {
                    Some($models_url.into())
                }
            )?

            $(
                fn models_dev_canonical_ids(&self) -> &'static [&'static str] {
                    $canon_ids
                }
            )?

            $($custom_items)*
        }
    };

    (
        $(#[$meta:meta])*
        $struct_name:ident,
        id: $id:expr,
        name: $name:expr,
        base_url: $base_url:expr
        $(, rate_limit_scope: $rl_scope:expr)?
        $(, anonymous_fallback: $anon:expr)?
        $(, extra_headers: $extra_headers:expr)?
        $(, models_url: $models_url:expr)?
        $(, models_dev_canonical_ids: $canon_ids:expr)?
    ) => {
        $crate::declare_openai_adapter!(
            $(#[$meta])*
            $struct_name,
            id: $id,
            name: $name,
            base_url: $base_url
            $(, rate_limit_scope: $rl_scope)?
            $(, anonymous_fallback: $anon)?
            $(, extra_headers: $extra_headers)?
            $(, models_url: $models_url)?
            $(, models_dev_canonical_ids: $canon_ids)?,
            custom_impl: {}
        );
    };
}
pub use declare_openai_adapter;
