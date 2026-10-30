use super::{
    AdapterAuthType, AdapterFormat, Arc, DiscoveredModel, ModelId, ProviderAdapter,
    ProviderAdapterConfig, ProviderId, Result, TargetFormat, UpstreamClient, UpstreamRequest,
};
use crate::client_contract::Snapshot;
use crate::spoofer::claude_code_profile::ClaudeCodeProfile;

pub const CLAUDE_CODE_DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub const CLAUDE_CODE_USAGE_PATH: &str = "/api/oauth/usage";
pub const CLAUDE_CODE_ANTHROPIC_VERSION: &str = "2023-06-01";
pub const CLAUDE_CODE_BETA_HEADER: &str =
    "oauth-2025-04-20,claude-code-20250219,interleaved-thinking-2025-05-14";
pub const CLAUDE_CODE_USER_AGENT: &str = "claude-cli/2.1.293 (external, cli)";

mod adapter_impl;
mod betas;
pub mod cloaking;
pub mod signing;
mod tool_names;

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

pub fn claude_code_builtin_models() -> Vec<DiscoveredModel> {
    let models = [
        (
            "claude-sonnet-5-5",
            "Claude Sonnet 5.5",
            1_000_000,
            128_000,
            true,
        ),
        (
            "claude-opus-5-5",
            "Claude Opus 5.5",
            1_000_000,
            128_000,
            true,
        ),
        (
            "claude-fable-5-1",
            "Claude Fable 5.1",
            1_000_000,
            128_000,
            true,
        ),
        ("claude-opus-5", "Claude Opus 5", 1_000_000, 128_000, true),
        (
            "claude-sonnet-5",
            "Claude Sonnet 5",
            1_000_000,
            128_000,
            true,
        ),
        ("claude-fable-5", "Claude Fable 5", 1_000_000, 128_000, true),
        (
            "claude-opus-4-8",
            "Claude Opus 4.8",
            1_000_000,
            128_000,
            true,
        ),
        (
            "claude-opus-4-7",
            "Claude Opus 4.7",
            1_000_000,
            128_000,
            true,
        ),
        (
            "claude-sonnet-4-6",
            "Claude Sonnet 4.6",
            1_000_000,
            128_000,
            true,
        ),
        (
            "claude-opus-4-6",
            "Claude Opus 4.6",
            1_000_000,
            128_000,
            true,
        ),
        (
            "claude-opus-4-5-20251101",
            "Claude Opus 4.5",
            200_000,
            64_000,
            true,
        ),
        (
            "claude-haiku-4-5-20251001",
            "Claude Haiku 4.5",
            200_000,
            64_000,
            false,
        ),
        (
            "claude-sonnet-4-5-20250929",
            "Claude Sonnet 4.5",
            1_000_000,
            64_000,
            false,
        ),
    ];

    models
        .into_iter()
        .map(|(id, name, ctx, max_out, thinking)| {
            claude_discovered_model(id, name, ctx, max_out, thinking)
        })
        .collect()
}

/// Builds a [`DiscoveredModel`] in the canonical Claude shape: Anthropic
/// target format, chat type, text+image+pdf input modalities and the shared
/// capability set with `reasoning`/`thinking` gated by the model generation.
fn claude_discovered_model(
    id: &str,
    name: &str,
    ctx: i64,
    max_out: i64,
    reasoning: bool,
) -> DiscoveredModel {
    let caps = openproxy_types::ModelCapabilities {
        vision: Some(true),
        tool_calling: Some(true),
        reasoning: Some(reasoning),
        thinking: Some(reasoning),
        streaming: Some(true),
        ..Default::default()
    };
    DiscoveredModel {
        model_id: ModelId::new(id),
        display_name: Some(name.to_string()),
        target_format: TargetFormat::Anthropic,
        context_length: Some(ctx),
        max_output_tokens: Some(max_out),
        input_modalities: Some(
            vec!["text".to_string(), "image".to_string(), "pdf".to_string()].into_boxed_slice(),
        ),
        output_modalities: Some(vec!["text".to_string()].into_boxed_slice()),
        model_type: Some("chat".to_string()),
        family: Some("claude".to_string()),
        capabilities: Some(caps),
    }
}

pub fn parse_claude_models_response(body: &serde_json::Value) -> Option<Vec<DiscoveredModel>> {
    let items = body.get("data")?.as_array()?;
    let mut discovered = Vec::with_capacity(items.len());

    for item in items {
        let id = item.get("id").and_then(|v| v.as_str())?;
        let name = item
            .get("display_name")
            .and_then(|v| v.as_str())
            .unwrap_or(id);
        let ctx = item
            .get("max_input_tokens")
            .and_then(|v| v.as_i64())
            .unwrap_or(200_000);
        let max_out = item
            .get("max_tokens")
            .and_then(|v| v.as_i64())
            .unwrap_or(8_192);

        let reasoning = item
            .get("capabilities")
            .and_then(|c| c.get("effort"))
            .and_then(|e| e.get("supported"))
            .and_then(|v| v.as_bool())
            .unwrap_or_else(|| {
                id.contains("thinking")
                    || id.contains("sonnet")
                    || id.contains("opus")
                    || id.contains("fable")
            });

        discovered.push(claude_discovered_model(id, name, ctx, max_out, reasoning));
    }

    if discovered.is_empty() {
        None
    } else {
        Some(discovered)
    }
}

pub fn merge_claude_models(base: &mut Vec<DiscoveredModel>, dynamic: Vec<DiscoveredModel>) {
    for dm in dynamic {
        if let Some(existing) = base.iter_mut().find(|m| m.model_id == dm.model_id) {
            existing.display_name = dm.display_name;
            existing.context_length = dm.context_length;
            existing.max_output_tokens = dm.max_output_tokens;
            existing.capabilities = dm.capabilities;
        } else {
            base.push(dm);
        }
    }
}

#[cfg(test)]
mod tests;
