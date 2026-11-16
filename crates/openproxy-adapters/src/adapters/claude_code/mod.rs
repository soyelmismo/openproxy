use super::{
    AdapterAuthType, AdapterFormat, Arc, DiscoveredModel, ProviderAdapter,
    ProviderAdapterConfig, ProviderId, Result, UpstreamClient, UpstreamRequest,
};
use crate::client_contract::Snapshot;
use crate::spoofer::claude_code_profile::ClaudeCodeProfile;

pub const CLAUDE_CODE_DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub const CLAUDE_CODE_USAGE_PATH: &str = "/api/oauth/usage";
pub const CLAUDE_CODE_ANTHROPIC_VERSION: &str = "2023-06-01";
pub const CLAUDE_CODE_BETA_HEADER: &str =
    "oauth-2025-04-20,claude-code-20250219,interleaved-thinking-2025-05-14";
pub const CLAUDE_CODE_USER_AGENT: &str = "claude-cli/2.1.293 (external, cli)";

pub mod adapter_impl;
mod betas;
pub mod cloaking;
pub mod signing;
mod tool_names;

pub use adapter_impl::{
    apply_claude_code_billing_version, merge_claude_beta_header, sync_claude_session_id,
};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ClaudeCodeAdapter {
    config: ProviderAdapterConfig,
    /// Snapshot del contrato congelado por petición (ver
    /// [`ProviderAdapter::pin_request_contract`]): headers, body, betas y TTL
    /// de la petición leen la MISMA generación aunque el runtime global
    /// refresque en paralelo. No se persiste: es estado de ejecución, no
    /// configuración.
    #[serde(skip, default)]
    pinned: Option<Arc<Snapshot<ClaudeCodeProfile>>>,
}

impl ClaudeCodeAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("claude-code"),
                name: "Claude Code".into(),
                anonymous_fallback: false,
                rate_limit_scope: "account".into(),
                allow_loopback: false,
                base_url: CLAUDE_CODE_DEFAULT_BASE_URL.into(),
                auth_type: AdapterAuthType::OAuth,
                format: AdapterFormat::Anthropic,
                extra_headers: vec![],
            },
            pinned: None,
        }
    }

    /// El snapshot del contrato congelado para esta petición, si hay uno.
    #[must_use]
    pub fn pinned_contract(&self) -> Option<&Arc<Snapshot<ClaudeCodeProfile>>> {
        self.pinned.as_ref()
    }

    /// Congela un snapshot explícito: headers, body, betas y TTL de la petición
    /// leen esa generación concreta. Lo usa el pipeline cuando ya tiene el
    /// snapshot memoizado, y los tests.
    #[must_use]
    pub fn with_pinned_contract(mut self, pinned: Arc<Snapshot<ClaudeCodeProfile>>) -> Self {
        self.pinned = Some(pinned);
        self
    }

    /// El perfil de esta petición: el snapshot congelado si existe, si no el
    /// activo del runtime. Un único punto para que ningún consumidor se salte
    /// el pin.
    fn request_profile(&self) -> ClaudeCodeProfile {
        self.pinned.as_ref().map_or_else(
            crate::spoofer::claude_code_profile::current_profile,
            |snapshot| snapshot.payload.clone(),
        )
    }

    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        let mut adapter = Self::new();
        adapter.config.base_url = base_url.into();
        adapter
    }

    pub async fn fetch_models_pipeline(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        let models = claude_code_builtin_models();
        let token = api_key.strip_prefix("Bearer ").unwrap_or(api_key).trim();
        if token.is_empty() {
            return Ok(models);
        }

        let base = self.config.base_url.trim_end_matches('/');
        let url = format!("{base}/v1/models?limit=100");
        let mut req = UpstreamRequest::get(&url);

        if token.starts_with("sk-ant-api") {
            if let Ok(val) = http::HeaderValue::from_str(token) {
                req.headers
                    .insert(http::header::HeaderName::from_static("x-api-key"), val);
            }
        } else if let Ok(val) = crate::upstream::bearer_header_value(token) {
            req.headers.insert(http::header::AUTHORIZATION, val);
        }

        req.headers.insert(
            http::header::HeaderName::from_static("anthropic-version"),
            http::HeaderValue::try_from(
                crate::spoofer::claude_code::current_claude_code_anthropic_version(),
            )
            .unwrap_or_else(|_| http::HeaderValue::from_static(CLAUDE_CODE_ANTHROPIC_VERSION)),
        );
        req.headers.insert(
            http::header::HeaderName::from_static("anthropic-beta"),
            http::HeaderValue::from_static("oauth-2025-04-20,claude-code-20250219"),
        );
        req.headers.insert(
            http::header::USER_AGENT,
            http::HeaderValue::try_from(crate::spoofer::claude_code::current_claude_code_ua())
                .unwrap_or_else(|_| http::HeaderValue::from_static(CLAUDE_CODE_USER_AGENT)),
        );

        let cancel = crate::upstream::CancellationToken::new();
        if let Ok(resp) = upstream_client
            .call(req, crate::upstream::TimeoutProfile::Chat, cancel)
            .await
            && resp.status.is_success()
            && let Ok(body_bytes) = resp.collect().await
            && let Ok(json) = serde_json::from_slice::<serde_json::Value>(&body_bytes)
            && let Some(dynamic_models) = parse_claude_models_response(&json)
        {
            return Ok(dynamic_models);
        }

        Ok(models)
    }
}

crate::adapters::derive_default_from_new!(ClaudeCodeAdapter);

pub mod models;
pub use models::{
    claude_code_builtin_models, merge_claude_models, parse_claude_models_response,
};

#[cfg(test)]
mod tests;

