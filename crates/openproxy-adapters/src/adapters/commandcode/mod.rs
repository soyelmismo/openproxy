//! Command Code adapter.
//!
//! Handles reverse-engineered Command Code CLI protocol:
//! - POST `/alpha/generate` upstream endpoint for Command Code Go accounts.
//! - Dynamic `x-command-code-version` header acquired from npm registry with local fallback.
//! - Payload packaging matching Command Code CLI environment and schema.
//! - Live `/provider/v1/models` discovery and `/alpha/billing/*` quota tracking.

pub mod billing;
pub mod payload;

#[cfg(test)]
mod tests;

pub(crate) use billing::*;
pub use payload::*;

use super::{
    Arc, CoreError, DiscoveredModel, ModelId, ProviderAdapter, ProviderAdapterConfig, Result,
    TargetFormat, UpstreamClient,
};
use crate::upstream::{CancellationToken, TimeoutProfile, UpstreamRequest};
use crate::{AdapterAuthType, AdapterFormat};
use openproxy_types::{AccountQuota, ProviderId, ProviderMetadata, ResultExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use crate::spoofer::{
    current_commandcode_ua, current_commandcode_version, set_dynamic_commandcode_extra_header,
    set_dynamic_commandcode_ua, set_dynamic_commandcode_version,
};

pub const DEFAULT_COMMANDCODE_CLI_VERSION: &str = crate::spoofer::DEFAULT_COMMANDCODE_CLI_VERSION;
const NPM_COMMANDCODE_METADATA_URL: &str = "https://registry.npmjs.org/command-code/latest";

/// Returns the current dynamic Command Code CLI version.
/// Respects `OPENPROXY_COMMANDCODE_CLI_VERSION` env var if set.
pub fn get_commandcode_cli_version() -> String {
    current_commandcode_version()
}

/// Updates the dynamic Command Code CLI version.
pub fn set_commandcode_cli_version(version: String) {
    set_dynamic_commandcode_version(version);
}

/// Resolves the canonical base URL for Command Code API, honoring `OPENPROXY_COMMANDCODE_BASE_URL`.
pub fn commandcode_base_url() -> String {
    std::env::var("OPENPROXY_COMMANDCODE_BASE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "https://api.commandcode.ai".to_string())
}

/// Asynchronously queries npm registry for the latest `command-code` CLI version.
pub async fn refresh_commandcode_cli_version(upstream_client: &Arc<UpstreamClient>) {
    let req = UpstreamRequest::get(NPM_COMMANDCODE_METADATA_URL);
    let cancel = CancellationToken::new();
    let Ok(resp) = upstream_client
        .call(req, TimeoutProfile::Quota, cancel)
        .await
    else {
        return;
    };
    if !resp.status.is_success() {
        return;
    }
    let Ok(body) = resp.collect().await else {
        return;
    };
    if let Ok(v) = serde_json::from_slice::<Value>(&body)
        && let Some(ver) = v.get("version").and_then(Value::as_str)
    {
        set_commandcode_cli_version(ver.to_string());
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandCodeGoAdapter {
    config: ProviderAdapterConfig,
}

impl CommandCodeGoAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("commandcodego"),
                name: "Command Code Go".into(),
                base_url: "https://api.commandcode.ai".into(),
                auth_type: AdapterAuthType::Bearer,
                format: AdapterFormat::CommandCodeGo,
                extra_headers: vec![],
                anonymous_fallback: false,
                rate_limit_scope: "account".into(),
            },
        }
    }
}

crate::adapters::derive_default_from_new!(CommandCodeGoAdapter);

impl ProviderAdapter for CommandCodeGoAdapter {
    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn metadata(&self) -> ProviderMetadata {
        ProviderMetadata {
            built_in: true,
            deletable: false,
            supports_quota: true,
            quota_refresh_supported: true,
            requires_oauth: false,
            oauth_refresh_lead_seconds: None,
        }
    }

    fn build_chat_url(&self, _target_format: TargetFormat, _model: &ModelId) -> String {
        if let Ok(url) = std::env::var("OPENPROXY_COMMANDCODE_CHAT_URL")
            && !url.trim().is_empty()
        {
            return url.trim().to_string();
        }
        let base = commandcode_base_url();
        format!("{base}/alpha/generate")
    }

