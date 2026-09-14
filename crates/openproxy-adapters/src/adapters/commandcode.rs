//! Command Code adapter.
//!
//! Handles reverse-engineered Command Code CLI protocol:
//! - POST `/alpha/generate` upstream endpoint for Command Code Go accounts.
//! - Dynamic `x-command-code-version` header acquired from npm registry with local fallback.
//! - Payload packaging matching Command Code CLI environment and schema.
//! - Live `/provider/v1/models` discovery and `/alpha/billing/*` quota tracking.

use super::{
    Arc, CoreError, DiscoveredModel, ModelId, ProviderAdapter, ProviderAdapterConfig, Result,
    TargetFormat, UpstreamClient,
};
use crate::upstream::{CancellationToken, TimeoutProfile, UpstreamRequest};
use crate::{AdapterAuthType, AdapterFormat};
use openproxy_types::{AccountQuota, ProviderId, ProviderMetadata};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
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
            .map_err(|e| CoreError::UpstreamConnection(format!("commandcode /models: {e}")))?;

        if !resp.status.is_success() {
            return Err(CoreError::UpstreamConnection(format!(
                "commandcode /models returned status {}",
                resp.status
            )));
        }

        let body = resp
            .collect()
            .await
            .map_err(|e| CoreError::UpstreamConnection(format!("read /models body: {e}")))?;

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

fn extract_commandcode_reset(val: Option<&Value>) -> Option<String> {
    val.and_then(|v| {
        v.as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != "0")
            .map(ToString::to_string)
            .or_else(|| v.as_i64().filter(|&n| n > 0).map(|n| n.to_string()))
            .or_else(|| {
                v.as_f64()
                    .filter(|&n| n > 0.0)
                    .map(|n| (n as i64).to_string())
            })
    })
}

fn parse_commandcode_window(win: Option<&Value>) -> (Option<i64>, Option<i64>, Option<String>) {
    let Some(win) = win else {
        return (None, None, None);
    };
    let reset = extract_commandcode_reset(win.get("resetAt"));
    let Some(used) = win.get("used").and_then(Value::as_f64) else {
        return (None, None, reset);
    };
    let Some(cap) = win.get("cap").and_then(Value::as_f64) else {
        return (None, None, reset);
    };

    if cap <= 0.0 {
        return (None, None, reset);
    }

    let is_exceeded = win
        .get("exceeded")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut pct = ((used / cap) * 100.0).round().clamp(0.0, 100.0) as i64;
    if is_exceeded {
        pct = 100;
    }

    (Some(pct), Some(100), reset)
}

fn humanize_commandcode_plan(plan_id: Option<&str>, monthly_info: Option<&str>) -> String {
    let base = match plan_id {
        Some("individual-go") => "Command Code · Go",
        Some("individual-pro") => "Command Code · Pro",
        Some("individual-goat") => "Command Code · GOAT",
        Some("individual-max-10x") => "Command Code · Max 10×",
        Some("individual-max-20x") => "Command Code · Max 20×",
        Some("team-pro") => "Command Code · Team Pro",
        Some(other) if !other.is_empty() => other,
        _ => "Command Code",
    };

    match monthly_info {
        Some(info) if !info.is_empty() => format!("{base} · {info}"),
        _ => base.to_string(),
    }
}

