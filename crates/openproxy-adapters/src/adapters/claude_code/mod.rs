use super::{
    AdapterAuthType, AdapterFormat, Arc, DiscoveredModel, ModelId, ProviderAdapter,
    ProviderAdapterConfig, ProviderId, Result, TargetFormat, UpstreamClient, UpstreamRequest,
};
use crate::client_contract::Snapshot;
use crate::spoofer::claude_code_profile::ClaudeCodeProfile;
use openproxy_types::{AccountQuota, CoreError, ModelQuotaDetail, ProviderMetadata};

pub const CLAUDE_CODE_DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub const CLAUDE_CODE_USAGE_PATH: &str = "/api/oauth/usage";
pub const CLAUDE_CODE_ANTHROPIC_VERSION: &str = "2023-06-01";
pub const CLAUDE_CODE_BETA_HEADER: &str =
    "oauth-2025-04-20,claude-code-20250219,interleaved-thinking-2025-05-14";
pub const CLAUDE_CODE_USER_AGENT: &str = "claude-cli/2.1.293 (external, cli)";

pub mod cloaking;
pub mod signing;

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
        } else if let Ok(val) = http::HeaderValue::from_str(&format!("Bearer {token}")) {
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
            let caps = openproxy_types::ModelCapabilities {
                vision: Some(true),
                tool_calling: Some(true),
                reasoning: Some(thinking),
                thinking: Some(thinking),
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
                    vec!["text".to_string(), "image".to_string()].into_boxed_slice(),
                ),
                output_modalities: Some(vec!["text".to_string()].into_boxed_slice()),
                model_type: Some("chat".to_string()),
                family: Some("claude".to_string()),
                capabilities: Some(caps),
            }
        })
        .collect()
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

        let caps = openproxy_types::ModelCapabilities {
            vision: Some(true),
            tool_calling: Some(true),
            reasoning: Some(reasoning),
            thinking: Some(reasoning),
            streaming: Some(true),
            ..Default::default()
        };

        discovered.push(DiscoveredModel {
            model_id: ModelId::new(id),
            display_name: Some(name.to_string()),
            target_format: TargetFormat::Anthropic,
            context_length: Some(ctx),
            max_output_tokens: Some(max_out),
            input_modalities: Some(
                vec!["text".to_string(), "image".to_string()].into_boxed_slice(),
            ),
            output_modalities: Some(vec!["text".to_string()].into_boxed_slice()),
            model_type: Some("chat".to_string()),
            family: Some("claude".to_string()),
            capabilities: Some(caps),
        });
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

impl ProviderAdapter for ClaudeCodeAdapter {
    crate::adapters::impl_adapter_config!();

    fn pin_request_contract(&mut self) {
        // Congela el snapshot activo (LKG en disco o semilla medida): headers,
        // body, betas y TTL de la petición leen la MISMA generación aunque el
        // runtime global refresque en paralelo. Sin runtime disponible el
        // adapter sigue funcionando con su identidad configurada: el contrato
        // es opt-in por petición.
        self.pinned = crate::spoofer::claude_code_profile::ensure_loaded().ok();
    }

