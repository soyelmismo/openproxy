//! Devin provider adapter.
//!
//! Speaks Devin's native Connect-RPC + protobuf protocol against
//! `https://server.codeium.com`, reverse-engineered from CLIProxyAPI's Go
//! implementation:
//! - Chat: `POST /exa.api_server_pb.ApiServerService/GetChatMessage`
//!   (`application/connect+proto`, Connect envelope, sentry-trace).
//! - Quota: `POST /exa.seat_management_pb.SeatManagementService/GetUserStatus`
//!   (`application/proto`, bare protobuf).
//!
//! The account `api_key` is the Devin session token. It is embedded twice per
//! request: in `Authorization: Basic <token>-<token>` and inside the signed
//! ClientMetadata protobuf field — which is why request formatting runs in
//! two stages (see [`DevinAdapter::format_request`]).

pub mod models;
pub mod protoc;
pub mod quota;
pub mod response;
pub mod signature;
pub mod tools;
pub mod turn;
pub mod wire;

pub use models::*;
pub use protoc::*;
pub use quota::*;
pub use response::*;
pub use tools::*;
pub use turn::*;
pub use wire::*;

use super::{
    AdapterAuthType, AdapterFormat, Arc, DiscoveredModel, ModelId, ProviderAdapterConfig,
    ProviderId, Result, TargetFormat, UpstreamClient,
};
use crate::adapters::ProviderAdapter;
use openproxy_types::{AccountQuota, CoreError, ResultExt};
use serde::{Deserialize, Serialize};

pub const DEVIN_DEFAULT_BASE_URL: &str = "https://server.codeium.com";
pub const DEVIN_CHAT_PATH: &str = "/exa.api_server_pb.ApiServerService/GetChatMessage";
pub const DEVIN_USER_STATUS_PATH: &str =
    "/exa.seat_management_pb.SeatManagementService/GetUserStatus";
pub const DEVIN_CLIENT_NAME: &str = "chisel";
pub const DEVIN_CLIENT_VERSION: &str = "3000.10.21";
pub const DEVIN_LOCALE: &str = "en";
pub const DEVIN_DEFAULT_MAX_TOKENS: u64 = 128_000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DevinAdapter {
    pub(crate) config: ProviderAdapterConfig,
}

impl DevinAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("devin"),
                name: "Devin".into(),
                anonymous_fallback: false,
                rate_limit_scope: "account".into(),
                base_url: DEVIN_DEFAULT_BASE_URL.into(),
                // Devin auth is the Basic `<token>-<token>` pair, built by
                // `build_auth_header` below. Bearer keeps the provider's
                // credential semantics (requires an api_key/session token)
                // without any code path emitting a literal Bearer header.
                auth_type: AdapterAuthType::Bearer,
                format: AdapterFormat::Devin,
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

crate::adapters::derive_default_from_new!(DevinAdapter);

impl ProviderAdapter for DevinAdapter {
    crate::adapters::impl_adapter_config!();