    fn build_headers(
        &self,
        api_key: &str,
        _target_format: TargetFormat,
        _model: &ModelId,
    ) -> Vec<(String, String)> {
        use crate::spoofer::{ClientSpoofer, CommandCodeSpoofer};
        let mut headers = CommandCodeSpoofer.headers();
        if let Some((name, value)) = self.build_auth_header(api_key) {
            headers.push((name, value));
        }
        for (k, v) in &self.config().extra_headers {
            headers.push((k.clone(), v.clone()));
        }
        headers
    }

    fn models_url(&self) -> Option<String> {
        let base = commandcode_base_url();
        Some(format!("{base}/provider/v1/models"))
    }

    fn wrap_request_body(
        &self,
        body: bytes::Bytes,
        _target_format: TargetFormat,
        model: &ModelId,
        _resolved_target: &openproxy_types::context::ResolvedTarget,
    ) -> Result<bytes::Bytes> {
        let Ok(mut val) = serde_json::from_slice::<Value>(&body) else {
            return Ok(body);
        };

        // If body already has "config" and "params", pass it through
        if val.get("config").is_some() && val.get("params").is_some() {
            return Ok(body);
        }

        let upstream_model = model.as_str();
        let cc_envelope = transform_openai_to_commandcode(&mut val, upstream_model);
        serde_json::to_vec(&cc_envelope)
            .map(bytes::Bytes::from)
            .map_err(|e| CoreError::Parse(format!("serialize commandcode request: {e}")))
    }

    async fn fetch_models(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        _api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        let url = self
            .models_url()
            .ok_or_else(|| CoreError::Internal("missing models_url".into()))?;

        // Opportunistically trigger a background refresh of the CLI version from npm registry
        let client_clone = Arc::clone(upstream_client);
        tokio::spawn(async move {
            refresh_commandcode_cli_version(&client_clone).await;
        });

        let req = UpstreamRequest::get(&url);
        let cancel = CancellationToken::new();
        let resp = upstream_client
            .call(req, TimeoutProfile::ModelDiscovery, cancel)
            .await
            .ctx_upstream("commandcode /models")?;

        if !resp.status.is_success() {
            return Err(CoreError::UpstreamConnection(format!(
                "commandcode /models returned status {}",
                resp.status
            )));
        }

        let body = resp.collect().await.ctx_upstream("read /models body")?;

        let val: Value = serde_json::from_slice(&body)
            .map_err(|e| CoreError::Parse(format!("commandcode /models parse: {e}")))?;

        let data = val
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(|| CoreError::Parse("expected 'data' array in /models".into()))?;

        let discovered = data
            .iter()
            .filter_map(|item| {
                let id = item.get("id").and_then(Value::as_str)?;
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .map(ToString::to_string);
                let ctx = item.get("context_length").and_then(Value::as_i64);
                Some(DiscoveredModel {
                    model_id: ModelId::new(id),
                    display_name: name,
                    target_format: TargetFormat::CommandCodeGo,
                    context_length: ctx,
                    max_output_tokens: None,
                    input_modalities: Some(vec!["text".into(), "image".into()].into()),
                    output_modalities: Some(vec!["text".into()].into()),
                    model_type: Some("chat".into()),
                    family: None,
                    capabilities: None,
                })
            })
            .collect();

        Ok(discovered)
    }

    async fn fetch_quota(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
        access_token: Option<&str>,
        _provider_specific: Option<&str>,
    ) -> Option<Result<AccountQuota>> {
        let token = access_token.unwrap_or(api_key);
        if token.is_empty() {
            return Some(Ok(AccountQuota {
                session_used: None,
                session_limit: None,
                session_reset_at: None,
                weekly_used: None,
                weekly_limit: None,
                weekly_reset_at: None,
                plan_name: None,
                last_fetched_at: openproxy_types::now_unix_secs_str(),
                fetch_error: Some("commandcode requires token for quota".into()),
                model_details: None,
            }));
        }

        Some(fetch_commandcode_quota(upstream_client, token).await)
    }
}

pub fn apply_commandcode_cli_headers(req: &mut UpstreamRequest, token: &str) {
    use crate::spoofer::{ClientSpoofer, CommandCodeSpoofer};
    let auth_header = format!("Bearer {token}");
    if let (Ok(name), Ok(val)) = (
        http::HeaderName::from_bytes(b"authorization"),
        http::HeaderValue::from_str(&auth_header),
    ) {
        req.headers.insert(name, val);
    }
    CommandCodeSpoofer.apply_to_request(req);
}
