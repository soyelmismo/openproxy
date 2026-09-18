use super::{
    AdapterAuthType, AdapterFormat, Arc, CancellationToken, CoreError, DiscoveredModel, ModelId,
    ProviderAdapterConfig, ProviderId, Result, TargetFormat, TimeoutProfile, UpstreamClient,
    UpstreamRequest, fetch_openai_models,
};

static DYNAMIC_USER_AGENT: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);
static DYNAMIC_ANTHROPIC_VERSION: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);

/// Set dynamic User-Agent override for MiniMax in memory at runtime.
pub fn set_dynamic_user_agent(ua: impl Into<String>) {
    if let Ok(mut lock) = DYNAMIC_USER_AGENT.write() {
        *lock = Some(ua.into());
    }
}

/// Set dynamic Anthropic-Version override for MiniMax in memory at runtime.
pub fn set_dynamic_anthropic_version(ver: impl Into<String>) {
    if let Ok(mut lock) = DYNAMIC_ANTHROPIC_VERSION.write() {
        *lock = Some(ver.into());
    }
}

/// Resolve current MiniMax User-Agent string.
pub fn current_user_agent() -> String {
    if let Ok(lock) = DYNAMIC_USER_AGENT.read()
        && let Some(ref ua) = *lock
    {
        return ua.clone();
    }
    if let Ok(env_ua) = std::env::var("OPENPROXY_MINIMAX_USER_AGENT")
        && !env_ua.is_empty()
    {
        return env_ua;
    }
    "MiniMaxAgent".to_string()
}

/// Resolve current MiniMax Anthropic-Version string.
pub fn current_anthropic_version() -> String {
    if let Ok(lock) = DYNAMIC_ANTHROPIC_VERSION.read()
        && let Some(ref ver) = *lock
    {
        return ver.clone();
    }
    if let Ok(env_ver) = std::env::var("OPENPROXY_MINIMAX_ANTHROPIC_VERSION")
        && !env_ver.is_empty()
    {
        return env_ver;
    }
    "2023-06-01".to_string()
}

static DYNAMIC_EXTRA_HEADERS: std::sync::RwLock<std::collections::BTreeMap<String, String>> =
    std::sync::RwLock::new(std::collections::BTreeMap::new());

/// Set dynamic extra header override for MiniMax in memory at runtime without recompiling.
pub fn set_dynamic_extra_header(key: impl Into<String>, val: impl Into<String>) {
    if let Ok(mut lock) = DYNAMIC_EXTRA_HEADERS.write() {
        lock.insert(key.into(), val.into());
    }
}

/// Reset dynamic in-memory overrides for MiniMax (useful for tests and cleanup).
pub fn reset_dynamic_overrides() {
    if let Ok(mut lock) = DYNAMIC_USER_AGENT.write() {
        *lock = None;
    }
    if let Ok(mut lock) = DYNAMIC_ANTHROPIC_VERSION.write() {
        *lock = None;
    }
    if let Ok(mut lock) = DYNAMIC_EXTRA_HEADERS.write() {
        lock.clear();
    }
}

#[cfg(test)]
pub(crate) static MINIMAX_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

static EXTRA_HEADERS: std::sync::LazyLock<Vec<(String, String)>> =
    std::sync::LazyLock::new(|| crate::spoofer::parse_env_extra_headers("OPENPROXY_MINIMAX_EXTRA_HEADERS"));

/// Adapter for MiniMax's Anthropic-compatible coding endpoint.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct MiniMaxAdapter {
    pub(crate) config: ProviderAdapterConfig,
}

impl MiniMaxAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("minimax"),
                name: "MiniMax Coding".into(),
                anonymous_fallback: false,
                rate_limit_scope: "account".into(),
                base_url: "https://agent.minimax.io".into(),
                auth_type: AdapterAuthType::OAuth,
                format: AdapterFormat::Anthropic,
                extra_headers: vec![],
            },
        }
    }
}

use crate::adapters::ProviderAdapter;
crate::adapters::derive_default_from_new!(MiniMaxAdapter);

impl ProviderAdapter for MiniMaxAdapter {
    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn config_mut(&mut self) -> Option<&mut ProviderAdapterConfig> {
        Some(&mut self.config)
    }

