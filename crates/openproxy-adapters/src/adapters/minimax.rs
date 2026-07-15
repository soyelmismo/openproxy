use super::*;
use crate::upstream::UpstreamError;
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

    fn metadata(&self) -> openproxy_types::ProviderMetadata {
        let mut meta = openproxy_types::ProviderMetadata::custom_default();
        meta.built_in = openproxy_types::is_builtin(self.id().as_str());
        meta.deletable = !openproxy_types::is_builtin(self.id().as_str());
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

    fn build_auth_header(&self, api_key: &str) -> Option<(String, String)> {
        Some(("Authorization".to_string(), format!("Bearer {}", api_key)))
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
        Some(self.fetch_minimax_quota_local(upstream_client, api_key).await)
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
            match self.fetch_minimax_from_url_local(upstream, api_key, url).await {
                Ok(quota) => return Ok(quota),
                Err(e) => last_err = Some(format!("{}: {}", url, e)),
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
        let mut req = UpstreamRequest::get(url);
        if let Ok(v) = http::HeaderValue::from_str(&format!("Bearer {api_key}")) {
            req.headers.insert(http::header::AUTHORIZATION, v);
        }
        let cancel = CancellationToken::new();
        let response = upstream
            .call(req, TimeoutProfile::Quota, cancel)
            .await
            .map_err(|e| match e {
                UpstreamError::Cancel => CoreError::ClientDisconnected,
                other => CoreError::UpstreamConnection(format!("{}: {}", url, other)),
            })?;

        if !response.status.is_success() {
            return Err(CoreError::UpstreamConnection(format!(
                "{}: status {}",
                url,
                response.status.as_u16()
            )));
        }

        let body = response
            .collect()
            .await
            .map_err(|e| CoreError::UpstreamConnection(format!("{}: {}", url, e)))?;

        let json: serde_json::Value =
            serde_json::from_slice(&body).map_err(|e| CoreError::Parse(format!("{}: {}", url, e)))?;
        parse_minimax_quota(&json, url)
    }
}

fn parse_minimax_quota(body: &serde_json::Value, url: &str) -> Result<openproxy_types::AccountQuota> {
    let plan_name = body
        .get("plan_name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let entries = body
        .get("model_remains")
        .and_then(|v| v.as_array())
        .ok_or_else(|| CoreError::Parse(format!("{}: missing 'model_remains' array", url)))?;

    if entries.is_empty() {
        return Err(CoreError::Parse(format!("{}: empty model_remains", url)));
    }

    let target = entries
        .iter()
        .find(|e| {
            let name = e.get("model_name").and_then(|v| v.as_str()).unwrap_or("");
            let lower = name.to_ascii_lowercase();
            lower == "general" || lower == "coding-plan"
        })
        .or_else(|| {
            entries.iter().find(|e| {
                let name = e.get("model_name").and_then(|v| v.as_str()).unwrap_or("");
                name.to_ascii_lowercase().starts_with("minimax-m")
            })
        })
        .or_else(|| entries.first())
        .expect("non-empty checked above");

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

    let session_reset_at = target
        .get("remains_time")
        .and_then(|v| v.as_i64())
        .and_then(ms_epoch_to_secs_str);
    let weekly_reset_at = target
        .get("weekly_remains_time")
        .and_then(|v| v.as_i64())
        .and_then(ms_epoch_to_secs_str);

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

fn extract_used_limit(
    entry: &serde_json::Value,
    used_count_key: &str,
    limit_count_key: &str,
    remaining_pct_key: &str,
) -> (Option<i64>, Option<i64>) {
    let used = entry.get(used_count_key).and_then(|v| v.as_i64());
    let limit = entry.get(limit_count_key).and_then(|v| v.as_i64());
    if let (Some(u), Some(l)) = (used, limit)
        && l > 0
    {
        return (Some(u), Some(l));
    }

    let remaining = entry.get(remaining_pct_key).and_then(|v| v.as_i64());
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