    fn models_dev_canonical_ids(&self) -> &'static [&'static str] {
        &["claude-code", "claude"]
    }

    fn metadata(&self) -> ProviderMetadata {
        ProviderMetadata {
            built_in: true,
            deletable: false,
            supports_quota: true,
            quota_refresh_supported: true,
            requires_oauth: true,
            oauth_refresh_lead_seconds: Some(300),
        }
    }

    fn build_chat_url(&self, _target_format: TargetFormat, _model: &ModelId) -> String {
        let base = self.config.base_url.trim_end_matches('/');
        let url = if base.ends_with("/messages") {
            base.to_string()
        } else if base.ends_with("/v1") {
            format!("{base}/messages")
        } else {
            format!("{base}/v1/messages")
        };
        if url.contains('?') {
            if !url.contains("beta=true") {
                format!("{url}&beta=true")
            } else {
                url
            }
        } else {
            format!("{url}?beta=true")
        }
    }

    fn build_headers(
        &self,
        api_key: &str,
        _target_format: TargetFormat,
        model: &ModelId,
    ) -> Vec<(String, String)> {
        let auth = if !api_key.trim().is_empty() {
            Some(("Authorization".into(), format!("Bearer {}", api_key.trim())))
        } else {
            None
        };
        let spoofer = crate::spoofer::ClaudeCodeSpoofer::new()
            .with_model(model.as_str())
            .with_oauth(true);
        // Los headers del spoofer salen del perfil congelado de esta petición:
        // UA, betas y versiones stainless son de la MISMA generación que el
        // body, aunque el runtime global refresque en paralelo.
        let headers = spoofer.headers_for_profile(&self.request_profile());
        crate::adapters::traits::build_spoofer_headers_from(
            auth,
            headers,
            &self.config.extra_headers,
        )
    }

    fn wrap_request_body(
        &self,
        body: bytes::Bytes,
        _target_format: TargetFormat,
        model: &ModelId,
        resolved_target: &openproxy_types::context::ResolvedTarget,
    ) -> Result<bytes::Bytes> {
        let mut json = serde_json::from_slice::<serde_json::Value>(&body)
            .map_err(|e| CoreError::Parse(format!("failed to parse claude-code request: {e}")))?;

        let account_uuid = resolved_target
            .custom_meta
            .as_ref()
            .and_then(|m| m.claude_account_uuid.clone())
            .or_else(|| {
                resolved_target
                    .custom_meta
                    .as_ref()
                    .and_then(|m| m.claude_metadata.as_deref())
                    .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
                    .and_then(|v| {
                        v.get("account_uuid")
                            .or_else(|| v.get("accountUuid"))
                            .and_then(|s| s.as_str())
                            .map(ToString::to_string)
                    })
            })
            .unwrap_or_else(|| "53098eb0-6bbd-4c76-9f9c-09bc956164bc".to_string());

        let device_id = {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(account_uuid.as_bytes());
            hasher.update(b"openproxy-claude-device");
            let hash = hasher.finalize();
            hash.iter().fold(String::with_capacity(64), |mut s, b| {
                use std::fmt::Write;
                let _ = write!(s, "{b:02x}");
                s
            })
        };

        let session_id = json
            .get("metadata")
            .and_then(|m| m.get("user_id"))
            .and_then(|u| u.as_str())
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            .and_then(|obj| {
                obj.get("session_id")
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string)
            })
            .unwrap_or_else(|| {
                let first_user_text = cloaking::extract_first_user_message_text(&json);
                if !first_user_text.is_empty() {
                    openproxy_types::format_as_v4_uuid(&format!("{account_uuid}:{first_user_text}"))
                } else {
                    uuid::Uuid::new_v4().to_string()
                }
            });

        cloaking::cloak_claude_request_body_from_profile(
            &mut json,
            model.as_str(),
            &account_uuid,
            &session_id,
            &device_id,
            &self.request_profile(),
        );

        let mut body_bytes = serde_json::to_vec(&json).map_err(|e| {
            CoreError::Parse(format!("failed to serialize claude-code request: {e}"))
        })?;

        if let Err(e) = signing::sign_anthropic_messages_body(&mut body_bytes) {
            tracing::warn!(error = %e, "claude-code: cch signing failed");
        }

        Ok(bytes::Bytes::from(body_bytes))
    }

    fn finalize_request(
        &self,
        body: bytes::Bytes,
        headers: &mut Vec<(String, String)>,
        _target_format: TargetFormat,
        model: &ModelId,
        _resolved_target: &openproxy_types::context::ResolvedTarget,
    ) -> Result<bytes::Bytes> {
        let mut json = serde_json::from_slice::<serde_json::Value>(&body)
            .map_err(|e| CoreError::Parse(format!("failed to parse claude-code request: {e}")))?;

        // Read CLI, SDK and runtime from the pinned profile snapshot, with
        // manual overrides. `request_profile()` returns the frozen generation
        // when the pipeline pinned one, so billing, beta and identity headers
        // cannot disagree with the body of this same request.
        let identity =
            crate::spoofer::claude_code::current_claude_code_identity_from(&self.request_profile());
        let user_agent = identity.0;
        let version = user_agent
            .strip_prefix("claude-cli/")
            .and_then(|rest| rest.split(' ').next())
            .filter(|found| !found.is_empty())
            .map_or(identity.1, ToString::to_string);
        let package_version = identity.2;
        let runtime_version = identity.3;

        // Align billing with the final User-Agent, even across a profile reload.
        apply_claude_code_billing_version(&mut json, &version);

        // The pipeline already resolved the canonical session and applied it as
        // the X-Claude-Code-Session-Id header. Mirror that value into the body
        // metadata so the signed payload and the routing header describe the
        // same session.
        sync_claude_session_id(&mut json, headers);

        // Regenerate the Anthropic-Beta capability set from the finalized body
        // (post strip) so body-derived capabilities stay coherent with the bytes
        // actually sent, while preserving any betas the downstream caller
        // explicitly requested.
        // These capabilities belong to the payload, not the model name. Drop
        // stale defaults before regenerating them after parameter stripping.
        for (_, value) in headers
            .iter_mut()
            .filter(|(key, _)| key.eq_ignore_ascii_case("anthropic-beta"))
        {
            *value = value
                .split(',')
                .map(str::trim)
                .filter(|beta| {
                    ![
                        "server-side-fallback-2026-06-01",
                        "fallback-credit-2026-06-01",
                        cloaking::BETA_THINKING_DISPLAY_UPDATES,
                        cloaking::BETA_REDACT_THINKING,
                        cloaking::BETA_EXTENDED_CACHE_TTL,
                        cloaking::BETA_STRUCTURED_OUTPUTS,
                        cloaking::BETA_EFFORT,
                    ]
                    .iter()
                    .any(|conditional| beta.eq_ignore_ascii_case(conditional))
                })
                .collect::<Vec<_>>()
                .join(",");
        }
        let body_betas = cloaking::build_claude_code_betas_for_body_from_profile(
            model.as_str(),
            &json,
            true,
            &self.request_profile(),
        );
        merge_claude_beta_header(headers, &body_betas);

        // Pin the identity headers to the same snapshot as the billing version.
        // Overrides still win: a value explicitly configured through
        // `set_dynamic_claude_code_*`/env is what `current_*` already returned,
        // and an incoming header is only replaced when it disagrees with that
        // snapshot.
        set_header_if_present(headers, "user-agent", &user_agent);
        set_header_if_present(headers, "x-stainless-package-version", &package_version);
        set_header_if_present(headers, "x-stainless-runtime-version", &runtime_version);

        let mut body_bytes = serde_json::to_vec(&json).map_err(|e| {
            CoreError::Parse(format!("failed to serialize claude-code request: {e}"))
        })?;

        // Sign the CCH over the exact finalized bytes. This runs after
        // formatting, wrapping, parameter stripping and header propagation, so
        // the checksum describes the payload that is actually dispatched.
        // Signing failures are surfaced, never ignored.
        signing::sign_anthropic_messages_body(&mut body_bytes)?;

        Ok(bytes::Bytes::from(body_bytes))
    }

    fn models_url(&self) -> Option<String> {
        let base = self.config.base_url.trim_end_matches('/');
        Some(format!("{base}/v1/models?limit=100"))
    }

    async fn fetch_models(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        self.fetch_models_pipeline(upstream_client, api_key).await
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
        _provider_specific: Option<&str>,
        proxy_url: Option<&str>,
    ) -> Option<Result<AccountQuota>> {
        let effective_token = access_token
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(api_key)
            .trim();

        if effective_token.is_empty() {
            return Some(Err(CoreError::Validation(
                "claude-code: missing access token for quota fetch".into(),
            )));
        }

        Some(
            fetch_claude_code_quota(
                upstream_client,
                &self.config.base_url,
                effective_token,
                proxy_url,
            )
            .await,
        )
    }
}

