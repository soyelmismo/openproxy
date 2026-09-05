use super::{
    AdapterAuthType, AdapterFormat, Arc, CancellationToken, CoreError, DiscoveredModel, ModelId,
    ProviderAdapterConfig, ProviderId, Result, TargetFormat, TimeoutProfile, UpstreamClient,
    UpstreamRequest, fetch_openai_models,
};
// =====================================================================
// MiniMax (Coding)
// =====================================================================

/// Adapter for MiniMax's Anthropic-compatible coding endpoint.
///
/// The base URL is `https://api.minimax.io` (the bare host, no path). The
/// chat endpoint is reached by appending `/anthropic/v1/messages?beta=true`
/// at request time, and the model-discovery endpoint is reached by
/// appending `/v1/models`. Splitting the two paths this way is what lets
/// the same `base_url` serve both surfaces without one being a substring
/// of the other.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct MiniMaxAdapter {
    config: ProviderAdapterConfig,
}

impl MiniMaxAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("minimax"),
                name: "MiniMax Coding".into(),
                anonymous_fallback: false,
                rate_limit_scope: "account".into(),
                base_url: "https://api.minimax.io".into(),
                auth_type: AdapterAuthType::Bearer,
                format: AdapterFormat::Anthropic,
                extra_headers: vec![("Anthropic-Version".into(), "2023-06-01".into())],
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

    fn models_dev_canonical_ids(&self) -> &'static [&'static str] {
        &["minimax"]
    }

    fn metadata(&self) -> openproxy_types::ProviderMetadata {
        let mut meta = openproxy_types::ProviderMetadata::custom_default();
        meta.built_in = true;
        meta.deletable = false;
        meta.supports_quota = true;
        meta.quota_refresh_supported = true;
        meta
    }

    fn build_chat_url(&self, _target_format: TargetFormat, _model: &ModelId) -> String {
        // MiniMax exposes the Anthropic Messages API at /anthropic/v1/messages.
        // The `?beta=true` query parameter is required to enable the relevant
        // beta features (tool use, prompt caching, etc.).
        format!("{}/anthropic/v1/messages?beta=true", self.config.base_url)
    }

    fn models_url(&self) -> Option<String> {
        // MiniMax exposes its model catalogue at /v1/models (separate from
        // the /anthropic/v1/ chat surface). The auth scheme is the same
        // Bearer token.
        Some(format!("{}/v1/models", self.config.base_url))
    }

    async fn fetch_models(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        let url = self.models_url().ok_or_else(|| {
            CoreError::Internal("minimax: models_url is None (impossible)".into())
        })?;

        fetch_openai_models(
            &url,
            upstream_client,
            api_key,
            "minimax",
            openproxy_types::TargetFormat::Anthropic,
        )
        .await
    }

    async fn fetch_quota(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Option<Result<openproxy_types::AccountQuota>> {
        Some(
            self.fetch_minimax_quota_local(upstream_client, api_key)
                .await,
        )
    }
}

impl MiniMaxAdapter {
    async fn fetch_minimax_quota_local(
        &self,
        upstream: &Arc<UpstreamClient>,
        api_key: &str,
    ) -> Result<openproxy_types::AccountQuota> {
        let urls = [
            "https://api.minimax.io/v1/token_plan/remains",
            "https://api.minimax.io/v1/api/openplatform/coding_plan/remains",
        ];

        let mut last_err: Option<String> = None;
        for url in &urls {
            match self
                .fetch_minimax_from_url_local(upstream, api_key, url)
                .await
            {
                Ok(quota) => return Ok(quota),
                Err(e) => last_err = Some(format!("{url}: {e}")),
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
    if let Ok(v) = http::HeaderValue::from_str(&crate::adapters::format_bearer(api_key)) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimax_quota_with_end_times() {
        let json = serde_json::json!({
            "model_remains": [
                {
                    "start_time": 1787842800000_i64,
                    "end_time": 1787860800000_i64,
                    "remains_time": 11409757,
                    "current_interval_total_count": 0,
                    "current_interval_usage_count": 0,
                    "model_name": "general",
                    "current_weekly_total_count": 0,
                    "current_weekly_usage_count": 0,
                    "weekly_start_time": 1787529600000_i64,
                    "weekly_end_time": 1788134400000_i64,
                    "weekly_remains_time": 285009757,
                    "current_interval_status": 2,
                    "current_interval_remaining_percent": 0,
                    "current_weekly_status": 1,
                    "current_weekly_remaining_percent": 79
                }
            ],
            "base_resp": {
                "status_code": 0,
                "status_msg": "success"
            }
        });

        let quota = parse_minimax_quota(&json, "https://api.minimax.io/v1/token_plan/remains")
            .expect("quota parsed successfully");

        assert_eq!(quota.session_reset_at.as_deref(), Some("1787860800"));
        assert_eq!(quota.weekly_reset_at.as_deref(), Some("1788134400"));
        assert_eq!(quota.session_used, Some(100));
        assert_eq!(quota.session_limit, Some(100));
        assert_eq!(quota.weekly_used, Some(21));
        assert_eq!(quota.weekly_limit, Some(100));
    }

    #[test]
    fn parses_minimax_quota_fallback_remains_time() {
        let json = serde_json::json!({
            "model_remains": [
                {
                    "remains_time": 3600000,
                    "model_name": "coding-plan",
                    "current_interval_total_count": 50,
                    "current_interval_usage_count": 10
                }
            ]
        });

        let quota = parse_minimax_quota(&json, "https://api.minimax.io/v1/token_plan/remains")
            .expect("quota parsed successfully");

        assert_eq!(quota.session_used, Some(10));
        assert_eq!(quota.session_limit, Some(50));
        assert!(quota.session_reset_at.is_some());
    }
}
