use super::{
    AdapterAuthType, AdapterFormat, Arc, Bytes, CancellationToken, CoreError, DiscoveredModel,
    HeaderValue, ModelId, ProviderAdapter, ProviderAdapterConfig, ProviderId, Result, TargetFormat,
    TimeoutProfile, UpstreamClient, UpstreamRequest,
};
use crate::spoofer::{AntigravitySpoofer, ClientSpoofer};
use crate::upstream::UpstreamError;

crate::define_jump_map! {
    /// Jump map for Antigravity physical model translation.
    pub fn map_antigravity_physical_model(model: &str) -> &str {
        "gemini-3.1-pro-high" | "gemini-3.1-pro-medium" => "gemini-pro-agent",
        "gemini-3.5-flash-high" => "gemini-3-flash-agent",
        other => other,
    }
}

// =====================================================================
// Antigravity (Cloud Code)
// =====================================================================

/// Adapter for Google's Antigravity (Cloud Code) API.
///
/// Antigravity wraps Gemini requests in a Cloud Code envelope:
/// - Auth: `Authorization: Bearer <token>` (OAuth)
/// - Chat URL: `${base}/v1internal:generateContent`
/// - No model discovery endpoint (models are hardcoded)
#[derive(Clone, Debug, serde::Serialize)]
pub struct AntigravityAdapter {
    config: ProviderAdapterConfig,
}

impl<'de> serde::Deserialize<'de> for AntigravityAdapter {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        struct Helper {
            config: ProviderAdapterConfig,
        }
        let helper = Helper::deserialize(deserializer)?;
        Ok(Self {
            config: helper.config,
        })
    }
}

pub const DEFAULT_ANTIGRAVITY_BASE_URL: &str = "https://daily-cloudcode-pa.googleapis.com";

impl AntigravityAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("antigravity"),
                name: "Google Antigravity".into(),
                anonymous_fallback: false,
                rate_limit_scope: "account".into(),
                base_url: DEFAULT_ANTIGRAVITY_BASE_URL.into(),
                auth_type: AdapterAuthType::OAuth,
                format: AdapterFormat::Gemini,
                extra_headers: vec![],
            },
        }
    }

    fn extract_antigravity_model_capabilities(
        model_data: &serde_json::Value,
    ) -> openproxy_types::ModelCapabilities {
        let supports_thinking = model_data
            .get("supportsThinking")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let supports_images = model_data
            .get("supportsImages")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let tool_formatter_type = model_data
            .get("toolFormatterType")
            .and_then(|v| v.as_str())
            .is_some();
        let supports_cumulative_context = model_data
            .get("supportsCumulativeContext")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        openproxy_types::ModelCapabilities {
            vision: Some(supports_images),
            tool_calling: Some(tool_formatter_type || supports_cumulative_context),
            reasoning: Some(supports_thinking),
            thinking: Some(supports_thinking),
            attachment: Some(supports_images),
            structured_output: None,
            temperature: None,
        }
    }

    fn map_antigravity_discovered_model(
        model_id: &str,
        model_data: &serde_json::Value,
    ) -> DiscoveredModel {
        let display_name = model_data
            .get("displayName")
            .and_then(|d| d.as_str())
            .map(std::string::ToString::to_string);

        let context_length = model_data
            .get("maxTokens")
            .and_then(serde_json::Value::as_u64)
            .or_else(|| {
                model_data
                    .get("contextLength")
                    .and_then(serde_json::Value::as_u64)
            })
            .map(|v| v as i64);

        let max_output_tokens = model_data
            .get("maxOutputTokens")
            .and_then(serde_json::Value::as_u64)
            .map(|v| v as i64)
            .or(Some(8192));

        let capabilities = Self::extract_antigravity_model_capabilities(model_data);

        DiscoveredModel {
            model_id: ModelId::new(model_id),
            display_name,
            target_format: TargetFormat::Gemini,
            context_length,
            max_output_tokens,
            input_modalities: None,
            output_modalities: None,
            model_type: Some("chat".to_string()),
            family: None,
            capabilities: Some(capabilities),
        }
    }

    /// Parse fetchAvailableModels response into DiscoveredModel list.
    fn parse_models_response(body: &serde_json::Value) -> Option<Vec<DiscoveredModel>> {
        tracing::info!(
            "Antigravity fetchAvailableModels response: {}",
            serde_json::to_string(body).unwrap_or_else(|_| "{}".to_string())
        );
        let models_obj = body.get("models")?.as_object()?;
        let models: Vec<DiscoveredModel> = models_obj
            .iter()
            .map(|(k, v)| Self::map_antigravity_discovered_model(k, v))
            .collect();

        (!models.is_empty()).then_some(models)
    }
}

