use super::{
    AdapterAuthType, AdapterFormat, Arc, CancellationToken, CoreError, DiscoveredModel, ModelId,
    ProviderAdapter, ProviderAdapterConfig, ProviderId, Result, TargetFormat, TimeoutProfile,
    UpstreamClient, UpstreamRequest,
};
use openproxy_types::ResultExt;

pub use crate::spoofer::CODEX_SPOOFING_HEADERS;
use crate::spoofer::{ClientSpoofer, CodexSpoofer, current_codex_ua, current_codex_version};

pub fn codex_client_version() -> String {
    current_codex_version()
}

pub fn codex_user_agent() -> String {
    current_codex_ua()
}

pub fn codex_client_version_str() -> &'static str {
    crate::spoofer::DEFAULT_CODEX_VERSION
}

pub fn codex_user_agent_str() -> &'static str {
    "codex-cli/0.144.0 (Windows 10.0.26200; x64)"
}

pub const CODEX_MODELS_URL: &str = "https://chatgpt.com/backend-api/codex/models";
pub const CODEX_UPSTREAM_MODELS_RAW_URL: &str =
    "https://raw.githubusercontent.com/openai/codex/main/codex-rs/models-manager/models.json";

pub fn apply_codex_spoofing_headers(req: &mut UpstreamRequest) {
    CodexSpoofer.apply_to_request(req);
}

/// Returns the official bundled model catalog for Codex (from codex-rs/models-manager/models.json).
pub fn codex_static_models() -> Vec<DiscoveredModel> {
    CodexAdapter::hardcoded_models()
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CodexAdapter {
    config: ProviderAdapterConfig,
}

impl CodexAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("codex"),
                name: "Codex".into(),
                anonymous_fallback: false,
                rate_limit_scope: "account".into(),
                base_url: "https://chatgpt.com/backend-api/codex".into(),
                auth_type: AdapterAuthType::OAuth,
                format: AdapterFormat::Responses,
                extra_headers: vec![],
            },
        }
    }

    fn build_hardcoded_codex_model(
        (id, name, ctx, _max_ctx, vision): (&str, &str, i64, i64, bool),
    ) -> DiscoveredModel {
        let input_modalities = if vision {
            Some(vec!["text".to_string(), "image".to_string()].into())
        } else {
            Some(vec!["text".to_string()].into())
        };
        let caps = openproxy_types::ModelCapabilities {
            vision: Some(vision),
            tool_calling: Some(true),
            reasoning: Some(true),
            thinking: Some(true),
            ..Default::default()
        };
        DiscoveredModel {
            model_id: ModelId::new(id),
            display_name: Some(name.to_string()),
            target_format: TargetFormat::Responses,
            context_length: Some(ctx),
            max_output_tokens: Some(32_768),
            input_modalities,
            output_modalities: Some(vec!["text".to_string()].into()),
            model_type: Some("chat".to_string()),
            family: Some("gpt".to_string()),
            capabilities: Some(caps),
        }
    }

    fn hardcoded_models() -> Vec<DiscoveredModel> {
        // Models from codex-rs/models-manager/models.json (official Codex bundled catalog)
        // Updated from https://github.com/openai/codex
        [
            ("gpt-6-astra", "GPT-6-Astra", 272_000, 872_000, true),
            ("gpt-6-sol", "GPT-6-Sol", 272_000, 872_000, true),
            ("gpt-6-luna", "GPT-6-Luna", 272_000, 872_000, true),
            ("gpt-5.6-sol", "GPT-5.6-Sol", 272_000, 872_000, true),
            ("gpt-5.6-terra", "GPT-5.6-Terra", 272_000, 872_000, true),
            ("gpt-5.6-luna", "GPT-5.6-Luna", 272_000, 872_000, true),
            ("gpt-daybreak-blue-latest", "Daybreak Blue", 272_000, 872_000, true),
            ("gpt-daybreak-red-latest", "Daybreak Red", 372_000, 372_000, true),
            ("gpt-5.5", "GPT-5.5", 272_000, 272_000, true),
            ("gpt-5.4", "GPT-5.4", 272_000, 1_000_000, true),
            // codex-auto-review is internal (visibility: hide), excluded from user-facing list
        ]
        .into_iter()
        .map(Self::build_hardcoded_codex_model)
        .collect()
    }
}

