pub use super::kiro::{
    KiroDeframer, KiroEventType, KiroFrameEvent, KiroRequest, build_kiro_request,
    fold_kiro_body_to_openai_response,
};
use super::{
    AdapterAuthType, AdapterFormat, Arc, CancellationToken, CoreError, DiscoveredModel, ModelId,
    ProviderAdapter, ProviderAdapterConfig, ProviderId, Result, TargetFormat, TimeoutProfile,
    UpstreamClient, UpstreamRequest,
};
use crate::spoofer::KiroSpoofer;
use openproxy_types::ResultExt;
use std::sync::LazyLock;

static REGION_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| openproxy_types::static_regex!(r"[a-z]{2}-[a-z]+-[0-9]"));

// Kiro AI (AWS CodeWhisperer)

/// Adapter for Kiro AI (AWS CodeWhisperer).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct KiroAdapter {
    config: ProviderAdapterConfig,
}

impl KiroAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("kiro"),
                name: "Kiro AI".into(),
                anonymous_fallback: false,
                rate_limit_scope: "account".into(),
                base_url: "https://codewhisperer.us-east-1.amazonaws.com".into(),
                auth_type: AdapterAuthType::OAuth,
                format: AdapterFormat::Openai,
                extra_headers: vec![],
            },
        }
    }

    fn map_kiro_discovered_model(item: &serde_json::Value) -> Option<DiscoveredModel> {
        let model_id_str = item
            .get("modelId")
            .and_then(|v| v.as_str())
            .or_else(|| item.get("id").and_then(|v| v.as_str()))?;
        let display_name_str = item
            .get("modelName")
            .and_then(|v| v.as_str())
            .or_else(|| item.get("name").and_then(|v| v.as_str()))
            .unwrap_or(model_id_str);

        let caps = openproxy_types::ModelCapabilities {
            vision: Some(true),
            tool_calling: Some(true),
            reasoning: Some(true),
            thinking: Some(true),
            ..Default::default()
        };

        let token_limits = item.get("tokenLimits");
        let context_length = token_limits
            .and_then(|v| v.get("maxInputTokens"))
            .and_then(|v| v.as_i64())
            .or(Some(200_000));
        let max_output_tokens = token_limits
            .and_then(|v| v.get("maxOutputTokens"))
            .and_then(|v| v.as_i64())
            .or(Some(64_000));

        let input_modalities = item
            .get("supportedInputTypes")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| t.as_str())
                    .map(|s| s.to_ascii_lowercase())
                    .collect()
            });

        Some(DiscoveredModel {
            model_id: ModelId::new(model_id_str),
            display_name: Some(display_name_str.to_string()),
            target_format: TargetFormat::Openai,
            context_length,
            max_output_tokens,
            input_modalities,
            output_modalities: None,
            model_type: Some("chat".to_string()),
            family: None,
            capabilities: Some(caps),
        })
    }

    fn parse_models_response(json: &serde_json::Value) -> Option<Vec<DiscoveredModel>> {
        let models_arr = json
            .get("models")
            .and_then(|v| v.as_array())
            .or_else(|| json.get("availableModels").and_then(|v| v.as_array()))?;

        let discovered: Vec<DiscoveredModel> = models_arr
            .iter()
            .filter_map(Self::map_kiro_discovered_model)
            .collect();

        (!discovered.is_empty()).then_some(discovered)
    }
}

crate::adapters::derive_default_from_new!(KiroAdapter);

fn extract_kiro_region(account_label: &str) -> String {
    if !account_label.is_empty()
        && let Some(m) = REGION_RE.find(account_label)
    {
        m.as_str().to_string()
    } else {
        "us-east-1".to_string()
    }
}