crate::adapters::derive_default_from_new!(AntigravityAdapter);

impl ProviderAdapter for AntigravityAdapter {
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
        // Ensure aliases like 'agy' support quota
        if self.id().as_str() == "antigravity" || self.id().as_str() == "agy" {
            meta.supports_quota = true;
            meta.quota_refresh_supported = true;
        }
        meta
    }

    fn build_chat_url(&self, _target_format: TargetFormat, _model: &ModelId) -> String {
        // Antigravity uses the Cloud Code endpoint; model goes in the body.
        // We MUST use streamGenerateContent?alt=sse because openproxy forces
        // is_streaming=true upstream and expects an SSE stream to parse.
        format!(
            "{}/v1internal:streamGenerateContent?alt=sse",
            self.config.base_url
        )
    }

    fn models_url(&self) -> Option<String> {
        // Antigravity does not expose a /models endpoint.
        None
    }

    fn format_request(
        &self,
        _target_format: TargetFormat,
        req: &openproxy_types::OpenAIRequest,
        _model: &ModelId,
        messages: &[openproxy_types::OpenAIMessage],
        _stream: bool,
    ) -> std::result::Result<bytes::Bytes, CoreError> {
        let gemini_req = crate::adapters::gemini::openai_to_gemini(req, messages);
        serde_json::to_vec(&gemini_req)
            .map(bytes::Bytes::from)
            .map_err(|e| CoreError::Parse(format!("serialize antigravity gemini request: {e}")))
    }

    fn translate_non_streaming_response(
        &self,
        _target_format: TargetFormat,
        response_body: serde_json::Value,
    ) -> std::result::Result<openproxy_types::OpenAIResponse, CoreError> {
        let gemini_resp: crate::adapters::gemini::GeminiResponse =
            <crate::adapters::gemini::GeminiResponse as serde::Deserialize>::deserialize(
                &response_body,
            )
            .map_err(|e| CoreError::Parse(format!("parse antigravity gemini response: {e}")))?;
        Ok(crate::adapters::gemini::gemini_to_openai(&gemini_resp))
    }

    fn build_headers(
        &self,
        api_key: &str,
        _target_format: TargetFormat,
        _model: &ModelId,
    ) -> Vec<(String, String)> {
        let mut headers_vec = Vec::with_capacity(10);
        headers_vec.push(("Authorization".into(), format!("Bearer {api_key}")));
        headers_vec.push(("Content-Type".into(), "application/json".into()));
        headers_vec.extend(AntigravitySpoofer::new().headers());

        for (k, v) in &self.config.extra_headers {
            headers_vec.push((k.clone(), v.clone()));
        }

        headers_vec
    }

    fn wrap_request_body(
        &self,
        body: bytes::Bytes,
        target_format: TargetFormat,
        model: &ModelId,
        resolved_target: &openproxy_types::context::ResolvedTarget,
    ) -> Result<bytes::Bytes> {
        if target_format == TargetFormat::Gemini {
            let mut json = serde_json::from_slice::<serde_json::Value>(&body)
                .map_err(|e| CoreError::Parse(format!("failed to parse gemini request: {e}")))?;
            let project = resolved_target
                .custom_meta
                .as_ref()
                .and_then(|m| m.antigravity_project.as_deref())
                .unwrap_or_default();
            let physical_model = map_antigravity_physical_model(model.as_str());

            if let Some(contents) = json.get_mut("contents") {
                inject_sentinel_thought_signatures(contents, physical_model);
            }

            let wrapped = serde_json::json!({
                "project": project,
                "model": physical_model,
                "requestType": "agent",
                "requestId": uuid::Uuid::new_v4().to_string(),
                "userAgent": "antigravity",
                "request": json,
                "enabledCreditTypes": ["GOOGLE_ONE_AI"]
            });
            let wrapped_bytes = bytes::Bytes::from(serde_json::to_vec(&wrapped).map_err(|e| {
                CoreError::Parse(format!("failed to serialize wrapped gemini request: {e}"))
            })?);
            tracing::info!(
                "Antigravity test payload: {}",
                serde_json::to_string(&wrapped).unwrap_or_else(|_| "{}".to_string())
            );
            return Ok(wrapped_bytes);
        }
        Ok(body)
    }

    async fn fetch_models(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        if api_key.is_empty() {
            return Err(CoreError::Validation(
                "antigravity: api key or access token is required to fetch models".into(),
            ));
        }

        let endpoints = [
            "https://daily-cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels",
            "https://cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels",
        ];

        for endpoint in endpoints {
            if let Some(models) =
                fetch_antigravity_models_from_endpoint(upstream_client, api_key, endpoint).await
                && !models.is_empty()
            {
                return Ok(models);
            }
        }

        Err(CoreError::UpstreamConnection(
            "antigravity: failed to fetch available models from all endpoints".into(),
        ))
    }

    async fn fetch_quota(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        _: &str,
        access_token: Option<&str>,
        _: Option<&str>,
    ) -> Option<Result<openproxy_types::AccountQuota>> {
        // Antigravity requires access_token to fetch quota
        if let Some(token) = access_token {
            Some(
                self.fetch_antigravity_quota_local(upstream_client, token)
                    .await,
            )
        } else {
            Some(Ok(openproxy_types::AccountQuota {
                session_used: None,
                session_limit: None,
                session_reset_at: None,
                weekly_used: None,
                weekly_limit: None,
                weekly_reset_at: None,
                plan_name: None,
                last_fetched_at: openproxy_types::now_unix_secs_str(),
                fetch_error: Some(
                    "missing access_token or project_id for antigravity quota".into(),
                ),
                model_details: None,
            }))
        }
    }
}

