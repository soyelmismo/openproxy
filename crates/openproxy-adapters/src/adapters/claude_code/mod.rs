use super::{
    AdapterAuthType, AdapterFormat, Arc, DiscoveredModel, ModelId, ProviderAdapter,
    ProviderAdapterConfig, ProviderId, Result, TargetFormat, UpstreamClient, UpstreamRequest,
};
use openproxy_types::{AccountQuota, CoreError, ModelQuotaDetail, ProviderMetadata};

pub const CLAUDE_CODE_DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub const CLAUDE_CODE_USAGE_PATH: &str = "/api/oauth/usage";
pub const CLAUDE_CODE_ANTHROPIC_VERSION: &str = "2023-06-01";
pub const CLAUDE_CODE_BETA_HEADER: &str =
    "oauth-2025-04-20,claude-code-20250219,interleaved-thinking-2025-05-14";
pub const CLAUDE_CODE_USER_AGENT: &str = "claude-code/0.2.29";

pub mod signing;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ClaudeCodeAdapter {
    config: ProviderAdapterConfig,
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
        }
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
            http::HeaderValue::from_static(CLAUDE_CODE_ANTHROPIC_VERSION),
        );
        req.headers.insert(
            http::header::HeaderName::from_static("anthropic-beta"),
            http::HeaderValue::from_static("oauth-2025-04-20,claude-code-20250219"),
        );
        req.headers.insert(
            http::header::USER_AGENT,
            http::HeaderValue::from_static(CLAUDE_CODE_USER_AGENT),
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
    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn config_mut(&mut self) -> Option<&mut ProviderAdapterConfig> {
        Some(&mut self.config)
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
        let auth = if !api_key.trim().is_empty() {
            Some(("Authorization".into(), format!("Bearer {}", api_key.trim())))
        } else {
            None
        };
        crate::adapters::traits::build_spoofer_headers(
            auth,
            &crate::spoofer::ClaudeCodeSpoofer::new(),
            &self.config.extra_headers,
        )
    }

    fn wrap_request_body(
        &self,
        body: bytes::Bytes,
        _target_format: TargetFormat,
        _model: &ModelId,
        resolved_target: &openproxy_types::context::ResolvedTarget,
    ) -> Result<bytes::Bytes> {
        let mut json = serde_json::from_slice::<serde_json::Value>(&body)
            .map_err(|e| CoreError::Parse(format!("failed to parse claude-code request: {e}")))?;

        let account_uuid = resolved_target
            .custom_meta
            .as_ref()
            .and_then(|m| m.claude_account_uuid.as_deref())
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
                    .map(|s| s.leak() as &str)
            })
            .unwrap_or("53098eb0-6bbd-4c76-9f9c-09bc956164bc");

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

        let session_id = uuid::Uuid::new_v4().to_string();

        if let Some(obj) = json.as_object_mut() {
            let meta_val = obj
                .entry("metadata")
                .or_insert_with(|| serde_json::json!({}));
            if let Some(meta_obj) = meta_val.as_object_mut() {
                if let Some(existing_user_id) = meta_obj.get("user_id").and_then(|v| v.as_str()) {
                    if let Ok(mut parsed_id) =
                        serde_json::from_str::<serde_json::Value>(existing_user_id)
                        && let Some(id_map) = parsed_id.as_object_mut()
                    {
                        id_map.insert("device_id".into(), serde_json::Value::String(device_id));
                        if account_uuid != "53098eb0-6bbd-4c76-9f9c-09bc956164bc"
                            || !id_map.contains_key("account_uuid")
                        {
                            id_map.insert(
                                "account_uuid".into(),
                                serde_json::Value::String(account_uuid.to_string()),
                            );
                        }
                        if !id_map.contains_key("session_id") {
                            id_map
                                .insert("session_id".into(), serde_json::Value::String(session_id));
                        }
                        meta_obj.insert(
                            "user_id".to_string(),
                            serde_json::Value::String(parsed_id.to_string()),
                        );
                    }
                } else {
                    let user_id_obj = serde_json::json!({
                        "device_id": device_id,
                        "account_uuid": account_uuid,
                        "session_id": session_id,
                    });
                    meta_obj.insert(
                        "user_id".to_string(),
                        serde_json::Value::String(user_id_obj.to_string()),
                    );
                }
            }
        }

        signing::ensure_claude_billing_header(&mut json);

        let mut body_bytes = serde_json::to_vec(&json).map_err(|e| {
            CoreError::Parse(format!("failed to serialize claude-code request: {e}"))
        })?;

        if let Err(e) = signing::sign_anthropic_messages_body(&mut body_bytes) {
            tracing::warn!(error = %e, "claude-code: cch signing failed");
        }

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
        .call(req, crate::upstream::TimeoutProfile::Chat, cancel)
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

#[cfg(test)]
mod tests;