async fn fetch_commandcode_quota(
    upstream_client: &Arc<UpstreamClient>,
    token: &str,
) -> Result<AccountQuota> {
    // 1. Query /alpha/billing/credits
    let credits_url = "https://api.commandcode.ai/alpha/billing/credits";
    let mut credits_req = UpstreamRequest::get(credits_url);
    apply_commandcode_cli_headers(&mut credits_req, token);
    let cancel = CancellationToken::new();
    let credits_resp = upstream_client
        .call(credits_req, TimeoutProfile::Quota, cancel)
        .await
        .map_err(|e| CoreError::UpstreamConnection(format!("credits request failed: {e}")))?;

    let mut session_used = None;
    let mut session_limit = None;
    let mut session_reset_at = None;
    let mut weekly_used = None;
    let mut weekly_limit = None;
    let mut weekly_reset_at = None;
    let mut remaining_monthly = None;

    if credits_resp.status.is_success()
        && let Ok(body) = credits_resp.collect().await
        && let Ok(v) = serde_json::from_slice::<Value>(&body)
    {
        let limits = v.get("windowLimits").unwrap_or(&v);
        let (s_used, s_limit, s_reset) = parse_commandcode_window(limits.get("fiveHour"));
        session_used = s_used;
        session_limit = s_limit;
        session_reset_at = s_reset;

        let (w_used, w_limit, w_reset) = parse_commandcode_window(limits.get("weekly"));
        weekly_used = w_used;
        weekly_limit = w_limit;
        weekly_reset_at = w_reset;

        remaining_monthly = v
            .get("credits")
            .and_then(|c| c.get("monthlyCredits"))
            .and_then(Value::as_f64);
    }

    // 2. Query /alpha/billing/subscriptions
    let sub_url = "https://api.commandcode.ai/alpha/billing/subscriptions";
    let mut sub_req = UpstreamRequest::get(sub_url);
    apply_commandcode_cli_headers(&mut sub_req, token);
    let cancel = CancellationToken::new();
    let (plan_id, period_end) = if let Ok(sub_resp) = upstream_client
        .call(sub_req, TimeoutProfile::Quota, cancel)
        .await
        && sub_resp.status.is_success()
        && let Ok(body) = sub_resp.collect().await
        && let Ok(v) = serde_json::from_slice::<Value>(&body)
    {
        let data = v.get("data").unwrap_or(&v);
        let plan = data
            .get("planId")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        let end = data
            .get("currentPeriodEnd")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        (plan, end)
    } else {
        (None, None)
    };

    // 3. Query /alpha/usage/summary for monthly spent credits
    let summary_url = "https://api.commandcode.ai/alpha/usage/summary";
    let mut summary_req = UpstreamRequest::get(summary_url);
    apply_commandcode_cli_headers(&mut summary_req, token);
    let cancel = CancellationToken::new();
    let used_monthly = if let Ok(summary_resp) = upstream_client
        .call(summary_req, TimeoutProfile::Quota, cancel)
        .await
        && summary_resp.status.is_success()
        && let Ok(body) = summary_resp.collect().await
        && let Ok(v) = serde_json::from_slice::<Value>(&body)
    {
        v.get("totalMonthlyCredits")
            .and_then(Value::as_f64)
            .or_else(|| v.get("totalCost").and_then(Value::as_f64))
    } else {
        None
    };

    let monthly_info = match (remaining_monthly, used_monthly) {
        (Some(rem), Some(used)) if (rem + used) > 0.0 => {
            let total = rem + used;
            let pct = ((used / total) * 100.0).round().clamp(0.0, 100.0) as i64;
            let reset_desc = period_end.as_deref().and_then(|iso| {
                chrono::DateTime::parse_from_rfc3339(iso)
                    .ok()
                    .map(|dt| dt.format("%b %-d").to_string())
            });
            match reset_desc {
                Some(date) => Some(format!("Monthly: {pct}% · resets {date}")),
                None => Some(format!("Monthly: {pct}%")),
            }
        }
        _ => None,
    };

    let plan_name = Some(humanize_commandcode_plan(
        plan_id.as_deref(),
        monthly_info.as_deref(),
    ));

    Ok(AccountQuota {
        session_used,
        session_limit,
        session_reset_at,
        weekly_used,
        weekly_limit,
        weekly_reset_at,
        plan_name,
        last_fetched_at: openproxy_types::now_unix_secs_str(),
        fetch_error: None,
        model_details: None,
    })
}