impl Default for CodexAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderAdapter for CodexAdapter {
    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn config_mut(&mut self) -> Option<&mut ProviderAdapterConfig> {
        Some(&mut self.config)
    }

    fn metadata(&self) -> openproxy_types::ProviderMetadata {
        openproxy_types::ProviderMetadata {
            built_in: true,
            deletable: false,
            supports_quota: true,
            quota_refresh_supported: true,
            requires_oauth: true,
            oauth_refresh_lead_seconds: Some(300),
        }
    }

    fn build_headers(
        &self,
        api_key: &str,
        _target_format: TargetFormat,
        _model: &ModelId,
    ) -> Vec<(String, String)> {
        super::build_spoofer_headers(
            self.build_auth_header(api_key),
            &CodexSpoofer,
            &self.config.extra_headers,
        )
    }

    fn wrap_request_body(
        &self,
        body: bytes::Bytes,
        _target_format: TargetFormat,
        _model: &ModelId,
        _resolved_target: &openproxy_types::context::ResolvedTarget,
    ) -> std::result::Result<bytes::Bytes, openproxy_types::error::CoreError> {
        if body.is_empty() {
            return Ok(body);
        }
        let mut val: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|e| openproxy_types::error::CoreError::Parse(e.to_string()))?;

        if let Some(obj) = val.as_object_mut() {
            patch_codex_request_object(obj);
        }

        let new_body = serde_json::to_vec(&val)
            .map_err(|e| openproxy_types::error::CoreError::Parse(e.to_string()))?;
        Ok(bytes::Bytes::from(new_body))
    }

    fn models_url(&self) -> Option<String> {
        Some(CODEX_MODELS_URL.to_string())
    }

    async fn fetch_models(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        // 1. If an access token is available, attempt dynamic discovery from backend-api/codex/models
        if !api_key.trim().is_empty()
            && let Some(models) = try_fetch_backend_models(upstream_client, api_key).await
        {
            return Ok(models);
        }

        // 2. Fetch dynamically from official upstream Codex repository (bundled models.json)
        if let Some(models) = try_fetch_upstream_repo_models(upstream_client).await {
            return Ok(models);
        }

        // 3. Resilient fallback to embedded catalog if network unreachable
        Ok(Self::hardcoded_models())
    }

    async fn fetch_quota(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        _: &str,
        access_token: Option<&str>,
        provider_specific: Option<&str>,
    ) -> Option<Result<openproxy_types::AccountQuota>> {
        if let Some(token) = access_token {
            Some(
                self.fetch_codex_quota_local(upstream_client, token, provider_specific)
                    .await,
            )
        } else {
            Some(Ok(openproxy_types::AccountQuota::with_error(
                "codex requires OAuth access token",
            )))
        }
    }
}

impl CodexAdapter {
    async fn fetch_codex_quota_local(
        &self,
        upstream: &Arc<UpstreamClient>,
        access_token: &str,
        workspace_id: Option<&str>,
    ) -> Result<openproxy_types::AccountQuota> {
        let req = build_codex_quota_request(access_token, workspace_id);
        let cancel = CancellationToken::new();
        let response = upstream
            .call(req, TimeoutProfile::Chat, cancel)
            .await
            .map_err(|e| CoreError::UpstreamConnection(e.to_string()))?;

        let status = response.status.as_u16();
        if !response.status.is_success() {
            let body = response.collect().await.unwrap_or_default();
            let snippet = String::from_utf8_lossy(&body)
                .chars()
                .take(200)
                .collect::<String>();
            return Ok(build_codex_error_quota(status, &snippet));
        }

        let body = response.collect().await.ctx_upstream("codex quota read")?;
        let json: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|e| CoreError::Parse(format!("codex quota parse: {e}")))?;
        parse_codex_usage_quota(&json)
    }
}