    fn models_dev_canonical_ids(&self) -> &'static [&'static str] {
        &["minimax"]
    }

    fn metadata(&self) -> openproxy_types::ProviderMetadata {
        let mut meta = openproxy_types::ProviderMetadata::custom_default();
        meta.built_in = true;
        meta.deletable = false;
        meta.supports_quota = true;
        meta.quota_refresh_supported = true;
        meta.requires_oauth = false;
        meta
    }

    fn build_chat_url(&self, _target_format: TargetFormat, _model: &ModelId) -> String {
        let base = self.config.base_url.trim_end_matches('/');
        if base.contains("agent.minimax") {
            if base.ends_with("/messages") {
                base.to_string()
            } else if base.ends_with("/v1") {
                format!("{base}/messages")
            } else {
                format!("{base}/mavis/api/v1/llm/v1/messages")
            }
        } else if base.ends_with("/messages") {
            base.to_string()
        } else if base.ends_with("/anthropic/v1") {
            format!("{base}/messages?beta=true")
        } else {
            format!("{base}/anthropic/v1/messages?beta=true")
        }
    }

    fn build_headers(
        &self,
        api_key: &str,
        _target_format: TargetFormat,
        _model: &ModelId,
    ) -> Vec<(String, String)> {
        let mut headers: Vec<(String, String)> =
            Vec::with_capacity(8 + self.config.extra_headers.len());
        let trimmed = api_key.trim();
        if trimmed.starts_with("sk-") {
            headers.push(("x-api-key".into(), trimmed.to_string()));
            headers.push(("Authorization".into(), format!("Bearer {trimmed}")));
        } else if !trimmed.is_empty() {
            headers.push(("Authorization".into(), format!("Bearer {trimmed}")));
        }
        headers.push(("Content-Type".into(), "application/json".into()));
        headers.push(("User-Agent".into(), current_user_agent()));
        headers.push(("Anthropic-Version".into(), current_anthropic_version()));
        headers.push(("X-Mavis-Agent-Id".into(), "main".into()));
        headers.push(("X-Mavis-Timezone-Offset".into(), "0".into()));
        headers.push((
            "X-Mavis-Session-Id".into(),
            format!("session_{}", uuid::Uuid::new_v4().simple()),
        ));

        crate::spoofer::merge_header_refs(&mut headers, &*EXTRA_HEADERS);
        if let Ok(lock) = DYNAMIC_EXTRA_HEADERS.read() {
            for (k, v) in lock.iter() {
                crate::spoofer::upsert_header(&mut headers, k, v.clone());
            }
        }
        crate::spoofer::merge_header_refs(&mut headers, &self.config.extra_headers);
        headers
    }

    fn models_url(&self) -> Option<String> {
        Some("https://api.minimax.io/v1/models".to_string())
    }

    async fn fetch_models(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        let trimmed = api_key.trim();
        if !trimmed.starts_with("sk-") {
            return Ok(minimax_builtin_models());
        }

        let url = self.models_url().ok_or_else(|| {
            CoreError::Internal("minimax: models_url is None (impossible)".into())
        })?;

        match fetch_openai_models(
            &url,
            upstream_client,
            trimmed,
            "minimax",
            openproxy_types::TargetFormat::Anthropic,
        )
        .await
        {
            Ok(models) if !models.is_empty() => Ok(models),
            Ok(_) | Err(_) => Ok(minimax_builtin_models()),
        }
    }

    async fn fetch_quota(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
        access_token: Option<&str>,
        provider_specific: Option<&str>,
    ) -> Option<Result<openproxy_types::AccountQuota>> {
        Some(
            self.fetch_minimax_quota_unified(
                upstream_client,
                api_key,
                access_token,
                provider_specific,
            )
            .await,
        )
    }
}

impl MiniMaxAdapter {
    async fn fetch_minimax_quota_unified(
        &self,
        upstream: &Arc<UpstreamClient>,
        api_key: &str,
        access_token: Option<&str>,
        provider_specific: Option<&str>,
    ) -> Result<openproxy_types::AccountQuota> {
        let token = access_token.map(str::trim).filter(|t| !t.is_empty());
        if let Some(token) = token {
            let (op_group_id, region, tier) = parse_minimax_meta(provider_specific);
            let platform_origin = match region.as_deref() {
                Some("cn") => "https://platform.minimaxi.com",
                _ => "https://platform.minimax.io",
            };
            let url = format!("{platform_origin}/v1/api/openplatform/coding_plan/remains");

            let mut req = UpstreamRequest::get(&url);
            if let Ok(val) = http::HeaderValue::from_str(&format!("Bearer {token}")) {
                req.headers.insert(http::header::AUTHORIZATION, val);
            }
            if let Some(ref gid) = op_group_id
                && let Ok(val) = http::HeaderValue::from_str(gid)
            {
                req.headers.insert(http::HeaderName::from_static("x-group-id"), val);
            }
            req.headers.insert(
                http::header::ACCEPT,
                http::HeaderValue::from_static("application/json"),
            );

            let cancel = CancellationToken::new();
            let response = upstream
                .call(req, TimeoutProfile::Quota, cancel)
                .await
                .map_err(|e| e.to_core_error(&url))?;

            if !response.status.is_success() {
                return Err(CoreError::UpstreamConnection(format!(
                    "{url}: status {}",
                    response.status.as_u16()
                )));
            }

            let body_bytes = response.collect().await.map_err(|e| e.to_core_error(&url))?;
            let json: serde_json::Value = serde_json::from_slice(&body_bytes)
                .map_err(|e| CoreError::Parse(format!("{url}: {e}")))?;

            let mut quota = parse_minimax_quota(&json, &url)?;
            if quota.plan_name.is_none() && tier.is_some() {
                quota.plan_name = tier;
            }
            return Ok(quota);
        }

        self.fetch_minimax_quota_local(upstream, api_key).await
    }

