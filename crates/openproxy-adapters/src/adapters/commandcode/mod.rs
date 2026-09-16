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
pub(crate) use payload::*;

use super::{
    Arc, CoreError, DiscoveredModel, ModelId, ProviderAdapter, ProviderAdapterConfig, Result,
    TargetFormat, UpstreamClient,
};
use crate::upstream::{CancellationToken, TimeoutProfile, UpstreamRequest};
use crate::{AdapterAuthType, AdapterFormat};
use openproxy_types::{AccountQuota, ProviderId, ProviderMetadata, ResultExt};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::OnceLock;

pub const DEFAULT_COMMANDCODE_CLI_VERSION: &str = "1.54.0";
const NPM_COMMANDCODE_METADATA_URL: &str = "https://registry.npmjs.org/command-code/latest";

static DYNAMIC_CLI_VERSION: OnceLock<RwLock<String>> = OnceLock::new();

fn version_lock() -> &'static RwLock<String> {
    DYNAMIC_CLI_VERSION.get_or_init(|| RwLock::new(DEFAULT_COMMANDCODE_CLI_VERSION.to_string()))
}

/// Returns the current dynamic Command Code CLI version.
/// Respects `OPENPROXY_COMMANDCODE_CLI_VERSION` env var if set.
pub fn get_commandcode_cli_version() -> String {
    if let Ok(env_ver) = std::env::var("OPENPROXY_COMMANDCODE_CLI_VERSION")
        && !env_ver.trim().is_empty()
    {
        return env_ver.trim().to_string();
    }
    version_lock().read().clone()
}

/// Updates the dynamic Command Code CLI version.
pub fn set_commandcode_cli_version(version: String) {
    let trimmed = version.trim();
    if !trimmed.is_empty() {
        *version_lock().write() = trimmed.to_string();
    }
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
                extra_headers: vec![
                    ("x-cli-environment".into(), "production".into()),
                    ("x-project-slug".into(), "project".into()),
                    ("x-taste-learning".into(), "true".into()),
                    ("User-Agent".into(), "cli".into()),
                ],
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
        format!("{}/alpha/generate", self.config().base_url)
    }

    fn build_headers(
        &self,
        api_key: &str,
        _target_format: TargetFormat,
        _model: &ModelId,
    ) -> Vec<(String, String)> {
        let mut headers = Vec::with_capacity(8 + self.config().extra_headers.len());
        if let Some((name, value)) = self.build_auth_header(api_key) {
            headers.push((name, value));
        }
        headers.push(("Content-Type".into(), "application/json".into()));
        headers.push((
            "x-command-code-version".into(),
            get_commandcode_cli_version(),
        ));
        headers.push(("user-agent".into(), "cli".into()));
        headers.push(("x-cli-environment".into(), "production".into()));
        headers.push(("x-project-slug".into(), "project".into()));
        headers.push(("x-taste-learning".into(), "true".into()));
        for (k, v) in &self.config().extra_headers {
            headers.push((k.clone(), v.clone()));
        }
        headers
    }

    fn models_url(&self) -> Option<String> {
        Some(format!("{}/provider/v1/models", self.config().base_url))
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
    let auth_header = format!("Bearer {token}");
    if let (Ok(name), Ok(val)) = (
        http::HeaderName::from_bytes(b"authorization"),
        http::HeaderValue::from_str(&auth_header),
    ) {
        req.headers.insert(name, val);
    }
    if let (Ok(name), Ok(val)) = (
        http::HeaderName::from_bytes(b"x-command-code-version"),
        http::HeaderValue::from_str(&get_commandcode_cli_version()),
    ) {
        req.headers.insert(name, val);
    }
    if let Ok(name) = http::HeaderName::from_bytes(b"x-cli-environment") {
        req.headers
            .insert(name, http::HeaderValue::from_static("production"));
    }
    if let Ok(name) = http::HeaderName::from_bytes(b"x-project-slug") {
        req.headers
            .insert(name, http::HeaderValue::from_static("project"));
    }
    if let Ok(name) = http::HeaderName::from_bytes(b"x-taste-learning") {
        req.headers
            .insert(name, http::HeaderValue::from_static("true"));
    }
    if let Ok(name) = http::HeaderName::from_bytes(b"user-agent") {
        req.headers
            .insert(name, http::HeaderValue::from_static("cli"));
    }
}