fn patch_codex_request_object(obj: &mut serde_json::Map<String, serde_json::Value>) {
    // Codex backend ALWAYS requires stream: true, else it returns HTTP 400 {"detail":"Stream must be set to true"}
    obj.insert("stream".to_string(), serde_json::Value::Bool(true));
}

/// Parses upstream Codex models JSON response into a list of DiscoveredModel.
/// Supports `{ "models": [...] }`, `{ "data": [...] }`, or top-level arrays.
pub fn parse_codex_models_json(bytes: &[u8]) -> Result<Vec<DiscoveredModel>> {
    let json: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|e| CoreError::Parse(format!("codex models parse error: {e}")))?;

    let models_arr = json
        .get("models")
        .or_else(|| json.get("data"))
        .and_then(|v| v.as_array())
        .or_else(|| json.as_array())
        .ok_or_else(|| CoreError::Parse("codex models missing 'models' or 'data' array".into()))?;

    let mut items: Vec<(&serde_json::Value, i64)> = models_arr
        .iter()
        .map(|v| {
            let priority = v
                .get("priority")
                .and_then(|p| p.as_i64())
                .unwrap_or(i64::MAX);
            (v, priority)
        })
        .collect();
    items.sort_by_key(|(_, p)| *p);

    let discovered: Vec<DiscoveredModel> = items
        .into_iter()
        .filter_map(|(v, _)| map_codex_model_value(v))
        .collect();

    if discovered.is_empty() {
        return Err(CoreError::Parse("codex models empty after filtering".into()));
    }

    Ok(discovered)
}

fn map_codex_model_value(val: &serde_json::Value) -> Option<DiscoveredModel> {
    let slug = val
        .get("slug")
        .or_else(|| val.get("id"))
        .and_then(|v| v.as_str())?;

    // Exclude internal review model
    if slug == "codex-auto-review" {
        return None;
    }

    let display_name = val
        .get("display_name")
        .or_else(|| val.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or(slug);

    let context_length = val
        .get("context_window")
        .or_else(|| val.get("context_length"))
        .and_then(|v| v.as_i64())
        .unwrap_or(272_000);

    let max_output_tokens = val
        .get("max_output_tokens")
        .and_then(|v| v.as_i64())
        .or(Some(32_768));

    let has_image = val
        .get("input_modalities")
        .and_then(|v| v.as_array())
        .is_none_or(|arr| arr.iter().any(|m| m.as_str() == Some("image")));

    let input_modalities = if has_image {
        Some(vec!["text".to_string(), "image".to_string()].into())
    } else {
        Some(vec!["text".to_string()].into())
    };

    let caps = openproxy_types::ModelCapabilities {
        vision: Some(has_image),
        tool_calling: Some(true),
        reasoning: Some(true),
        thinking: Some(true),
        ..Default::default()
    };

    Some(DiscoveredModel {
        model_id: ModelId::new(slug),
        display_name: Some(display_name.to_string()),
        target_format: TargetFormat::Responses,
        context_length: Some(context_length),
        max_output_tokens,
        input_modalities,
        output_modalities: Some(vec!["text".to_string()].into()),
        model_type: Some("chat".to_string()),
        family: Some("gpt".to_string()),
        capabilities: Some(caps),
    })
}

async fn try_fetch_backend_models(
    upstream_client: &Arc<UpstreamClient>,
    api_key: &str,
) -> Option<Vec<DiscoveredModel>> {
    let url = format!("{CODEX_MODELS_URL}?client_version={}", current_codex_version());
    let mut req = UpstreamRequest::get(&url);
    if let Ok(v) = http::HeaderValue::from_str(&format!("Bearer {api_key}")) {
        req.headers.insert(http::header::AUTHORIZATION, v);
    }
    req.headers.insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("application/json"),
    );
    apply_codex_spoofing_headers(&mut req);

    let cancel = CancellationToken::new();
    let resp = upstream_client
        .call(req, TimeoutProfile::ModelDiscovery, cancel)
        .await
        .ok()?;

    if !resp.status.is_success() {
        return None;
    }

    let body = resp.collect().await.ok()?;
    parse_codex_models_json(&body).ok()
}