    async fn fetch_minimax_quota_local(
        &self,
        upstream: &Arc<UpstreamClient>,
        api_key: &str,
    ) -> Result<openproxy_types::AccountQuota> {
        let urls = [
            "https://api.minimax.io/v1/token_plan/remains",
            "https://api.minimax.io/v1/coding_plan/remains",
        ];

        let mut last_err: Option<String> = None;
        for url in &urls {
            match self
                .fetch_minimax_from_url_local(upstream, api_key, url)
                .await
            {
                Ok(quota) => return Ok(quota),
                Err(e) => last_err = Some(e.to_string()),
            }
        }

        Ok(openproxy_types::AccountQuota {
            session_used: None,
            session_limit: None,
            session_reset_at: None,
            weekly_used: None,
            weekly_limit: None,
            weekly_reset_at: None,
            plan_name: None,
            last_fetched_at: openproxy_types::now_unix_secs_str(),
            fetch_error: Some(last_err.unwrap_or_else(|| "unknown error".into())),
            model_details: None,
        })
    }

    async fn fetch_minimax_from_url_local(
        &self,
        upstream: &Arc<UpstreamClient>,
        api_key: &str,
        url: &str,
    ) -> Result<openproxy_types::AccountQuota> {
        let body = send_minimax_quota_request(upstream, api_key, url).await?;
        let json: serde_json::Value =
            serde_json::from_slice(&body).map_err(|e| CoreError::Parse(format!("{url}: {e}")))?;
        parse_minimax_quota(&json, url)
    }
}

async fn send_minimax_quota_request(
    upstream: &Arc<UpstreamClient>,
    api_key: &str,
    url: &str,
) -> Result<bytes::Bytes> {
    let mut req = UpstreamRequest::get(url);
    if let Ok(v) = http::HeaderValue::from_str(&format!("Bearer {api_key}")) {
        req.headers.insert(http::header::AUTHORIZATION, v);
    }
    let cancel = CancellationToken::new();
    let response = upstream
        .call(req, TimeoutProfile::Quota, cancel)
        .await
        .map_err(|e| e.to_core_error(url))?;

    if !response.status.is_success() {
        return Err(CoreError::UpstreamConnection(format!(
            "{url}: status {}",
            response.status.as_u16()
        )));
    }

    response.collect().await.map_err(|e| e.to_core_error(url))
}

fn is_preferred_minimax_model(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "general" || lower == "coding-plan"
}

fn is_secondary_minimax_model(name: &str) -> bool {
    name.to_ascii_lowercase().starts_with("minimax-m")
}

fn select_minimax_quota_entry(entries: &[serde_json::Value]) -> Option<&serde_json::Value> {
    entries
        .iter()
        .find(|e| {
            let name = e.get("model_name").and_then(|v| v.as_str()).unwrap_or("");
            is_preferred_minimax_model(name)
        })
        .or_else(|| {
            entries.iter().find(|e| {
                let name = e.get("model_name").and_then(|v| v.as_str()).unwrap_or("");
                is_secondary_minimax_model(name)
            })
        })
        .or_else(|| entries.first())
}