async fn fetch_antigravity_models_from_endpoint(
    upstream_client: &Arc<UpstreamClient>,
    api_key: &str,
    endpoint: &str,
) -> Option<Vec<DiscoveredModel>> {
    let mut req = UpstreamRequest::post_json(endpoint, Bytes::from_static(b"{}"));
    if let Ok(v) = HeaderValue::from_str(&format!("Bearer {api_key}")) {
        req.headers.insert(http::header::AUTHORIZATION, v);
    }
    req.headers.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    AntigravitySpoofer::new().apply_to_request(&mut req);

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
    AntigravityAdapter::parse_models_response(&json)
}

static PLAN_CACHE: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<String, String>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

fn merge_summary_into_models_quota(
    models_quota: &mut openproxy_types::AccountQuota,
    summary_quota: &openproxy_types::AccountQuota,
) {
    if summary_quota.weekly_used.is_some() {
        models_quota.weekly_used = summary_quota.weekly_used;
        models_quota.weekly_limit = summary_quota.weekly_limit;
        models_quota
            .weekly_reset_at
            .clone_from(&summary_quota.weekly_reset_at);
    }
    if models_quota.session_used.is_none() && summary_quota.session_used.is_some() {
        models_quota.session_used = summary_quota.session_used;
        models_quota.session_limit = summary_quota.session_limit;
        models_quota
            .session_reset_at
            .clone_from(&summary_quota.session_reset_at);
    }
}

fn resolve_final_plan_name(
    models_plan: Option<String>,
    summary_res: &Result<openproxy_types::AccountQuota>,
    plan_result: Option<String>,
) -> Option<String> {
    if let Some(plan) = plan_result {
        return Some(plan);
    }
    if models_plan.is_some() && models_plan.as_deref() != Some("Antigravity") {
        return models_plan;
    }
    if let Ok(summary_quota) = summary_res
        && let Some(summary_plan) = &summary_quota.plan_name
        && summary_plan != "Antigravity"
    {
        return Some(summary_plan.clone());
    }
    Some("Free".to_string())
}

async fn try_fetch_models_quota_endpoint(
    upstream: &Arc<UpstreamClient>,
    access_token: &str,
    endpoint: &str,
) -> Option<openproxy_types::AccountQuota> {
    let mut req = UpstreamRequest::post_json(endpoint, bytes::Bytes::from_static(b"{}"));
    if let Ok(v) = http::HeaderValue::from_str(&format!("Bearer {access_token}")) {
        req.headers.insert(http::header::AUTHORIZATION, v);
    }
    req.headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    crate::antigravity_headers::inject_antigravity_headers(&mut req.headers, None);

    let cancel = CancellationToken::new();
    let resp = upstream
        .call(req, TimeoutProfile::Quota, cancel)
        .await
        .ok()?;
    if !resp.status.is_success() {
        return None;
    }
    let body = resp.collect().await.ok()?;
    let json: serde_json::Value = serde_json::from_slice(&body).ok()?;
    parse_antigravity_models_response(&json).ok()
}

fn extract_tier_from_load_code_assist(json: &serde_json::Value) -> Option<&str> {
    let paid = json
        .pointer("/paidTier/name")
        .or_else(|| json.pointer("/paidTier/id"))
        .and_then(|v| v.as_str());
    if paid.is_some() {
        return paid;
    }
    let is_ineligible = json
        .pointer("/ineligibleTiers")
        .and_then(|v| v.as_array())
        .is_some_and(|a| !a.is_empty());

    if !is_ineligible {
        return json
            .pointer("/currentTier/name")
            .or_else(|| json.pointer("/currentTier/id"))
            .and_then(|v| v.as_str());
    }

    let allowed = json.pointer("/allowedTiers")?.as_array()?;
    for t in allowed {
        if t.get("isDefault")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            return t
                .get("name")
                .or_else(|| t.get("id"))
                .and_then(|v| v.as_str());
        }
    }
    None
}