pub(crate) async fn fetch_claude_code_quota(
    upstream: &Arc<UpstreamClient>,
    base_url: &str,
    access_token: &str,
    proxy_url: Option<&str>,
) -> Result<AccountQuota> {
    let base = base_url.trim_end_matches('/');
    let usage_url = format!("{base}{CLAUDE_CODE_USAGE_PATH}");

    let mut req = UpstreamRequest::get(&usage_url);
    req.headers.insert(
        http::header::AUTHORIZATION,
        http::HeaderValue::from_str(&format!("Bearer {access_token}"))
            .map_err(|e| CoreError::Validation(format!("invalid authorization header: {e}")))?,
    );
    req.headers.insert(
        http::header::HeaderName::from_static("anthropic-beta"),
        http::HeaderValue::from_static("oauth-2025-04-20"),
    );
    req.headers.insert(
        http::header::USER_AGENT,
        http::HeaderValue::from_static(CLAUDE_CODE_USER_AGENT),
    );

    if let Some(proxy) = proxy_url {
        req.proxy = Some(proxy.to_string());
    }

    let cancel = crate::upstream::CancellationToken::new();
    let resp = upstream
        .call(req, crate::upstream::TimeoutProfile::Quota, cancel)
        .await
        .map_err(|e| CoreError::UpstreamConnection(format!("claude-code quota: {e}")))?;

    let status = resp.status;
    let body = resp
        .collect()
        .await
        .map_err(|e| CoreError::UpstreamConnection(format!("claude-code quota body: {e}")))?;

    if !status.is_success() {
        let body_str = String::from_utf8_lossy(&body);
        return Err(CoreError::upstream_error(
            status.as_u16(),
            "claude-code",
            "<quota>",
            body_str.to_string(),
            false,
        ));
    }

    let json: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|e| CoreError::Parse(format!("claude-code quota json parse error: {e}")))?;

    Ok(parse_claude_code_usage_response(&json))
}