fn build_kiro_model_endpoints(region: &str) -> Vec<String> {
    let mut endpoints = Vec::new();
    if let Ok(host) = std::env::var("OPENPROXY_KIRO_HOST")
        && !host.trim().is_empty()
    {
        endpoints.push(format!(
            "{}/ListAvailableModels?origin=AI_EDITOR",
            host.trim().trim_end_matches('/')
        ));
    }
    if region == "us-east-1" {
        endpoints.push(
            "https://q.us-east-1.amazonaws.com/ListAvailableModels?origin=AI_EDITOR".to_string(),
        );
        endpoints.push(
            "https://codewhisperer.us-east-1.amazonaws.com/ListAvailableModels?origin=AI_EDITOR"
                .to_string(),
        );
    } else {
        endpoints.push(format!(
            "https://q.{region}.amazonaws.com/ListAvailableModels?origin=AI_EDITOR"
        ));
        endpoints.push(
            "https://q.us-east-1.amazonaws.com/ListAvailableModels?origin=AI_EDITOR".to_string(),
        );
    }
    endpoints
}

async fn try_fetch_models_from_endpoint(
    upstream_client: &Arc<UpstreamClient>,
    api_key: &str,
    endpoint: &str,
) -> Option<Vec<DiscoveredModel>> {
    let mut req = UpstreamRequest::get_json(endpoint);
    if let Ok(v) = crate::antigravity_headers::build_bearer_header(api_key) {
        req.headers.insert(http::header::AUTHORIZATION, v);
    }

    let cancel = CancellationToken::new();
    let resp = upstream_client
        .call(req, TimeoutProfile::ModelDiscovery, cancel)
        .await
        .ok()?;
    if !resp.status.is_success() {
        return None;
    }
    let body_bytes = resp.collect().await.ok()?;
    let json: serde_json::Value = serde_json::from_slice(&body_bytes).ok()?;
    KiroAdapter::parse_models_response(&json)
}

impl ProviderAdapter for KiroAdapter {
    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn metadata(&self) -> openproxy_types::ProviderMetadata {
        let mut meta = openproxy_types::ProviderMetadata {
            built_in: true,
            deletable: false,
            supports_quota: true,
            quota_refresh_supported: true,
            requires_oauth: true,
            oauth_refresh_lead_seconds: Some(300),
        };
        // Ensure legacy alias 'kiro' supports quota
        if self.id().as_str() == "kiro" {
            meta.supports_quota = true;
            meta.quota_refresh_supported = true;
        }
        meta
    }

    fn build_chat_url(&self, _target_format: TargetFormat, _model: &ModelId) -> String {
        format!("{}/generateAssistantResponse", self.config.base_url)
    }

    fn build_chat_url_for_account(
        &self,
        _target_format: TargetFormat,
        _model: &ModelId,
        account_label: &str,
    ) -> String {
        let mut region = "us-east-1".to_string();
        if !account_label.is_empty()
            && let Some(m) = REGION_RE.find(account_label)
        {
            region = m.as_str().to_string();
        }
        kiro_runtime_url(&region)
    }

    fn build_headers(
        &self,
        api_key: &str,
        _target_format: TargetFormat,
        _model: &ModelId,
    ) -> Vec<(String, String)> {
        super::build_spoofer_headers(
            self.build_auth_header(api_key),
            &KiroSpoofer,
            &self.config.extra_headers,
        )
    }

    fn wrap_request_body(
        &self,
        body: bytes::Bytes,
        _target_format: TargetFormat,
        _model: &ModelId,
        resolved_target: &openproxy_types::context::ResolvedTarget,
    ) -> std::result::Result<bytes::Bytes, openproxy_types::error::CoreError> {
        let req: openproxy_types::OpenAIRequest = serde_json::from_slice(&body).map_err(|e| {
            openproxy_types::error::CoreError::Validation(format!("Invalid OpenAI request: {e}"))
        })?;

        let profile_arn = resolved_target
            .custom_meta
            .as_ref()
            .and_then(|m| m.kiro_profile_arn.as_deref());

        let kiro_req = build_kiro_request(&req, profile_arn);
        let kiro_bytes = serde_json::to_vec(&kiro_req).map_err(|e| {
            openproxy_types::error::CoreError::Validation(format!(
                "Failed to serialize Kiro request: {e}"
            ))
        })?;

        Ok(kiro_bytes.into())
    }