const PLAN_KEYWORDS: &[(&[&str], &str)] = &[
    (&["ULTRA"], "Ultra"),
    (
        &["PRO", "PREMIUM", "GOOGLE_ONE", "ONE_AI", "GOOGLE ONE"],
        "Pro",
    ),
    (&["ENTERPRISE"], "Enterprise"),
    (&["BUSINESS", "STANDARD"], "Business"),
    (&["PLUS"], "Plus"),
    (&["LITE", "LIGHT"], "Lite"),
    (&["FREE", "INDIVIDUAL", "LEGACY"], "Free"),
];

fn classify_antigravity_plan_name(t: &str) -> String {
    let upper = t.to_uppercase();
    for (keywords, plan) in PLAN_KEYWORDS {
        if keywords.iter().any(|k| upper.contains(k)) {
            return (*plan).to_string();
        }
    }
    t.to_string()
}

async fn try_fetch_code_assist_plan(
    upstream: &Arc<UpstreamClient>,
    access_token: &str,
    endpoint: &str,
) -> Option<String> {
    let payload = bytes::Bytes::from_static(b"{\"metadata\": {\"ideType\": \"ANTIGRAVITY\"}}");
    let mut req = UpstreamRequest::post_json(endpoint, payload);
    if let Ok(v) = http::HeaderValue::from_str(&format!("Bearer {access_token}")) {
        req.headers.insert(http::header::AUTHORIZATION, v);
    }
    req.headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    crate::antigravity_headers::inject_antigravity_headers(&mut req.headers, None);

    let cancel = CancellationToken::new();
    let resp = upstream
        .call(req, TimeoutProfile::Quota, cancel)
        .await
        .ok()?;
    if !resp.status.is_success() {
        return None;
    }
    let body = resp.collect().await.ok()?;
    let json: serde_json::Value = serde_json::from_slice(&body).ok()?;
    let tier = extract_tier_from_load_code_assist(&json)?;
    Some(classify_antigravity_plan_name(tier))
}

impl AntigravityAdapter {
    async fn fetch_antigravity_quota_local(
        &self,
        upstream: &Arc<UpstreamClient>,
        access_token: &str,
    ) -> Result<openproxy_types::AccountQuota> {
        let (models_result, summary_result, plan_result) = tokio::join!(
            self.fetch_antigravity_models_quota_local(upstream, access_token),
            self.fetch_antigravity_user_quota_local(upstream, access_token),
            self.fetch_antigravity_subscription_plan_local(upstream, access_token),
        );

        match (models_result, summary_result) {
            (Ok(mut models_quota), summary_res) => {
                if let Ok(summary_quota) = &summary_res {
                    merge_summary_into_models_quota(&mut models_quota, summary_quota);
                }
                models_quota.plan_name =
                    resolve_final_plan_name(models_quota.plan_name, &summary_res, plan_result);
                Ok(models_quota)
            }
            (Err(_models_err), Ok(mut summary_quota)) => {
                let current_plan = summary_quota.plan_name.clone();
                summary_quota.plan_name =
                    resolve_final_plan_name(current_plan, &Ok(summary_quota.clone()), plan_result);
                Ok(summary_quota)
            }
            (Err(models_err), Err(_)) => Err(models_err),
        }
    }

    async fn fetch_antigravity_user_quota_local(
        &self,
        upstream: &Arc<UpstreamClient>,
        access_token: &str,
    ) -> Result<openproxy_types::AccountQuota> {
        let endpoints = [
            "https://daily-cloudcode-pa.sandbox.googleapis.com/v1internal:retrieveUserQuotaSummary",
            "https://daily-cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary",
            "https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary",
        ];

        let mut last_err: Option<CoreError> = None;
        for url in &endpoints {
            let mut req = UpstreamRequest::post_json(*url, bytes::Bytes::from_static(b"{}"));
            if let Ok(v) = http::HeaderValue::from_str(&format!("Bearer {access_token}")) {
                req.headers.insert(http::header::AUTHORIZATION, v);
            }
            req.headers.insert(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static("application/json"),
            );
            crate::antigravity_headers::inject_antigravity_headers(&mut req.headers, None);

            let cancel = CancellationToken::new();
            let response = match upstream.call(req, TimeoutProfile::Quota, cancel).await {
                Ok(r) => r,
                Err(UpstreamError::Cancel) => {
                    return Err(CoreError::Cancelled(
                        openproxy_types::CancelReason::ClientDisconnected,
                    ));
                }
                Err(e) => {
                    last_err = Some(CoreError::UpstreamConnection(e.to_string()));
                    continue;
                }
            };

            if !response.status.is_success() {
                last_err = Some(CoreError::UpstreamConnection(format!(
                    "{url}: status {}",
                    response.status.as_u16()
                )));
                continue;
            }

            let body = match response.collect().await {
                Ok(b) => b,
                Err(e) => {
                    last_err = Some(CoreError::UpstreamConnection(e.to_string()));
                    continue;
                }
            };

            let json: serde_json::Value = match serde_json::from_slice(&body) {
                Ok(j) => j,
                Err(e) => {
                    last_err = Some(CoreError::Parse(e.to_string()));
                    continue;
                }
            };

            match parse_antigravity_user_quota_summary(&json) {
                Ok(q) => return Ok(q),
                Err(e) => {
                    last_err = Some(e);
                }
            }
        }

        Err(last_err.unwrap_or_else(|| {
            CoreError::UpstreamConnection("all retrieveUserQuotaSummary endpoints failed".into())
        }))
    }