async fn try_fetch_upstream_repo_models(
    upstream_client: &Arc<UpstreamClient>,
) -> Option<Vec<DiscoveredModel>> {
    let mut req = UpstreamRequest::get(CODEX_UPSTREAM_MODELS_RAW_URL);
    req.headers.insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("application/json"),
    );
    if let Ok(v) = http::HeaderValue::from_str(&current_codex_ua()) {
        req.headers.insert(http::header::USER_AGENT, v);
    }

    let cancel = CancellationToken::new();
    let resp = upstream_client
        .call(req, TimeoutProfile::ModelDiscovery, cancel)
        .await
        .ok()?;

    if !resp.status.is_success() {
        return None;
    }

    let body = resp.collect().await.ok()?;
    parse_codex_models_json(&body).ok()
}

fn build_codex_quota_request(access_token: &str, workspace_id: Option<&str>) -> UpstreamRequest {
    let url = "https://chatgpt.com/backend-api/wham/usage";
    let mut req = UpstreamRequest::get(url);
    req.headers.insert(
        http::header::AUTHORIZATION,
        http::HeaderValue::from_str(&format!("Bearer {access_token}"))
            .unwrap_or_else(|_| http::HeaderValue::from_static("")),
    );
    req.headers.insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("application/json"),
    );
    req.headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    apply_codex_spoofing_headers(&mut req);
    let workspace_header = workspace_id.and_then(codex_workspace_header);
    if let Some(ws) = workspace_header.as_deref()
        && let Ok(val) = http::HeaderValue::from_str(ws)
    {
        req.headers
            .insert(http::HeaderName::from_static("chatgpt-account-id"), val);
    }
    req
}

fn build_codex_error_quota(status: u16, snippet: &str) -> openproxy_types::AccountQuota {
    let fetch_error = if snippet.is_empty() {
        format!("Codex quota check failed: HTTP {status}")
    } else {
        format!("Codex quota check failed: HTTP {status}: {snippet}")
    };
    openproxy_types::AccountQuota::with_error(fetch_error)
}

fn codex_workspace_header(provider_specific: &str) -> Option<String> {
    let raw = provider_specific.trim();
    if raw.is_empty() {
        return None;
    }
    if !raw.starts_with('{') {
        return Some(raw.to_string());
    }
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|v| {
            v.get("workspaceId")
                .or_else(|| v.get("workspace_id"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(ToString::to_string)
        })
}

fn parse_codex_usage_quota(body: &serde_json::Value) -> Result<openproxy_types::AccountQuota> {
    let rate_limit = body
        .get("rate_limit")
        .or_else(|| body.get("rateLimit"))
        .and_then(|v| v.as_object())
        .ok_or_else(|| CoreError::Parse("codex quota missing rate_limit".into()))?;

    let primary = rate_limit
        .get("primary_window")
        .or_else(|| rate_limit.get("primaryWindow"));
    let secondary = rate_limit
        .get("secondary_window")
        .or_else(|| rate_limit.get("secondaryWindow"));
    let (session_used, session_reset_at) = parse_codex_usage_window(primary);
    let (weekly_used, weekly_reset_at) = parse_codex_usage_window(secondary);

    Ok(openproxy_types::AccountQuota {
        session_used,
        session_limit: session_used.map(|_| 100),
        session_reset_at,
        weekly_used,
        weekly_limit: weekly_used.map(|_| 100),
        weekly_reset_at,
        plan_name: Some("Codex / ChatGPT".into()),
        last_fetched_at: openproxy_types::now_unix_secs_str(),
        fetch_error: None,
        model_details: None,
    })
}

fn parse_codex_usage_window(window: Option<&serde_json::Value>) -> (Option<i64>, Option<String>) {
    let Some(window) = window.and_then(|v| v.as_object()) else {
        return (None, None);
    };
    let used = window
        .get("used_percent")
        .or_else(|| window.get("usedPercent"))
        .and_then(json_f64)
        .map(|v| v.round().clamp(0.0, 100.0) as i64);
    let reset_at = window
        .get("reset_at")
        .or_else(|| window.get("resetAt"))
        .and_then(json_f64)
        .filter(|v| *v > 0.0)
        .map(|v| (v.ceil() as u64).to_string())
        .or_else(|| {
            window
                .get("reset_after_seconds")
                .or_else(|| window.get("resetAfterSeconds"))
                .and_then(json_f64)
                .filter(|v| *v > 0.0)
                .map(|v| {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_secs());
                    (now + v.ceil() as u64).to_string()
                })
        });
    (used, reset_at)
}