    fn models_url(&self) -> Option<String> {
        None
    }

    async fn fetch_models(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        self.fetch_models_for_account(upstream_client, api_key, "")
            .await
    }

    async fn fetch_models_for_account(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
        account_label: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        if api_key.is_empty() {
            return Err(CoreError::Validation(
                "kiro_ai: api key is required to fetch models".into(),
            ));
        }

        let region = extract_kiro_region(account_label);
        let endpoints = build_kiro_model_endpoints(&region);

        for endpoint in &endpoints {
            if let Some(models) =
                try_fetch_models_from_endpoint(upstream_client, api_key, endpoint).await
                && !models.is_empty()
            {
                return Ok(models);
            }
        }

        Err(CoreError::UpstreamConnection(
            "kiro_ai: failed to fetch models from all endpoints".into(),
        ))
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
                self.fetch_kiro_quota_local(upstream_client, token, provider_specific)
                    .await,
            )
        } else {
            Some(Ok(openproxy_types::AccountQuota::with_error(
                "kiro requires OAuth access token",
            )))
        }
    }
}

pub fn kiro_runtime_url(region: &str) -> String {
    if let Ok(url) = std::env::var("OPENPROXY_KIRO_RUNTIME_URL")
        && !url.trim().is_empty()
    {
        return url.trim().to_string();
    }
    let region = if region.is_empty() {
        "us-east-1"
    } else {
        region
    };
    let host = if let Ok(host) = std::env::var("OPENPROXY_KIRO_HOST")
        && !host.trim().is_empty()
    {
        host.trim().trim_end_matches('/').to_string()
    } else if region == "us-east-1" {
        format!("https://codewhisperer.{region}.amazonaws.com")
    } else {
        format!("https://q.{region}.amazonaws.com")
    };
    format!("{host}/generateAssistantResponse")
}

fn parse_kiro_meta_config(provider_specific: Option<&str>) -> (String, Option<String>) {
    let mut region = "us-east-1".to_string();
    let mut profile_arn = None;

    if let Some(json_str) = provider_specific
        && let Ok(meta) = serde_json::from_str::<serde_json::Value>(json_str)
    {
        if let Some(r) = meta.get("region").and_then(|v| v.as_str())
            && !r.is_empty()
        {
            region = r.to_string();
        }
        if let Some(arn) = meta
            .get("profileArn")
            .or_else(|| meta.get("profile_arn"))
            .and_then(|v| v.as_str())
        {
            profile_arn = Some(arn.to_string());
        }
    }
    (region, profile_arn)
}

fn extract_profile_arn_from_json(value: &serde_json::Value, region: &str) -> Option<String> {
    let arr = value.get("profiles")?.as_array()?;
    let region_pattern = format!(":{region}:");
    let target = arr
        .iter()
        .find(|p| {
            p.get("arn")
                .or_else(|| p.get("profileArn"))
                .and_then(|v| v.as_str())
                .is_some_and(|s| s.contains(&region_pattern))
        })
        .or_else(|| arr.first())?;

    target
        .get("arn")
        .or_else(|| target.get("profileArn"))
        .and_then(|v| v.as_str())
        .map(std::string::ToString::to_string)
}