pub(crate) fn parse_claude_code_usage_response(json: &serde_json::Value) -> AccountQuota {
    let mut session_used = None;
    let mut session_limit = None;
    let mut session_reset_at = None;

    if let Some(h5) = json.get("five_hour").and_then(|v| v.as_object()) {
        if let Some(util) = h5.get("utilization").and_then(|v| v.as_f64()) {
            session_used = Some(util.round() as i64);
            session_limit = Some(100);
        }
        if let Some(r) = h5.get("resets_at").and_then(|v| v.as_str()) {
            session_reset_at = Some(r.to_string());
        }
    }

    let mut weekly_used = None;
    let mut weekly_limit = None;
    let mut weekly_reset_at = None;

    if let Some(d7) = json.get("seven_day").and_then(|v| v.as_object()) {
        if let Some(util) = d7.get("utilization").and_then(|v| v.as_f64()) {
            weekly_used = Some(util.round() as i64);
            weekly_limit = Some(100);
        }
        if let Some(r) = d7.get("resets_at").and_then(|v| v.as_str()) {
            weekly_reset_at = Some(r.to_string());
        }
    }

    let mut model_details = Vec::new();

    if let Some(opus) = json.get("seven_day_opus").and_then(|v| v.as_object()) {
        let util = opus
            .get("utilization")
            .or_else(|| opus.get("percent"))
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let resets_at = opus
            .get("resets_at")
            .and_then(|v| v.as_str())
            .map(ToString::to_string);
        let remaining_fraction = ((100.0 - util).max(0.0) / 100.0).clamp(0.0, 1.0);
        model_details.push(ModelQuotaDetail {
            model_id: "Claude Opus (Weekly)".to_string(),
            session_used: util.round() as i64,
            session_limit: 100,
            session_reset_at: resets_at,
            remaining_fraction,
        });
    }

    if let Some(sonnet) = json.get("seven_day_sonnet").and_then(|v| v.as_object()) {
        let util = sonnet
            .get("utilization")
            .or_else(|| sonnet.get("percent"))
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let resets_at = sonnet
            .get("resets_at")
            .and_then(|v| v.as_str())
            .map(ToString::to_string);
        let remaining_fraction = ((100.0 - util).max(0.0) / 100.0).clamp(0.0, 1.0);
        model_details.push(ModelQuotaDetail {
            model_id: "Claude Sonnet (Weekly)".to_string(),
            session_used: util.round() as i64,
            session_limit: 100,
            session_reset_at: resets_at,
            remaining_fraction,
        });
    }

    if let Some(limits) = json.get("limits").and_then(|v| v.as_array()) {
        for lim in limits {
            let kind = lim.get("kind").and_then(|k| k.as_str()).unwrap_or("");
            if kind == "session" || kind == "weekly_all" {
                continue;
            }
            let Some(name) = lim
                .get("scope")
                .and_then(|s| s.get("model"))
                .and_then(|m| m.get("display_name").or_else(|| m.get("id")))
                .or_else(|| lim.get("name"))
                .and_then(|n| n.as_str())
            else {
                continue;
            };
            let util = lim
                .get("utilization")
                .or_else(|| lim.get("percent"))
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let resets_at = lim
                .get("resets_at")
                .and_then(|v| v.as_str())
                .map(ToString::to_string);
            let remaining_fraction = ((100.0 - util).max(0.0) / 100.0).clamp(0.0, 1.0);
            model_details.push(ModelQuotaDetail {
                model_id: name.to_string(),
                session_used: util.round() as i64,
                session_limit: 100,
                session_reset_at: resets_at,
                remaining_fraction,
            });
        }
    }

    let is_extra_usage_enabled = json
        .get("extra_usage")
        .and_then(|v| v.get("is_enabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let plan_name = if is_extra_usage_enabled {
        Some("Claude Code (Max/Team)".to_string())
    } else if session_limit.is_some() || weekly_limit.is_some() {
        Some("Claude Code (Pro)".to_string())
    } else {
        Some("Claude Code".to_string())
    };

    AccountQuota {
        session_used,
        session_limit,
        session_reset_at,
        weekly_used,
        weekly_limit,
        weekly_reset_at,
        plan_name,
        last_fetched_at: openproxy_types::now_unix_secs_str(),
        fetch_error: None,
        pools: None,
        model_details: if model_details.is_empty() {
            None
        } else {
            Some(model_details.into_boxed_slice())
        },
    }
}

/// Overwrite the `session_id` field inside the JSON-encoded
/// `metadata.user_id` string with the canonical session the pipeline placed in
/// the `X-Claude-Code-Session-Id` header, so the body and header agree.
fn sync_claude_session_id(json: &mut serde_json::Value, headers: &[(String, String)]) {
    let Some(session) = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("x-claude-code-session-id"))
        .map(|(_, v)| v.trim().to_string())
        .filter(|v| !v.is_empty())
    else {
        return;
    };

    let Some(meta) = json.get_mut("metadata").and_then(|m| m.as_object_mut()) else {
        return;
    };

    // `user_id` is itself a JSON-encoded string, not a nested object.
    let mut user_id = meta
        .get("user_id")
        .and_then(|v| v.as_str())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .unwrap_or_else(|| serde_json::json!({}));

    if let Some(obj) = user_id.as_object_mut() {
        obj.insert("session_id".to_string(), serde_json::Value::String(session));
    }

    meta.insert(
        "user_id".to_string(),
        serde_json::Value::String(user_id.to_string()),
    );
}