    fn models_dev_canonical_ids(&self) -> &'static [&'static str] {
        &["devin", "codeium"]
    }

    fn metadata(&self) -> openproxy_types::ProviderMetadata {
        openproxy_types::ProviderMetadata {
            built_in: true,
            deletable: false,
            supports_quota: true,
            quota_refresh_supported: true,
            requires_oauth: false,
            oauth_refresh_lead_seconds: None,
        }
    }

    fn build_chat_url(&self, _target_format: TargetFormat, _model: &ModelId) -> String {
        format!(
            "{}{}",
            self.config.base_url.trim_end_matches('/'),
            DEVIN_CHAT_PATH
        )
    }

    fn build_auth_header(&self, api_key: &str) -> Option<(String, String)> {
        let token = api_key.trim();
        (!token.is_empty()).then(|| ("authorization".into(), wire::basic_auth(token)))
    }

    fn build_headers(
        &self,
        api_key: &str,
        _target_format: TargetFormat,
        _model: &ModelId,
    ) -> Vec<(String, String)> {
        // Upstream ships an explicit empty User-Agent (native devin-cli
        // suppresses it); an empty value also blocks openproxy's default
        // `openproxy/<ver>` injection in `build_hyper_request`.
        let mut headers = Vec::with_capacity(6 + self.config.extra_headers.len());
        if let Some(auth) = self.build_auth_header(api_key) {
            headers.push(auth);
        }
        headers.push(("content-type".into(), "application/connect+proto".into()));
        headers.push(("connect-protocol-version".into(), "1".into()));
        headers.push(("accept".into(), "*/*".into()));
        headers.push(("user-agent".into(), String::new()));
        headers.push(("sentry-trace".into(), wire::sentry_trace_header()));
        for (k, v) in &self.config.extra_headers {
            headers.push((k.clone(), v.clone()));
        }
        headers
    }

    /// Stage 1 of a two-stage body build: the signed protobuf payload embeds
    /// ClientMetadata.api_key, but `format_request` never sees credentials, so
    /// it emits a JSON carrier (the OpenAI request with the upstream model,
    /// effective messages and stream flag folded in). `wrap_request_body`
    /// — which receives the ResolvedTarget — turns it into the real Connect
    /// envelope.
    fn format_request(
        &self,
        _target_format: TargetFormat,
        req: &openproxy_types::OpenAIRequest,
        model: &ModelId,
        messages: &[openproxy_types::OpenAIMessage],
        stream: bool,
    ) -> std::result::Result<bytes::Bytes, CoreError> {
        let mut carrier = req.clone();
        carrier.messages = messages.to_vec();
        carrier.model = model.as_str().to_string();
        carrier.stream = stream;
        serde_json::to_vec(&carrier)
            .map(bytes::Bytes::from)
            .ctx_parse("devin: serialize request carrier")
    }

    /// Stage 2: decode the carrier and build the Connect envelope with the
    /// resolved account token. Bodies that are not carriers but already look
    /// like a finished Connect envelope pass through untouched, so a
    /// Devin-aware formatter can short-circuit this stage safely.
    fn wrap_request_body(
        &self,
        body: bytes::Bytes,
        _target_format: TargetFormat,
        _model: &ModelId,
        resolved_target: &openproxy_types::context::ResolvedTarget,
    ) -> std::result::Result<bytes::Bytes, CoreError> {
        let req: openproxy_types::OpenAIRequest = match serde_json::from_slice(&body) {
            Ok(req) => req,
            Err(_) if wire::looks_like_connect_envelope(&body) => return Ok(body),
            Err(e) => {
                return Err(CoreError::Parse(format!(
                    "devin: decode request carrier: {e}"
                )));
            }
        };
        let upstream_model = req.model.clone();
        let device_seed = resolved_target
            .custom_meta
            .as_ref()
            .and_then(|m| m.devin_device_seed.as_deref())
            .unwrap_or("");
        wire::build_devin_request(&req, &resolved_target.api_key, &upstream_model, device_seed)
    }

    fn models_url(&self) -> Option<String> {
        None
    }

    fn fetch_models(
        &self,
        _upstream_client: &Arc<UpstreamClient>,
        _api_key: &str,
    ) -> impl std::future::Future<Output = Result<Vec<DiscoveredModel>>> + Send {
        std::future::ready(Ok(models::devin_builtin_models()))
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
        let token = access_token
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| api_key.trim());
        let device_seed = provider_specific
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
            .and_then(|v| {
                v.get("device_seed")
                    .or_else(|| v.get("deviceSeed"))
                    .and_then(|s| s.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_default();
        Some(
            quota::fetch_devin_quota(
                upstream_client,
                &self.config.base_url,
                token,
                &device_seed,
                proxy_url,
            )
            .await,
        )
    }
}

#[cfg(test)]
pub mod tests;