fn json_f64(value: &serde_json::Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.parse::<f64>().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_codex_usage_quota_valid() {
        let body = json!({
            "rate_limit": {
                "primary_window": {
                    "used_percent": 42.5,
                    "reset_at": 1_700_000_000.0
                },
                "secondary_window": {
                    "usedPercent": 85.1,
                    "resetAfterSeconds": 3600.0
                }
            }
        });

        let quota = parse_codex_usage_quota(&body).expect("should parse");
        assert_eq!(quota.session_used, Some(43));
        assert_eq!(quota.session_reset_at, Some("1700000000".to_string()));
        assert_eq!(quota.weekly_used, Some(85));
        assert!(quota.weekly_reset_at.is_some());
    }

    #[test]
    fn test_parse_codex_usage_quota_missing_rate_limit() {
        let body = json!({});
        let err = parse_codex_usage_quota(&body).unwrap_err();
        assert!(err.to_string().contains("codex quota missing rate_limit"));
    }

    #[test]
    fn test_apply_codex_spoofing_headers() {
        let mut req = UpstreamRequest::post_json("http://dummy.com", bytes::Bytes::new());
        apply_codex_spoofing_headers(&mut req);

        for &(k, v) in CODEX_SPOOFING_HEADERS {
            let header_value = req.headers.get(k).expect("header missing");
            if k == "user-agent" {
                assert_eq!(header_value, current_codex_ua().as_str());
            } else if k == "version" {
                assert_eq!(header_value, current_codex_version().as_str());
            } else {
                assert_eq!(header_value, http::HeaderValue::from_str(v).unwrap());
            }
        }
    }

    #[test]
    fn test_wrap_request_body_enforces_stream_true() {
        let adapter = CodexAdapter::new();
        let json_body = serde_json::json!({
            "model": "gpt-5.6-luna",
            "input": [{"role": "user", "content": "hi"}],
            "stream": false
        });
        let body_bytes = bytes::Bytes::from(serde_json::to_vec(&json_body).unwrap());

        let resolved_target = openproxy_types::context::ResolvedTarget {
            target: openproxy_types::combos::ComboTarget {
                id: openproxy_types::ComboTargetId(1),
                combo_id: openproxy_types::ComboId(1),
                provider_id: openproxy_types::ProviderId::new("codex"),
                account_id: None,
                model_row_id: Some(openproxy_types::ModelRowId(1)),
                sub_combo_id: None,
                priority_order: 0,
                weight: 100,
                active: true,
                rate_limit_scope: openproxy_types::RateLimitScope::Account,
                cooldown_mode: None,
                cooldown_base_secs: None,
                cooldown_max_secs: None,
                cooldown_factor: None,
                thinking_effort: None,
                description: None,
            },
            model: openproxy_types::Model {
                row_id: openproxy_types::ModelRowId(1),
                provider_id: openproxy_types::ProviderId::new("codex"),
                model_id: openproxy_types::ModelId::new("gpt-5.6-luna"),
                target_format: openproxy_types::TargetFormat::Responses,
                discovered_at: openproxy_types::now_unix_secs_str().into_boxed_str(),
                context_length: Some(272_000),
                max_output_tokens: Some(32_768),
                model_type: "chat".into(),
                ..Default::default()
            },
            api_key: "tok".to_string(),
            api_key_label: None,
            custom_meta: None,
        };

        let wrapped = adapter
            .wrap_request_body(
                body_bytes,
                TargetFormat::Responses,
                &ModelId::new("gpt-5.6-luna"),
                &resolved_target,
            )
            .expect("wrap should succeed");

        let val: serde_json::Value = serde_json::from_slice(&wrapped).unwrap();
        assert_eq!(
            val.get("stream").and_then(serde_json::Value::as_bool),
            Some(true),
            "Codex must ALWAYS enforce stream: true on request payload"
        );
    }

    #[test]
    fn test_codex_build_headers_with_extra_and_dynamic_overrides() {
        let _guard = crate::spoofer::CODEX_TEST_LOCK.lock().unwrap();
        crate::spoofer::reset_dynamic_codex_overrides();

        let mut adapter = CodexAdapter::new();
        adapter.config_mut().unwrap().extra_headers = vec![
            ("x-codex-workspace".to_string(), "ws-123".to_string()),
            ("user-agent".to_string(), "CustomCodex/2.0".to_string()),
        ];

        crate::spoofer::set_dynamic_codex_extra_header("x-dynamic-header", "dynamic-val");

        let headers = adapter.build_headers(
            "my-key",
            TargetFormat::Responses,
            &ModelId::new("gpt-5.6-luna"),
        );

        let find = |k: &str| {
            headers
                .iter()
                .find(|(hk, _)| hk.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.as_str())
        };

        assert_eq!(find("Authorization"), Some("Bearer my-key"));
        assert_eq!(find("Content-Type"), Some("application/json"));
        assert_eq!(find("x-codex-workspace"), Some("ws-123"));
        assert_eq!(find("user-agent"), Some("CustomCodex/2.0"));
        assert_eq!(find("x-dynamic-header"), Some("dynamic-val"));
        assert_eq!(find("origin"), Some("https://chatgpt.com"));
        assert_eq!(find("originator"), Some("codex_cli_rs"));

        crate::spoofer::reset_dynamic_codex_overrides();
    }

    #[test]
    fn test_codex_default_headers_contract() {
        let _guard = crate::spoofer::CODEX_TEST_LOCK.lock().unwrap();
        crate::spoofer::reset_dynamic_codex_overrides();

        let adapter = CodexAdapter::new();
        let headers = adapter.build_headers(
            "codex-token-xyz",
            TargetFormat::Responses,
            &ModelId::new("gpt-5.6-luna"),
        );
        let find = |k: &str| {
            headers
                .iter()
                .find(|(hk, _)| hk.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.as_str())
        };

        // Strict bijective closed-world set equality contract
        let actual_keys: std::collections::BTreeSet<String> = headers
            .iter()
            .map(|(k, _)| k.to_ascii_lowercase())
            .collect();
        let expected_keys: std::collections::BTreeSet<String> = [
            "authorization",
            "content-type",
            "origin",
            "originator",
            "version",
            "user-agent",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        assert_eq!(
            actual_keys, expected_keys,
            "Codex contract breach: header added or removed"
        );

        assert_eq!(find("authorization"), Some("Bearer codex-token-xyz"));
        assert_eq!(find("content-type"), Some("application/json"));
        assert_eq!(find("origin"), Some("https://chatgpt.com"));
        assert_eq!(find("originator"), Some("codex_cli_rs"));
        assert_eq!(find("version"), Some(current_codex_version().as_str()));
        assert_eq!(find("user-agent"), Some(current_codex_ua().as_str()));
    }

    #[test]
    fn test_codex_hardcoded_models_parity() {
        let models = codex_static_models();
        assert_eq!(models.len(), 10);
        let ids: Vec<&str> = models.iter().map(|m| m.model_id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "gpt-6-astra",
                "gpt-6-sol",
                "gpt-6-luna",
                "gpt-5.6-sol",
                "gpt-5.6-terra",
                "gpt-5.6-luna",
                "gpt-daybreak-blue-latest",
                "gpt-daybreak-red-latest",
                "gpt-5.5",
                "gpt-5.4",
            ]
        );
        for m in &models {
            assert_eq!(m.target_format, TargetFormat::Responses);
            assert!(m.context_length.unwrap_or(0) >= 272_000);
            assert_eq!(m.max_output_tokens, Some(32_768));
            let caps = m.capabilities.as_ref().expect("capabilities");
            assert_eq!(caps.vision, Some(true));
            assert_eq!(caps.tool_calling, Some(true));
            assert_eq!(caps.reasoning, Some(true));
            assert_eq!(caps.thinking, Some(true));
        }
    }
}