/// Aligns the billing header's `cc_version` with the pinned version used for
/// the identity headers.
///
/// When the body already advertises the pinned version this is a no-op, so the
/// common path never re-derives the build fingerprint and the signed bytes stay
/// identical to what the wrapper produced. Only a mid-request profile reload
/// (which is exactly the split this guards) rewrites the block, and it rebuilds
/// the fingerprint from the pinned version so both halves stay consistent.
fn apply_claude_code_billing_version(json: &mut serde_json::Value, version: &str) {
    let (existing_text, entrypoint) = {
        let Some(system) = json.get("system").and_then(|s| s.as_array()) else {
            return;
        };
        let Some(first) = system.first().and_then(|b| b.as_object()) else {
            return;
        };
        let Some(text) = first.get("text").and_then(|t| t.as_str()) else {
            return;
        };
        if !text.starts_with("x-anthropic-billing-header:") {
            return;
        }
        let entrypoint = text
            .split("cc_entrypoint=")
            .nth(1)
            .and_then(|rest| rest.split(';').next())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("cli")
            .to_string();
        (text.to_string(), entrypoint)
    };

    // Already coherent: leave the block (and its fingerprint) untouched.
    if existing_text.contains(&format!("cc_version={version}.")) {
        return;
    }

    let message_text = cloaking::extract_first_user_message_text(json);
    let (_, prev_req, prompt_id) = cloaking::extract_claude_billing_tags(json);
    let billing_text = cloaking::generate_billing_header(
        version,
        &message_text,
        &entrypoint,
        prev_req.as_deref(),
        prompt_id.as_deref(),
    );

    if let Some(first) = json
        .get_mut("system")
        .and_then(|s| s.as_array_mut())
        .and_then(|arr| arr.first_mut())
        .and_then(|block| block.as_object_mut())
        && let Some(text) = first.get_mut("text")
    {
        *text = serde_json::Value::String(billing_text);
    }
}

/// Replaces an existing header value in place when the key matches, keeping the
/// caller's key casing and the position of their entry. Never inserts a header
/// that was not already there: dropping or adding identity headers is the
/// header builder's job, not the finalizer's.
fn set_header_if_present(headers: &mut [(String, String)], key: &str, value: &str) {
    for (existing_key, existing_value) in headers.iter_mut() {
        if existing_key.eq_ignore_ascii_case(key) {
            if existing_value != value {
                *existing_value = value.to_string();
            }
            return;
        }
    }
}

/// Merge body-derived betas into the `anthropic-beta` header without dropping
/// betas the downstream caller requested (case-insensitive de-duplication).
fn merge_claude_beta_header(headers: &mut Vec<(String, String)>, body_betas: &str) {
    let mut betas = Vec::<String>::new();
    for value in headers
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case("anthropic-beta"))
        .map(|(_, value)| value.as_str())
        .chain(std::iter::once(body_betas))
    {
        for beta in value
            .split(',')
            .map(str::trim)
            .filter(|beta| !beta.is_empty())
        {
            if !betas
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(beta))
            {
                betas.push(beta.to_string());
            }
        }
    }
    headers.retain(|(key, _)| !key.eq_ignore_ascii_case("anthropic-beta"));
    headers.push(("anthropic-beta".to_string(), betas.join(",")));
}

#[cfg(test)]
mod tests;