    async fn fetch_antigravity_models_quota_local(
        &self,
        upstream: &Arc<UpstreamClient>,
        access_token: &str,
    ) -> Result<openproxy_types::AccountQuota> {
        let endpoints = [
            "https://daily-cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels",
            "https://cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels",
        ];

        for endpoint in endpoints {
            if let Some(quota) =
                try_fetch_models_quota_endpoint(upstream, access_token, endpoint).await
            {
                return Ok(quota);
            }
        }

        Err(CoreError::UpstreamConnection(
            "all fetchAvailableModels endpoints failed".into(),
        ))
    }

    async fn fetch_antigravity_subscription_plan_local(
        &self,
        upstream: &Arc<UpstreamClient>,
        access_token: &str,
    ) -> Option<String> {
        if let Some(plan) = PLAN_CACHE.lock().get(access_token) {
            return Some(plan.clone());
        }

        let endpoints = [
            "https://daily-cloudcode-pa.sandbox.googleapis.com/v1internal:loadCodeAssist",
            "https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist",
        ];

        for endpoint in endpoints {
            if let Some(plan_name) =
                try_fetch_code_assist_plan(upstream, access_token, endpoint).await
            {
                PLAN_CACHE
                    .lock()
                    .insert(access_token.to_string(), plan_name.clone());
                return Some(plan_name);
            }
        }

        None
    }
}

fn parse_model_quota_detail(
    model_id: &str,
    model_data: &serde_json::Value,
) -> Option<openproxy_types::ModelQuotaDetail> {
    const NORMALIZED_BASE: i64 = 1000;
    let quota_info = model_data.get("quotaInfo")?;
    let reset_time = quota_info
        .get("resetTime")
        .and_then(|r| r.as_str())
        .map(String::from);

    let remaining_fraction = quota_info
        .get("remainingFraction")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or_else(|| if reset_time.is_some() { 0.0 } else { 1.0 });

    let is_unlimited = reset_time.is_none() && remaining_fraction >= 1.0;
    let remaining = (NORMALIZED_BASE as f64 * remaining_fraction) as i64;
    let used = if is_unlimited {
        0
    } else {
        NORMALIZED_BASE.saturating_sub(remaining)
    };

    Some(openproxy_types::ModelQuotaDetail {
        model_id: model_id.to_string(),
        session_used: used,
        session_limit: NORMALIZED_BASE,
        session_reset_at: reset_time,
        remaining_fraction,
    })
}