fn parse_minimax_quota(
    body: &serde_json::Value,
    url: &str,
) -> Result<openproxy_types::AccountQuota> {
    if let Some(base_resp) = body.get("base_resp") {
        let code = base_resp
            .get("status_code")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        if code != 0 {
            if code == 2062 {
                return Ok(openproxy_types::AccountQuota {
                    session_used: None,
                    session_limit: None,
                    session_reset_at: None,
                    weekly_used: None,
                    weekly_limit: None,
                    weekly_reset_at: None,
                    plan_name: Some("Free".to_string()),
                    last_fetched_at: openproxy_types::now_unix_secs_str(),
                    fetch_error: None,
                    model_details: None,
                });
            }
            let msg = base_resp
                .get("status_msg")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Err(CoreError::UpstreamConnection(format!(
                "{url}: [{code}] {msg}"
            )));
        }
    }

    let plan_name = body
        .get("plan_name")
        .and_then(|v| v.as_str())
        .map(std::string::ToString::to_string);

    let entries = body
        .get("model_remains")
        .and_then(|v| v.as_array())
        .ok_or_else(|| CoreError::Parse(format!("{url}: missing 'model_remains' array")))?;

    if entries.is_empty() {
        return Err(CoreError::Parse(format!("{url}: empty model_remains")));
    }

    let target = select_minimax_quota_entry(entries)
        .ok_or_else(|| CoreError::Parse(format!("{url}: no valid model entry")))?;

    let (session_used, session_limit) = extract_used_limit(
        target,
        "current_interval_usage_count",
        "current_interval_total_count",
        "current_interval_remaining_percent",
    );
    let (weekly_used, weekly_limit) = extract_used_limit(
        target,
        "current_weekly_usage_count",
        "current_weekly_total_count",
        "current_weekly_remaining_percent",
    );

    let session_reset_at = extract_reset_timestamp(target, "end_time", "remains_time");
    let weekly_reset_at = extract_reset_timestamp(target, "weekly_end_time", "weekly_remains_time");

    Ok(openproxy_types::AccountQuota {
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

fn extract_reset_timestamp(
    entry: &serde_json::Value,
    end_time_key: &str,
    remains_time_key: &str,
) -> Option<String> {
    if let Some(end_ms) = entry
        .get(end_time_key)
        .and_then(serde_json::Value::as_i64)
        .filter(|&v| v > 0)
    {
        return ms_epoch_to_secs_str(end_ms);
    }
    if let Some(remains_ms) = entry
        .get(remains_time_key)
        .and_then(serde_json::Value::as_i64)
        .filter(|&v| v > 0)
    {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let delta_secs = (remains_ms / 1000) as u64;
        return Some((now_secs + delta_secs).to_string());
    }
    None
}

fn extract_used_limit(
    entry: &serde_json::Value,
    used_count_key: &str,
    limit_count_key: &str,
    remaining_pct_key: &str,
) -> (Option<i64>, Option<i64>) {
    let used = entry
        .get(used_count_key)
        .and_then(serde_json::Value::as_i64);
    let limit = entry
        .get(limit_count_key)
        .and_then(serde_json::Value::as_i64);
    if let (Some(u), Some(l)) = (used, limit)
        && l > 0
    {
        return (Some(u), Some(l));
    }

    let remaining = entry
        .get(remaining_pct_key)
        .and_then(serde_json::Value::as_i64);
    if let Some(rp) = remaining
        && (0..=100).contains(&rp)
    {
        let used_calc = (100 - rp).max(0);
        return (Some(used_calc), Some(100));
    }

    (None, None)
}

fn ms_epoch_to_secs_str(ms: i64) -> Option<String> {
    let secs = ms.checked_div(1000)?;
    Some(secs.to_string())
}

fn parse_minimax_meta(
    provider_specific: Option<&str>,
) -> (Option<String>, Option<String>, Option<String>) {
    let Some(raw) = provider_specific else {
        return (None, None, None);
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(raw) else {
        return (None, None, None);
    };
    let op_group = json
        .get("op_group_id")
        .and_then(serde_json::Value::as_str)
        .map(std::string::ToString::to_string);
    let region = json
        .get("region")
        .and_then(serde_json::Value::as_str)
        .map(std::string::ToString::to_string);
    let tier = json
        .get("token_plan_tier")
        .and_then(serde_json::Value::as_str)
        .map(std::string::ToString::to_string);
    (op_group, region, tier)
}

pub fn minimax_builtin_models() -> Vec<DiscoveredModel> {
    use crate::adapters::discovery::build_discovered_model_full;
    vec![
        build_discovered_model_full(
            "MiniMax-M3".into(),
            Some("MiniMax-M3".into()),
            TargetFormat::Anthropic,
            Some(1_000_000),
            Some(128_000),
        ),
        build_discovered_model_full(
            "MiniMax-M2.7-highspeed".into(),
            Some("MiniMax-M2.7-highspeed".into()),
            TargetFormat::Anthropic,
            Some(200_000),
            Some(128_000),
        ),
        build_discovered_model_full(
            "MiniMax-M2.7".into(),
            Some("MiniMax-M2.7".into()),
            TargetFormat::Anthropic,
            Some(200_000),
            Some(128_000),
        ),
        build_discovered_model_full(
            "minimax-m2.1".into(),
            Some("MiniMax-M2.1".into()),
            TargetFormat::Anthropic,
            Some(200_000),
            Some(128_000),
        ),
        build_discovered_model_full(
            "MiniMax-M2".into(),
            Some("MiniMax-M2".into()),
            TargetFormat::Anthropic,
            Some(200_000),
            Some(128_000),
        ),
    ]
}

#[cfg(test)]
mod tests;