async fn discover_kiro_profile_arn(
    upstream: &Arc<UpstreamClient>,
    access_token: &str,
    base_url: &str,
    region: &str,
) -> Option<String> {
    let url = format!("{base_url}/");
    let mut req = UpstreamRequest::post_json(&url, bytes::Bytes::from(r#"{"maxResults":10}"#));
    if let Ok(v) = crate::antigravity_headers::build_bearer_header(access_token) {
        req.headers.insert(http::header::AUTHORIZATION, v);
    }
    req.headers.insert(
        http::header::HeaderName::from_static("x-amz-target"),
        http::HeaderValue::from_static("AmazonCodeWhispererService.ListAvailableProfiles"),
    );
    req.headers.insert(
        http::header::HeaderName::from_static("x-amz-user-agent"),
        http::HeaderValue::from_static("aws-sdk-js/3.0.0 kiro/0.1"),
    );

    let cancel = CancellationToken::new();
    let resp = match upstream.call(req, TimeoutProfile::OAuth, cancel).await {
        Ok(r) if r.status.is_success() => r,
        Ok(r) => {
            let status = r.status;
            let body_str =
                String::from_utf8_lossy(&r.collect().await.unwrap_or_default()).to_string();
            tracing::info!(status = %status, body = %body_str, "Kiro profile ARN discovery returned non-success; proceeding without profile ARN");
            return None;
        }
        Err(e) => {
            tracing::info!(error = %e, "kiro listAvailableProfiles network call failed; proceeding without profile ARN");
            return None;
        }
    };

    let body_bytes = resp.collect().await.ok()?;
    let value: serde_json::Value = serde_json::from_slice(&body_bytes).ok()?;
    extract_profile_arn_from_json(&value, region)
}

fn empty_kiro_quota() -> openproxy_types::AccountQuota {
    openproxy_types::AccountQuota::with_plan("Kiro")
}

fn build_kiro_usage_limits_request(
    base_url: &str,
    access_token: &str,
    profile_arn: Option<&str>,
) -> Option<UpstreamRequest> {
    let url = base_url.trim_end_matches('/').to_string();
    let mut payload = serde_json::json!({
        "origin": "AI_EDITOR",
        "resourceType": "AGENTIC_REQUEST"
    });
    if let Some(arn) = profile_arn {
        payload["profileArn"] = serde_json::json!(arn);
    }
    let body_bytes = match serde_json::to_vec(&payload) {
        Ok(b) => b,
        Err(e) => {
            tracing::info!(error = %e, "kiro GetUsageLimits serialize payload failed; returning empty quota");
            return None;
        }
    };

    let mut req = UpstreamRequest::post_json(&url, bytes::Bytes::from(body_bytes));
    req.headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/x-amz-json-1.0"),
    );
    if let Ok(v) = crate::antigravity_headers::build_bearer_header(access_token) {
        req.headers.insert(http::header::AUTHORIZATION, v);
    }
    req.headers.insert(
        http::header::HeaderName::from_static("x-amz-target"),
        http::HeaderValue::from_static("AmazonCodeWhispererService.GetUsageLimits"),
    );
    req.headers.insert(
        http::header::HeaderName::from_static("x-amz-user-agent"),
        http::HeaderValue::from_static("aws-sdk-js/1.0.0 KiroIDE"),
    );
    Some(req)
}

async fn fetch_kiro_usage_limits_json(
    upstream: &Arc<UpstreamClient>,
    access_token: &str,
    base_url: &str,
    profile_arn: Option<&str>,
) -> Result<Option<serde_json::Value>> {
    let Some(req) = build_kiro_usage_limits_request(base_url, access_token, profile_arn) else {
        return Ok(None);
    };

    let cancel = CancellationToken::new();
    let resp = match upstream.call(req, TimeoutProfile::OAuth, cancel).await {
        Ok(r) => r,
        Err(e) => {
            tracing::info!(error = %e, "kiro GetUsageLimits network call failed; returning empty quota without error");
            return Ok(None);
        }
    };

    if !resp.status.is_success() {
        let status = resp.status.as_u16();
        let body_str =
            String::from_utf8_lossy(&resp.collect().await.unwrap_or_default()).to_string();
        tracing::info!(status = status, body = %body_str, "Kiro GetUsageLimits returned non-success; returning empty quota");
        return Ok(None);
    }

    let resp_bytes = resp
        .collect()
        .await
        .ctx_upstream("kiro GetUsageLimits read")?;
    let data: serde_json::Value =
        serde_json::from_slice(&resp_bytes).ctx_parse("kiro GetUsageLimits parse")?;
    Ok(Some(data))
}

fn extract_agentic_request_limits(data: &serde_json::Value) -> (Option<i64>, Option<i64>) {
    let Some(arr) = data.get("usageBreakdownList").and_then(|v| v.as_array()) else {
        return (None, None);
    };

    for breakdown in arr {
        let resource_type = breakdown
            .get("resourceType")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if resource_type.eq_ignore_ascii_case("agentic_request")
            || resource_type.eq_ignore_ascii_case("credit")
            || resource_type.is_empty()
        {
            let current = breakdown
                .get("currentUsageWithPrecision")
                .or_else(|| breakdown.get("currentUsage"))
                .and_then(serde_json::Value::as_f64)
                .map(|v| v.round() as i64);
            let mut limit = breakdown
                .get("usageLimitWithPrecision")
                .or_else(|| breakdown.get("usageLimit"))
                .and_then(serde_json::Value::as_f64)
                .map(|v| v.round() as i64);

            // Sum up bonuses and overageCredits if present
            if let Some(bonuses) = breakdown.get("bonuses").and_then(|v| v.as_array()) {
                for bonus in bonuses {
                    if let Some(bonus_limit) = bonus
                        .get("usageLimitWithPrecision")
                        .or_else(|| bonus.get("usageLimit"))
                        .and_then(serde_json::Value::as_f64)
                    {
                        *limit.get_or_insert(0) += bonus_limit.round() as i64;
                    }
                }
            }
            if let Some(overages) = breakdown.get("overageCredits").and_then(|v| v.as_array()) {
                for overage in overages {
                    if let Some(overage_limit) = overage
                        .get("usageLimitWithPrecision")
                        .or_else(|| overage.get("usageLimit"))
                        .and_then(serde_json::Value::as_f64)
                    {
                        *limit.get_or_insert(0) += overage_limit.round() as i64;
                    }
                }
            }

            return (current, limit);
        }
    }
    (None, None)
}

impl KiroAdapter {
    async fn fetch_kiro_quota_local(
        &self,
        upstream: &Arc<UpstreamClient>,
        access_token: &str,
        provider_specific: Option<&str>,
    ) -> Result<openproxy_types::AccountQuota> {
        let (region, mut profile_arn) = parse_kiro_meta_config(provider_specific);
        let base_url = if region == "us-east-1" || region.is_empty() {
            "https://codewhisperer.us-east-1.amazonaws.com".to_string()
        } else {
            format!("https://q.{region}.amazonaws.com")
        };

        if profile_arn.is_none() {
            profile_arn =
                discover_kiro_profile_arn(upstream, access_token, &base_url, &region).await;
        }

        let Some(data) =
            fetch_kiro_usage_limits_json(upstream, access_token, &base_url, profile_arn.as_deref())
                .await?
        else {
            return Ok(empty_kiro_quota());
        };

        let reset_at = data
            .get("nextDateReset")
            .or_else(|| data.get("resetDate"))
            .and_then(|v| {
                if let Some(s) = v.as_str() {
                    Some(s.to_string())
                } else if let Some(n) = v.as_i64() {
                    chrono::DateTime::from_timestamp(n, 0).map(|dt| dt.to_rfc3339())
                } else if let Some(f) = v.as_f64() {
                    chrono::DateTime::from_timestamp(f as i64, 0).map(|dt| dt.to_rfc3339())
                } else {
                    None
                }
            });

        let (session_used, session_limit) = extract_agentic_request_limits(&data);

        let plan_name = data
            .get("subscriptionInfo")
            .and_then(|v| v.get("subscriptionTitle"))
            .and_then(|v| v.as_str())
            .map(std::string::ToString::to_string)
            .or_else(|| Some("Kiro".to_string()));

        Ok(openproxy_types::AccountQuota {
            session_used,
            session_limit,
            session_reset_at: reset_at,
            weekly_used: None,
            weekly_limit: None,
            weekly_reset_at: None,
            plan_name,
            last_fetched_at: openproxy_types::now_unix_secs_str(),
            fetch_error: None,
            pools: None,
            model_details: None,
        })
    }
}

#[cfg(test)]
#[path = "kiro_ai_tests.rs"]
mod tests;