fn parse_antigravity_models_response(
    body: &serde_json::Value,
) -> Result<openproxy_types::AccountQuota> {
    let models = body
        .get("models")
        .and_then(|m| m.as_object())
        .ok_or_else(|| CoreError::Internal("missing 'models' in response".into()))?;

    let details: Vec<openproxy_types::ModelQuotaDetail> = models
        .iter()
        .filter_map(|(k, v)| parse_model_quota_detail(k, v))
        .collect();

    let worst = details
        .iter()
        .min_by(|a, b| {
            a.remaining_fraction
                .partial_cmp(&b.remaining_fraction)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .ok_or_else(|| CoreError::Internal("no quota info found in response".into()))?;

    Ok(openproxy_types::AccountQuota {
        plan_name: Some("Antigravity".to_string()),
        session_used: Some(worst.session_used),
        session_limit: Some(worst.session_limit),
        session_reset_at: worst.session_reset_at.clone(),
        weekly_used: None,
        weekly_limit: None,
        weekly_reset_at: None,
        last_fetched_at: openproxy_types::now_unix_secs_str(),
        fetch_error: None,
        model_details: Some(details),
    })
}

struct AntigravityQuotaBucket {
    plan_name: Option<String>,
    is_weekly: bool,
    used: i64,
    reset_at: Option<String>,
}

fn parse_quota_bucket(
    group_plan: Option<&str>,
    bucket: &serde_json::Value,
) -> AntigravityQuotaBucket {
    const NORMALIZED_BASE: i64 = 1000;
    let reset_time = bucket
        .get("resetTime")
        .and_then(|r| r.as_str())
        .map(String::from);
    let window = bucket.get("window").and_then(|w| w.as_str()).unwrap_or("");

    let remaining_fraction = bucket
        .get("remainingFraction")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or_else(|| if reset_time.is_some() { 0.0 } else { 1.0 });

    let is_unlimited = reset_time.is_none() && remaining_fraction >= 1.0;
    let remaining = (NORMALIZED_BASE as f64 * remaining_fraction) as i64;
    let used = if is_unlimited {
        0
    } else {
        NORMALIZED_BASE.saturating_sub(remaining)
    };

    let is_weekly = window.to_uppercase().contains("WEEK") || window.eq_ignore_ascii_case("WEEKLY");

    AntigravityQuotaBucket {
        plan_name: group_plan.map(std::string::ToString::to_string),
        is_weekly,
        used,
        reset_at: reset_time,
    }
}

fn extract_quota_buckets(
    groups: &[serde_json::Value],
) -> impl Iterator<Item = AntigravityQuotaBucket> + '_ {
    groups.iter().flat_map(|group| {
        let group_plan = group.get("displayName").and_then(|n| n.as_str());
        let buckets = group
            .get("buckets")
            .and_then(|b| b.as_array())
            .map_or(&[][..], |v| v.as_slice());

        buckets
            .iter()
            .map(move |bucket| parse_quota_bucket(group_plan, bucket))
    })
}

fn parse_antigravity_user_quota_summary(
    body: &serde_json::Value,
) -> Result<openproxy_types::AccountQuota> {
    const NORMALIZED_BASE: i64 = 1000;

    let groups = body
        .get("groups")
        .and_then(|g| g.as_array())
        .ok_or_else(|| {
            CoreError::Internal("missing 'groups' in retrieveUserQuotaSummary".into())
        })?;

    let mut weekly_used = None;
    let mut weekly_limit = None;
    let mut weekly_reset_at = None;
    let mut session_used = None;
    let mut session_limit = None;
    let mut session_reset_at = None;
    let mut plan_name = None;

    for bucket in extract_quota_buckets(groups) {
        if bucket.is_weekly {
            if weekly_used.is_none() {
                weekly_used = Some(bucket.used);
                weekly_limit = Some(NORMALIZED_BASE);
                weekly_reset_at = bucket.reset_at;
                plan_name = plan_name.or(bucket.plan_name);
            }
        } else if session_used.is_none() {
            session_used = Some(bucket.used);
            session_limit = Some(NORMALIZED_BASE);
            session_reset_at = bucket.reset_at;
            plan_name = plan_name.or(bucket.plan_name);
        }
    }

    if weekly_used.is_none() && session_used.is_none() {
        return Err(CoreError::Internal(
            "retrieveUserQuotaSummary: no usable buckets found".into(),
        ));
    }

    Ok(openproxy_types::AccountQuota {
        session_used,
        session_limit,
        session_reset_at,
        weekly_used,
        weekly_limit,
        weekly_reset_at,
        plan_name: Some(plan_name.unwrap_or_else(|| "Antigravity".to_string())),
        last_fetched_at: openproxy_types::now_unix_secs_str(),
        fetch_error: None,
        model_details: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_models_response_valid() {
        let body = json!({
            "models": {
                "gemini-1.5-pro": {
                    "displayName": "Gemini 1.5 Pro",
                    "maxTokens": 1000000,
                    "maxOutputTokens": 8192,
                    "supportsThinking": true,
                    "supportsImages": true,
                    "supportsCodeGeneration": true
                },
                "gemini-1.5-flash": {
                    "displayName": "Gemini 1.5 Flash",
                    "contextLength": 200000,
                    "supportsThinking": false
                }
            }
        });

        let models = AntigravityAdapter::parse_models_response(&body).expect("should parse");
        assert_eq!(models.len(), 2);

        let pro_model = models
            .iter()
            .find(|m| m.model_id.as_str() == "gemini-1.5-pro")
            .unwrap();
        assert_eq!(pro_model.display_name.as_deref(), Some("Gemini 1.5 Pro"));
        assert_eq!(pro_model.context_length, Some(1000000));
        assert_eq!(pro_model.max_output_tokens, Some(8192));
        let caps = pro_model.capabilities.as_ref().unwrap();
        assert_eq!(caps.thinking, Some(true));
        assert_eq!(caps.vision, Some(true));

        let flash_model = models
            .iter()
            .find(|m| m.model_id.as_str() == "gemini-1.5-flash")
            .unwrap();
        assert_eq!(
            flash_model.display_name.as_deref(),
            Some("Gemini 1.5 Flash")
        );
        assert_eq!(flash_model.context_length, Some(200000));
        assert_eq!(flash_model.max_output_tokens, Some(8192)); // Uses fallback 8192
        let flash_caps = flash_model.capabilities.as_ref().unwrap();
        assert_eq!(flash_caps.thinking, Some(false));
    }

    #[test]
    fn test_parse_models_response_invalid() {
        let body = json!({});
        assert!(AntigravityAdapter::parse_models_response(&body).is_none());
    }

    #[test]
    fn test_parse_antigravity_models_response_missing_models() {
        let body = json!({});
        let result = parse_antigravity_models_response(&body);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_antigravity_models_response_valid() {
        let body = json!({
            "models": {
                "model-a": {
                    "quotaInfo": {
                        "resetTime": "2023-01-01T00:00:00Z",
                        "remainingFraction": 0.5
                    }
                },
                "model-b": {
                    "quotaInfo": {
                        "resetTime": "2023-01-01T00:00:00Z",
                        "remainingFraction": 0.2
                    }
                }
            }
        });

        let result = parse_antigravity_models_response(&body).expect("should parse");
        assert_eq!(result.plan_name.unwrap(), "Antigravity");
        assert_eq!(result.session_limit.unwrap(), 1000);
        assert_eq!(result.session_used.unwrap(), 800);

        let details = result.model_details.unwrap();
        assert_eq!(details.len(), 2);
    }
}

pub const LOAD_CODE_ASSIST_URL: &str =
    "https://daily-cloudcode-pa.googleapis.com/v1internal:loadCodeAssist";
pub const ONBOARD_USER_URL: &str =
    "https://daily-cloudcode-pa.googleapis.com/v1internal:onboardUser";

/// Call `loadCodeAssist` and extract `projectId` (or `None` when
/// the user is not yet on-boarded).
pub async fn load_code_assist(
    upstream: &std::sync::Arc<crate::upstream::UpstreamClient>,
    access_token: &str,
    metadata: &serde_json::Value,
) -> std::result::Result<Option<String>, String> {
    let body = serde_json::json!({ "metadata": metadata });
    let body_bytes = serde_json::to_vec(&body)
        .map_err(|e| format!("antigravity loadCodeAssist serialize: {e}"))?;

    let mut req = crate::upstream::UpstreamRequest::post_json(
        LOAD_CODE_ASSIST_URL,
        bytes::Bytes::from(body_bytes),
    );
    if let Ok(v) = http::HeaderValue::from_str(&format!("Bearer {access_token}")) {
        req.headers.insert(http::header::AUTHORIZATION, v);
    }
    crate::antigravity_headers::inject_antigravity_headers(&mut req.headers, None);
    req.is_streaming = false;

    let cancel = crate::upstream::CancellationToken::new();
    let resp = upstream
        .call(req, crate::upstream::TimeoutProfile::OAuth, cancel)
        .await
        .map_err(|e| format!("antigravity loadCodeAssist: {e}"))?;

    if !resp.status.is_success() {
        let status = resp.status.as_u16();
        let body_str =
            String::from_utf8_lossy(&resp.collect().await.unwrap_or_default()).to_string();
        return Err(format!(
            "antigravity loadCodeAssist status {status}: {body_str}"
        ));
    }

    let body_bytes = resp
        .collect()
        .await
        .map_err(|e| format!("antigravity loadCodeAssist read: {e}"))?;

    let value: serde_json::Value = serde_json::from_slice(&body_bytes)
        .map_err(|e| format!("antigravity loadCodeAssist parse: {e}"))?;

    let project_id = value
        .get("cloudaicompanionProject")
        .and_then(|v| v.as_str())
        .map(std::string::ToString::to_string)
        .or_else(|| {
            value
                .get("cloudaicompanionProject")
                .and_then(|v| v.get("id"))
                .and_then(|v| v.as_str())
                .map(std::string::ToString::to_string)
        });

    Ok(project_id)
}

/// Call `onboardUser` and return `Ok(Some(project_id))` on success,
/// or `Ok(None)` when the server has not finished onboarding yet.
pub async fn onboard_user(
    upstream: &std::sync::Arc<crate::upstream::UpstreamClient>,
    access_token: &str,
    project_id: &str,
    metadata: &serde_json::Value,
) -> std::result::Result<Option<String>, String> {
    let body = serde_json::json!({
        "projectId": project_id,
        "metadata": metadata,
        "tier": "free-tier",
    });
    let body_bytes =
        serde_json::to_vec(&body).map_err(|e| format!("antigravity onboardUser serialize: {e}"))?;

    let mut req = crate::upstream::UpstreamRequest::post_json(
        ONBOARD_USER_URL,
        bytes::Bytes::from(body_bytes),
    );
    if let Ok(v) = http::HeaderValue::from_str(&format!("Bearer {access_token}")) {
        req.headers.insert(http::header::AUTHORIZATION, v);
    }
    crate::antigravity_headers::inject_antigravity_headers(&mut req.headers, None);
    req.is_streaming = false;

    let cancel = crate::upstream::CancellationToken::new();
    let resp = upstream
        .call(req, crate::upstream::TimeoutProfile::OAuth, cancel)
        .await
        .map_err(|e| format!("antigravity onboardUser: {e}"))?;

    if !resp.status.is_success() {
        let status = resp.status.as_u16();
        let body_str =
            String::from_utf8_lossy(&resp.collect().await.unwrap_or_default()).to_string();
        return Err(format!(
            "antigravity onboardUser status {status}: {body_str}"
        ));
    }

    let body_bytes = resp
        .collect()
        .await
        .map_err(|e| format!("antigravity onboardUser read: {e}"))?;

    let value: serde_json::Value = serde_json::from_slice(&body_bytes)
        .map_err(|e| format!("antigravity onboardUser parse: {e}"))?;

    let project_id = value
        .get("cloudaicompanionProject")
        .and_then(|v| v.get("id"))
        .and_then(|v| v.as_str())
        .or_else(|| value.get("projectId").and_then(|v| v.as_str()))
        .map(std::string::ToString::to_string);

    Ok(project_id)
}

fn patch_part_thought_signature(part: &mut serde_json::Value) {
    let has_fc = part.get("functionCall").is_some() || part.get("function_call").is_some();
    if has_fc
        && part.get("thoughtSignature").is_none()
        && part.get("thought_signature").is_none()
        && let Some(obj) = part.as_object_mut()
    {
        obj.insert(
            "thoughtSignature".to_string(),
            serde_json::json!("skip_thought_signature_validator"),
        );
        obj.insert(
            "thought_signature".to_string(),
            serde_json::json!("skip_thought_signature_validator"),
        );
    }
}

fn inject_sentinel_thought_signatures(contents: &mut serde_json::Value, model: &str) {
    let model_lc = model.to_lowercase();
    let is_flash_or_agent = (model_lc.contains("gemini") && model_lc.contains("flash"))
        || model_lc.contains("gemini-pro-agent")
        || model_lc.contains("gemini-3-flash-agent");
    if !is_flash_or_agent {
        return;
    }
    if let Some(arr) = contents.as_array_mut() {
        for msg in arr {
            if let Some(parts) = msg.get_mut("parts").and_then(|p| p.as_array_mut()) {
                for part in parts {
                    patch_part_thought_signature(part);
                }
            }
        }
    }
}

#[cfg(test)]
mod user_quota_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_inject_sentinel_thought_signatures_flash() {
        let mut contents = json!([
            {
                "role": "model",
                "parts": [
                    {
                        "functionCall": {
                            "name": "get_weather",
                            "args": {}
                        }
                    }
                ]
            }
        ]);

        inject_sentinel_thought_signatures(&mut contents, "gemini-3.7-flash-high");
        let fc_part = &contents[0]["parts"][0];
        assert_eq!(
            fc_part["thoughtSignature"],
            "skip_thought_signature_validator"
        );
        assert_eq!(
            fc_part["thought_signature"],
            "skip_thought_signature_validator"
        );
    }

    #[test]
    fn test_parse_antigravity_user_quota_summary_missing_groups() {
        let body = json!({});
        let result = parse_antigravity_user_quota_summary(&body);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_antigravity_user_quota_summary_valid() {
        let body = json!({
            "groups": [{
                "displayName": "Pro",
                "buckets": [{
                    "window": "WEEK",
                    "resetTime": "2023-01-01T00:00:00Z",
                    "remainingFraction": 0.5
                }, {
                    "window": "DAY",
                    "resetTime": "2023-01-01T00:00:00Z",
                    "remainingFraction": 0.8
                }]
            }]
        });

        let result = parse_antigravity_user_quota_summary(&body).expect("should parse");
        assert_eq!(result.plan_name.unwrap(), "Pro");
        assert_eq!(result.weekly_limit.unwrap(), 1000);
        assert_eq!(result.weekly_used.unwrap(), 500);
        assert_eq!(result.session_limit.unwrap(), 1000);
        assert_eq!(result.session_used.unwrap(), 200);
    }
}