fn transform_openai_to_commandcode(val: &mut Value, model_name: &str) -> Value {
    let messages = val
        .get_mut("messages")
        .and_then(|m| m.as_array_mut())
        .map(std::mem::take)
        .unwrap_or_default();

    // Pass 1: index tool_call id -> name
    let mut tool_id_to_name = std::collections::HashMap::new();
    for msg in &messages {
        if let Some(tool_calls) = msg.get("tool_calls").and_then(Value::as_array) {
            for tc in tool_calls {
                let id = tc.get("id").and_then(Value::as_str).unwrap_or("");
                let func = tc.get("function").unwrap_or(&Value::Null);
                let name = func.get("name").and_then(Value::as_str).unwrap_or("");
                if !id.is_empty() && !name.is_empty() {
                    tool_id_to_name.insert(id.to_string(), name.to_string());
                }
            }
        }
    }

    let mut system_prompt = String::new();
    let mut cc_messages: Vec<Value> = Vec::with_capacity(messages.len());

    for msg in messages {
        let role = msg
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user")
            .to_ascii_lowercase();

        match role.as_str() {
            "system" | "developer" => {
                let content = extract_content_string(&msg);
                if !content.is_empty() {
                    if !system_prompt.is_empty() {
                        system_prompt.push_str("\n\n");
                    }
                    system_prompt.push_str(&content);
                }
            }
            "assistant" => {
                let mut blocks: Vec<Value> = Vec::new();
                let text = match msg.get("content") {
                    Some(Value::String(s)) if !s.is_empty() => s.as_str(),
                    _ => "",
                };
                if !text.is_empty() {
                    blocks.push(json!({
                        "type": "text",
                        "text": text,
                    }));
                }

                if let Some(tool_calls) = msg.get("tool_calls").and_then(Value::as_array) {
                    for tc in tool_calls {
                        let id = tc.get("id").and_then(Value::as_str).unwrap_or("");
                        let func = tc.get("function").unwrap_or(&Value::Null);
                        let name = func.get("name").and_then(Value::as_str).unwrap_or("");
                        let args = func
                            .get("arguments")
                            .and_then(Value::as_str)
                            .unwrap_or("{}");
                        let input: Value = serde_json::from_str(args).unwrap_or_else(|_| json!({}));
                        blocks.push(json!({
                            "type": "tool-call",
                            "toolCallId": id,
                            "toolName": name,
                            "input": input,
                        }));
                    }
                }

                if blocks.is_empty() {
                    blocks.push(json!({
                        "type": "text",
                        "text": "",
                    }));
                }

                cc_messages.push(json!({
                    "role": "assistant",
                    "content": blocks,
                }));
            }
            "tool" => {
                let id = msg
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let tool_name = tool_id_to_name.get(id).map_or("", |s| s.as_str());
                let content = extract_content_string(&msg);

                cc_messages.push(json!({
                    "role": "tool",
                    "content": [
                        {
                            "type": "tool-result",
                            "toolCallId": id,
                            "toolName": tool_name,
                            "output": {
                                "type": "text",
                                "value": content,
                            }
                        }
                    ],
                }));
            }
            _ => {
                // user role
                let mut blocks: Vec<Value> = Vec::new();
                match msg.get("content") {
                    Some(Value::String(s)) => {
                        blocks.push(json!({
                            "type": "text",
                            "text": s,
                        }));
                    }
                    Some(Value::Array(arr)) => {
                        for part in arr {
                            let p_type = part.get("type").and_then(Value::as_str).unwrap_or("text");
                            if p_type == "text" {
                                let text = part.get("text").and_then(Value::as_str).unwrap_or("");
                                blocks.push(json!({
                                    "type": "text",
                                    "text": text,
                                }));
                            } else if p_type == "image_url"
                                && let Some(url) = part
                                    .get("image_url")
                                    .and_then(|u| u.get("url"))
                                    .and_then(Value::as_str)
                            {
                                blocks.push(json!({
                                    "type": "image",
                                    "image": url,
                                }));
                            }
                        }
                    }
                    Some(v) => {
                        blocks.push(json!({
                            "type": "text",
                            "text": v.to_string(),
                        }));
                    }
                    None => {}
                }

                if blocks.is_empty() {
                    blocks.push(json!({
                        "type": "text",
                        "text": "",
                    }));
                }

                cc_messages.push(json!({
                    "role": "user",
                    "content": blocks,
                }));
            }
        }
    }

    if cc_messages.is_empty() {
        cc_messages.push(json!({
            "role": "user",
            "content": [
                {
                    "type": "text",
                    "text": "",
                }
            ],
        }));
    }

    let mut params_obj = serde_json::Map::new();
    params_obj.insert("model".into(), json!(model_name));
    params_obj.insert("messages".into(), json!(cc_messages));
    params_obj.insert("stream".into(), json!(true));

    if !system_prompt.is_empty() {
        params_obj.insert("system".into(), json!(system_prompt));
    }

    if let Some(tools) = val.get("tools").and_then(Value::as_array) {
        let cc_tools: Vec<Value> = tools
            .iter()
            .filter_map(|t| {
                let func = t.get("function").or(Some(t))?;
                let name = func.get("name")?.as_str()?;
                if name.is_empty() {
                    return None;
                }
                let desc = func
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let params = func
                    .get("parameters")
                    .or_else(|| func.get("input_schema"))
                    .cloned()
                    .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
                Some(json!({
                    "name": name,
                    "description": desc,
                    "input_schema": params,
                }))
            })
            .collect();
        if !cc_tools.is_empty() {
            params_obj.insert("tools".into(), json!(cc_tools));
        }
    }

    if let Some(tc) = val.get("tool_choice") {
        let translated = if let Some(s) = tc.as_str() {
            match s {
                "auto" => Some(json!({"type": "auto"})),
                "none" => Some(json!({"type": "none"})),
                "required" => Some(json!({"type": "any"})),
                _ => None,
            }
        } else if let Some(obj) = tc.as_object() {
            if obj.get("type").and_then(Value::as_str) == Some("function") {
                let name = obj
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if !name.is_empty() {
                    Some(json!({"type": "tool", "name": name}))
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        if let Some(choice) = translated {
            params_obj.insert("tool_choice".into(), choice);
        }
    }

    if let Some(max_tokens) = val
        .get("max_tokens")
        .or_else(|| val.get("max_completion_tokens"))
    {
        params_obj.insert("max_tokens".into(), max_tokens.clone());
    } else {
        params_obj.insert("max_tokens".into(), json!(64000));
    }

    if let Some(temp) = val.get("temperature") {
        params_obj.insert("temperature".into(), temp.clone());
    }

    if let Some(reasoning) = val.get("reasoning_effort") {
        params_obj.insert("reasoning_effort".into(), reasoning.clone());
    }

    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let thread_id = uuid::Uuid::new_v4().to_string();

    json!({
        "config": {
            "workingDir": "/",
            "date": today,
            "environment": "linux-x86_64, OpenProxy",
            "structure": [],
            "isGitRepo": false,
            "currentBranch": "",
            "mainBranch": "",
            "gitStatus": "",
            "recentCommits": []
        },
        "permissionMode": "standard",
        "memory": null,
        "taste": null,
        "skills": null,
        "params": params_obj,
        "threadId": thread_id,
    })
}

fn extract_content_string(msg: &Value) -> String {
    match msg.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => {
            let mut buf = String::new();
            for part in arr {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    if !buf.is_empty() {
                        buf.push('\n');
                    }
                    buf.push_str(text);
                }
            }
            buf
        }
        Some(Value::Null) | None => String::new(),
        Some(v) => v.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_commandcode_dynamic_version() {
        assert_eq!(
            get_commandcode_cli_version(),
            DEFAULT_COMMANDCODE_CLI_VERSION
        );
        set_commandcode_cli_version("2.0.0".into());
        assert_eq!(get_commandcode_cli_version(), "2.0.0");
        set_commandcode_cli_version(DEFAULT_COMMANDCODE_CLI_VERSION.into());
    }

    #[test]
    fn test_commandcode_wrap_request_body() {
        let adapter = CommandCodeGoAdapter::new();
        let body = json!({
            "model": "claude-sonnet-5",
            "messages": [
                { "role": "system", "content": "You are helpful." },
                { "role": "user", "content": "Hello world" }
            ],
            "temperature": 0.7
        });
        let bytes = bytes::Bytes::from(serde_json::to_vec(&body).unwrap());
        let target = openproxy_types::context::ResolvedTarget {
            target: openproxy_types::combos::ComboTarget {
                id: openproxy_types::ComboTargetId(1),
                combo_id: openproxy_types::ComboId(1),
                provider_id: openproxy_types::ProviderId::new("commandcodego"),
                account_id: None,
                model_row_id: Some(openproxy_types::ModelRowId(1)),
                sub_combo_id: None,
                priority_order: 0,
                weight: 100,
                active: true,
                rate_limit_scope: openproxy_types::providers::RateLimitScope::Account,
                cooldown_mode: None,
                cooldown_base_secs: None,
                cooldown_max_secs: None,
                cooldown_factor: None,
                thinking_effort: None,
            },
            model: openproxy_types::Model {
                row_id: openproxy_types::ModelRowId(1),
                provider_id: openproxy_types::ProviderId::new("commandcodego"),
                target_format: openproxy_types::TargetFormat::CommandCodeGo,
                discovered_at: openproxy_types::now_unix_secs_str().into_boxed_str(),
                expires_at: None,
                model_id: openproxy_types::ModelId::new("claude-sonnet-5"),
                display_name: None,
                context_length: None,
                max_output_tokens: None,
                model_type: "chat".into(),
                family: None,
                input_modalities_json: None,
                output_modalities_json: None,
                capabilities_json: None,
                timeout_overrides_json: None,
                active: true,
                last_test_status: None,
                last_test_at: None,
                custom: false,
                ..Default::default()
            },
            api_key: "dummy".to_string(),
            api_key_label: None,
            custom_meta: None,
        };

        let wrapped = adapter
            .wrap_request_body(
                bytes,
                TargetFormat::CommandCodeGo,
                &ModelId::new("claude-sonnet-5"),
                &target,
            )
            .unwrap();

        let v: Value = serde_json::from_slice(&wrapped).unwrap();
        assert!(v.get("config").is_some());
        assert!(v.get("threadId").is_some());
        let params = v.get("params").unwrap();
        assert_eq!(params["model"].as_str(), Some("claude-sonnet-5"));
        assert_eq!(params["system"].as_str(), Some("You are helpful."));
        assert_eq!(params["stream"].as_bool(), Some(true));
        assert_eq!(params["temperature"].as_f64(), Some(0.7));
    }

    #[test]
    fn test_commandcode_wrap_request_body_with_tool_calls_and_results() {
        let adapter = CommandCodeGoAdapter::new();
        let body = json!({
            "model": "claude-sonnet-5",
            "messages": [
                { "role": "system", "content": "System directive" },
                { "role": "user", "content": "Check files" },
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [
                        {
                            "id": "call_abc123",
                            "type": "function",
                            "function": {
                                "name": "list_files",
                                "arguments": "{\"path\":\"/root\"}"
                            }
                        }
                    ]
                },
                {
                    "role": "tool",
                    "tool_call_id": "call_abc123",
                    "content": "file1.txt\nfile2.txt"
                },
                { "role": "user", "content": "Now read file1" }
            ],
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "list_files",
                        "description": "List directory contents",
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "path": { "type": "string" }
                            }
                        }
                    }
                }
            ],
            "tool_choice": "auto"
        });
        let bytes = bytes::Bytes::from(serde_json::to_vec(&body).unwrap());
        let target = openproxy_types::context::ResolvedTarget {
            target: openproxy_types::combos::ComboTarget {
                id: openproxy_types::ComboTargetId(1),
                combo_id: openproxy_types::ComboId(1),
                provider_id: openproxy_types::ProviderId::new("commandcodego"),
                account_id: None,
                model_row_id: Some(openproxy_types::ModelRowId(1)),
                sub_combo_id: None,
                priority_order: 0,
                weight: 100,
                active: true,
                rate_limit_scope: openproxy_types::providers::RateLimitScope::Account,
                cooldown_mode: None,
                cooldown_base_secs: None,
                cooldown_max_secs: None,
                cooldown_factor: None,
                thinking_effort: None,
            },
            model: openproxy_types::Model {
                row_id: openproxy_types::ModelRowId(1),
                provider_id: openproxy_types::ProviderId::new("commandcodego"),
                target_format: openproxy_types::TargetFormat::CommandCodeGo,
                discovered_at: openproxy_types::now_unix_secs_str().into_boxed_str(),
                expires_at: None,
                model_id: openproxy_types::ModelId::new("claude-sonnet-5"),
                display_name: None,
                context_length: None,
                max_output_tokens: None,
                model_type: "chat".into(),
                family: None,
                input_modalities_json: None,
                output_modalities_json: None,
                capabilities_json: None,
                timeout_overrides_json: None,
                active: true,
                last_test_status: None,
                last_test_at: None,
                custom: false,
                ..Default::default()
            },
            api_key: "dummy".to_string(),
            api_key_label: None,
            custom_meta: None,
        };

        let wrapped = adapter
            .wrap_request_body(
                bytes,
                TargetFormat::CommandCodeGo,
                &ModelId::new("claude-sonnet-5"),
                &target,
            )
            .unwrap();

        let v: Value = serde_json::from_slice(&wrapped).unwrap();
        let params = v.get("params").unwrap();
        let msgs = params["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 4);

        // Turn 1: user
        assert_eq!(msgs[0]["role"], "user");
        let u1_blocks = msgs[0]["content"].as_array().unwrap();
        assert_eq!(u1_blocks[0]["type"], "text");
        assert_eq!(u1_blocks[0]["text"], "Check files");

        // Turn 2: assistant with tool-call
        assert_eq!(msgs[1]["role"], "assistant");
        let a_blocks = msgs[1]["content"].as_array().unwrap();
        assert_eq!(a_blocks[0]["type"], "tool-call");
        assert_eq!(a_blocks[0]["toolCallId"], "call_abc123");
        assert_eq!(a_blocks[0]["toolName"], "list_files");
        assert_eq!(a_blocks[0]["input"]["path"], "/root");

        // Turn 3: tool result
        assert_eq!(msgs[2]["role"], "tool");
        let t_blocks = msgs[2]["content"].as_array().unwrap();
        assert_eq!(t_blocks[0]["type"], "tool-result");
        assert_eq!(t_blocks[0]["toolCallId"], "call_abc123");
        assert_eq!(t_blocks[0]["toolName"], "list_files");
        assert_eq!(t_blocks[0]["output"]["type"], "text");
        assert_eq!(t_blocks[0]["output"]["value"], "file1.txt\nfile2.txt");

        // Turn 4: user text
        assert_eq!(msgs[3]["role"], "user");
        let u2_blocks = msgs[3]["content"].as_array().unwrap();
        assert_eq!(u2_blocks[0]["type"], "text");
        assert_eq!(u2_blocks[0]["text"], "Now read file1");

        // Tools: Anthropic format (no "type": "function")
        let tools = params["tools"].as_array().unwrap();
        assert_eq!(tools[0]["name"], "list_files");
        assert!(tools[0].get("type").is_none());
        assert!(tools[0].get("input_schema").is_some());
    }

    #[test]
    fn test_parse_commandcode_window_normalizes_to_percentages() {
        // 5-hour window: used 0.21 out of 3.0 cap -> 7%
        let win_5h = json!({
            "used": 0.21,
            "cap": 3.0,
            "exceeded": false,
            "resetAt": 0
        });
        let (used, limit, reset) = parse_commandcode_window(Some(&win_5h));
        assert_eq!(used, Some(7));
        assert_eq!(limit, Some(100));
        assert_eq!(reset, None);

        // Weekly window: used 0.196657126 out of 6.0 cap -> 3%
        let win_weekly = json!({
            "used": 0.196657126,
            "cap": 6.0,
            "exceeded": false,
            "resetAt": 1789984055925i64
        });
        let (w_used, w_limit, w_reset) = parse_commandcode_window(Some(&win_weekly));
        assert_eq!(w_used, Some(3));
        assert_eq!(w_limit, Some(100));
        assert_eq!(w_reset.as_deref(), Some("1789984055925"));

        // Exceeded window clamps to 100%
        let win_exceeded = json!({
            "used": 3.5,
            "cap": 3.0,
            "exceeded": true,
            "resetAt": "1789984000000"
        });
        let (e_used, e_limit, _) = parse_commandcode_window(Some(&win_exceeded));
        assert_eq!(e_used, Some(100));
        assert_eq!(e_limit, Some(100));

        // Zero cap returns None
        let win_zero = json!({ "used": 0.0, "cap": 0.0 });
        let (z_used, z_limit, _) = parse_commandcode_window(Some(&win_zero));
        assert_eq!(z_used, None);
        assert_eq!(z_limit, None);
    }

    #[test]
    fn test_humanize_commandcode_plan() {
        assert_eq!(
            humanize_commandcode_plan(Some("individual-go"), Some("Monthly: 2% · resets Oct 14")),
            "Command Code · Go · Monthly: 2% · resets Oct 14"
        );

        assert_eq!(
            humanize_commandcode_plan(Some("individual-pro"), None),
            "Command Code · Pro"
        );

        assert_eq!(humanize_commandcode_plan(None, None), "Command Code");
    }
}
