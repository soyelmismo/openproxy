use super::*;
use crate::upstream::UpstreamError;
// =====================================================================
// Antigravity (Cloud Code)
// =====================================================================

/// Adapter for Google's Antigravity (Cloud Code) API.
///
/// Antigravity wraps Gemini requests in a Cloud Code envelope:
/// - Auth: `Authorization: Bearer <token>` (OAuth)
/// - Chat URL: `${base}/v1internal:generateContent`
/// - No model discovery endpoint (models are hardcoded)
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AntigravityAdapter {
    config: ProviderAdapterConfig,
}

pub const DEFAULT_ANTIGRAVITY_BASE_URL: &str = "https://daily-cloudcode-pa.googleapis.com";

impl AntigravityAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("antigravity"),
                base_url: DEFAULT_ANTIGRAVITY_BASE_URL.into(),
                auth_type: AdapterAuthType::Bearer,
                format: AdapterFormat::Gemini,
                extra_headers: vec![],
            },
        }
    }

    /// Parse fetchAvailableModels response into DiscoveredModel list.
    fn parse_models_response(&self, body: &serde_json::Value) -> Option<Vec<DiscoveredModel>> {
        let models_obj = body.get("models")?.as_object()?;

        let mut models = Vec::new();
        for (model_id, model_data) in models_obj {
            let upstream_id = model_id.clone();
            let client_id = upstream_id.clone();

            let display_name = model_data
                .get("displayName")
                .and_then(|d| d.as_str())
                .map(|s| s.to_string());

            // Read maxTokens as context_length (fallback to contextLength)
            let context_length = model_data
                .get("maxTokens")
                .and_then(|c| c.as_u64())
                .or_else(|| model_data.get("contextLength").and_then(|c| c.as_u64()))
                .map(|v| v as i64);

            // Read maxOutputTokens as max_output_tokens
            let max_output_tokens = model_data
                .get("maxOutputTokens")
                .and_then(|c| c.as_u64())
                .map(|v| v as i64)
                .or(Some(8192));

            let target_format = if client_id.starts_with("claude") {
                TargetFormat::Anthropic
            } else if client_id.starts_with("gemini") || client_id.starts_with("gpt-oss") {
                TargetFormat::Gemini
            } else {
                TargetFormat::Openai
            };

            // Infer capabilities from upstream fields
            let supports_thinking = model_data
                .get("supportsThinking")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let supports_images = model_data
                .get("supportsImages")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let tool_formatter_type = model_data
                .get("toolFormatterType")
                .and_then(|v| v.as_str())
                .is_some();
            let supports_cumulative_context = model_data
                .get("supportsCumulativeContext")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let capabilities = openproxy_types::ModelCapabilities {
                vision: Some(supports_images),
                tool_calling: Some(tool_formatter_type || supports_cumulative_context),
                reasoning: Some(supports_thinking),
                thinking: Some(supports_thinking),
                attachment: Some(supports_images),
                structured_output: None,
                temperature: None,
            };

            models.push(DiscoveredModel {
                model_id: ModelId::new(client_id),
                display_name,
                target_format,
                context_length,
                max_output_tokens,
                input_modalities: None,
                output_modalities: None,
                model_type: Some("chat".to_string()),
                family: None,
                capabilities: Some(capabilities),
            });
        }

        if models.is_empty() {
            None
        } else {
            Some(models)
        }
    }
}

crate::adapters::derive_default_from_new!(AntigravityAdapter);

impl ProviderAdapter for AntigravityAdapter {
    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn metadata(&self) -> openproxy_types::ProviderMetadata {
        let mut meta = openproxy_types::ProviderMetadata {
            built_in: openproxy_types::is_builtin(self.id().as_str()),
            deletable: !openproxy_types::is_builtin(self.id().as_str()),
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
        format!("{}/v1internal:generateContent", self.config.base_url)
    }

    fn models_url(&self) -> Option<String> {
        // Antigravity does not expose a /models endpoint.
        None
    }

    async fn fetch_models(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        if api_key.is_empty() {
            return Ok(vec![]);
        }

        let endpoints = [
            "https://daily-cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels",
            "https://cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels",
        ];

        for endpoint in &endpoints {
            let mut req = UpstreamRequest::post_json(*endpoint, Bytes::from_static(b"{}"));
            if let Ok(v) = HeaderValue::from_str(&format!("Bearer {api_key}")) {
                req.headers.insert(http::header::AUTHORIZATION, v);
            }
            req.headers.insert(
                http::header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            crate::antigravity_headers::inject_antigravity_headers(&mut req.headers, None);

            let cancel = CancellationToken::new();
            if let Ok(resp) = upstream_client
                .call(req, TimeoutProfile::ModelDiscovery, cancel)
                .await
                && resp.status.is_success()
                && let Ok(body_bytes) = resp.collect().await
                && let Ok(json) = serde_json::from_slice::<serde_json::Value>(&body_bytes)
                && let Some(models) = self.parse_models_response(&json)
            {
                return Ok(models);
            }
        }

        Ok(vec![])
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
            Some(self.fetch_antigravity_quota_local(upstream_client, token).await)
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

static PLAN_CACHE: once_cell::sync::Lazy<parking_lot::Mutex<std::collections::HashMap<String, String>>> =
    once_cell::sync::Lazy::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

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
                if let Ok(ref summary_quota) = summary_res {
                    if summary_quota.weekly_used.is_some() {
                        models_quota.weekly_used = summary_quota.weekly_used;
                        models_quota.weekly_limit = summary_quota.weekly_limit;
                        models_quota.weekly_reset_at = summary_quota.weekly_reset_at.clone();
                    }
                    if models_quota.session_used.is_none() && summary_quota.session_used.is_some() {
                        models_quota.session_used = summary_quota.session_used;
                        models_quota.session_limit = summary_quota.session_limit;
                        models_quota.session_reset_at = summary_quota.session_reset_at.clone();
                    }
                }

                if let Some(plan) = plan_result {
                    models_quota.plan_name = Some(plan);
                } else if models_quota.plan_name.is_none()
                    || models_quota.plan_name.as_deref() == Some("Antigravity")
                {
                    if let Ok(ref summary_quota) = summary_res {
                        if let Some(summary_plan) = &summary_quota.plan_name
                            && summary_plan != "Antigravity"
                        {
                            models_quota.plan_name = Some(summary_plan.clone());
                        } else {
                            models_quota.plan_name = Some("Free".to_string());
                        }
                    } else {
                        models_quota.plan_name = Some("Free".to_string());
                    }
                }

                Ok(models_quota)
            }
            (Err(_models_err), Ok(mut summary_quota)) => {
                if let Some(plan) = plan_result {
                    summary_quota.plan_name = Some(plan);
                } else if summary_quota.plan_name.is_none()
                    || summary_quota.plan_name.as_deref() == Some("Antigravity")
                {
                    summary_quota.plan_name = Some("Free".to_string());
                }
                Ok(summary_quota)
            }
            (Err(models_err), Err(_)) => Err(models_err),
        }
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

        for endpoint in &endpoints {
            let mut req = UpstreamRequest::post_json(*endpoint, bytes::Bytes::from_static(b"{}"));
            if let Ok(v) = http::HeaderValue::from_str(&format!("Bearer {access_token}")) {
                req.headers.insert(http::header::AUTHORIZATION, v);
            }
            req.headers.insert(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static("application/json"),
            );
            crate::antigravity_headers::inject_antigravity_headers(&mut req.headers, None);

            let cancel = CancellationToken::new();
            let response = upstream.call(req, TimeoutProfile::Quota, cancel).await;

            if let Ok(resp) = response
                && resp.status.is_success()
            {
                let body = match resp.collect().await {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&body) {
                    return parse_antigravity_models_response(&json);
                }
            }
        }

        Err(CoreError::UpstreamConnection(
            "all fetchAvailableModels endpoints failed".into(),
        ))
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
                Err(UpstreamError::Cancel) => return Err(CoreError::ClientDisconnected),
                Err(e) => {
                    last_err = Some(CoreError::UpstreamConnection(format!(
                        "retrieveUserQuotaSummary: {e}"
                    )));
                    continue;
                }
            };

            if !response.status.is_success() {
                last_err = Some(CoreError::UpstreamConnection(format!(
                    "retrieveUserQuotaSummary: status {}",
                    response.status.as_u16()
                )));
                continue;
            }

            let body = match response.collect().await {
                Ok(b) => b,
                Err(e) => {
                    last_err = Some(CoreError::UpstreamConnection(format!(
                        "retrieveUserQuotaSummary body: {e}"
                    )));
                    continue;
                }
            };

            let json: serde_json::Value = match serde_json::from_slice(&body) {
                Ok(j) => j,
                Err(e) => {
                    last_err = Some(CoreError::Parse(format!(
                        "retrieveUserQuotaSummary parse: {e}"
                    )));
                    continue;
                }
            };

            return parse_antigravity_user_quota_summary(&json);
        }

        Err(last_err.unwrap_or_else(|| {
            CoreError::UpstreamConnection("retrieveUserQuotaSummary: all endpoints failed".into())
        }))
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

        let payload = bytes::Bytes::from_static(b"{\"metadata\": {\"ideType\": \"ANTIGRAVITY\"}}");

        for endpoint in &endpoints {
            let mut req = UpstreamRequest::post_json(*endpoint, payload.clone());
            if let Ok(v) = http::HeaderValue::from_str(&format!("Bearer {access_token}")) {
                req.headers.insert(http::header::AUTHORIZATION, v);
            }
            req.headers.insert(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static("application/json"),
            );
            crate::antigravity_headers::inject_antigravity_headers(&mut req.headers, None);

            let cancel = CancellationToken::new();
            let response = upstream.call(req, TimeoutProfile::Quota, cancel).await;

            if let Ok(resp) = response
                && resp.status.is_success()
            {
                let body = match resp.collect().await {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&body) {
                    let paid_name = json.pointer("/paidTier/name").and_then(|v| v.as_str());
                    let paid_id = json.pointer("/paidTier/id").and_then(|v| v.as_str());

                    let mut tier = paid_name.or(paid_id);

                    if tier.is_none() {
                        let is_ineligible = json
                            .pointer("/ineligibleTiers")
                            .and_then(|v| v.as_array())
                            .is_some_and(|a| !a.is_empty());

                        if !is_ineligible {
                            let current_name =
                                json.pointer("/currentTier/name").and_then(|v| v.as_str());
                            let current_id = json.pointer("/currentTier/id").and_then(|v| v.as_str());
                            tier = current_name.or(current_id);
                        } else if let Some(allowed) =
                            json.pointer("/allowedTiers").and_then(|v| v.as_array())
                        {
                            for t in allowed {
                                if t.get("isDefault")
                                    .and_then(|v| v.as_bool())
                                    .unwrap_or(false)
                                {
                                    let name = t.get("name").and_then(|v| v.as_str());
                                    let id = t.get("id").and_then(|v| v.as_str());
                                    tier = name.or(id);
                                    break;
                                }
                            }
                        }
                    }

                    if let Some(t) = tier {
                        let upper = t.to_uppercase();
                        let plan_name = if upper.contains("ULTRA") {
                            "Ultra".to_string()
                        } else if upper.contains("PRO")
                            || upper.contains("PREMIUM")
                            || upper.contains("GOOGLE_ONE")
                            || upper.contains("ONE_AI")
                            || upper.contains("GOOGLE ONE")
                        {
                            "Pro".to_string()
                        } else if upper.contains("ENTERPRISE") {
                            "Enterprise".to_string()
                        } else if upper.contains("BUSINESS") || upper.contains("STANDARD") {
                            "Business".to_string()
                        } else if upper.contains("PLUS") {
                            "Plus".to_string()
                        } else if upper.contains("LITE") || upper.contains("LIGHT") {
                            "Lite".to_string()
                        } else if upper.contains("FREE")
                            || upper.contains("INDIVIDUAL")
                            || upper.contains("LEGACY")
                        {
                            "Free".to_string()
                        } else {
                            t.to_string()
                        };

                        PLAN_CACHE.lock().insert(access_token.to_string(), plan_name.clone());
                        return Some(plan_name);
                    }
                }
            }
        }

        None
    }
}

fn parse_antigravity_models_response(body: &serde_json::Value) -> Result<openproxy_types::AccountQuota> {
    const NORMALIZED_BASE: i64 = 1000;

    let models = body
        .get("models")
        .and_then(|m| m.as_object())
        .ok_or_else(|| CoreError::Internal("missing 'models' in response".into()))?;

    let mut details: Vec<openproxy_types::ModelQuotaDetail> = Vec::new();
    let mut worst_remaining = f64::MAX;
    let mut worst_model_id = String::new();

    for (model_id, model_data) in models {
        let Some(quota_info) = model_data.get("quotaInfo") else {
            continue;
        };
        let reset_time = quota_info
            .get("resetTime")
            .and_then(|r| r.as_str())
            .map(String::from);

        let remaining_fraction = quota_info
            .get("remainingFraction")
            .and_then(|f| f.as_f64())
            .unwrap_or_else(|| if reset_time.is_some() { 0.0 } else { 1.0 });

        let is_unlimited = reset_time.is_none() && remaining_fraction >= 1.0;
        let remaining = (NORMALIZED_BASE as f64 * remaining_fraction) as i64;
        let used = if is_unlimited {
            0
        } else {
            NORMALIZED_BASE.saturating_sub(remaining)
        };

        details.push(openproxy_types::ModelQuotaDetail {
            model_id: model_id.clone(),
            session_used: used,
            session_limit: NORMALIZED_BASE,
            session_reset_at: reset_time,
            remaining_fraction,
        });

        if remaining_fraction < worst_remaining {
            worst_remaining = remaining_fraction;
            worst_model_id = model_id.clone();
        }
    }

    if details.is_empty() {
        return Err(CoreError::Internal(
            "no quota info found in response".into(),
        ));
    }

    let worst = details
        .iter()
        .find(|d| d.model_id == worst_model_id)
        .unwrap();

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

fn parse_antigravity_user_quota_summary(body: &serde_json::Value) -> Result<openproxy_types::AccountQuota> {
    const NORMALIZED_BASE: i64 = 1000;

    let groups = body
        .get("groups")
        .and_then(|g| g.as_array())
        .ok_or_else(|| {
            CoreError::Internal("missing 'groups' in retrieveUserQuotaSummary".into())
        })?;

    let mut weekly_used: Option<i64> = None;
    let mut weekly_limit: Option<i64> = None;
    let mut weekly_reset_at: Option<String> = None;
    let mut session_used: Option<i64> = None;
    let mut session_limit: Option<i64> = None;
    let mut session_reset_at: Option<String> = None;
    let mut plan_name: Option<String> = None;

    for group in groups {
        let group_plan = group.get("displayName").and_then(|n| n.as_str());

        let buckets = match group.get("buckets").and_then(|b| b.as_array()) {
            Some(b) => b,
            None => continue,
        };

        for bucket in buckets {
            let reset_time = bucket
                .get("resetTime")
                .and_then(|r| r.as_str())
                .map(String::from);
            let window = bucket.get("window").and_then(|w| w.as_str()).unwrap_or("");

            let remaining_fraction = bucket
                .get("remainingFraction")
                .and_then(|f| f.as_f64())
                .unwrap_or_else(|| if reset_time.is_some() { 0.0 } else { 1.0 });

            let is_unlimited = reset_time.is_none() && remaining_fraction >= 1.0;
            let remaining = (NORMALIZED_BASE as f64 * remaining_fraction) as i64;
            let used = if is_unlimited {
                0
            } else {
                NORMALIZED_BASE.saturating_sub(remaining)
            };

            let is_weekly =
                window.to_uppercase().contains("WEEK") || window.eq_ignore_ascii_case("WEEKLY");
            if is_weekly && weekly_used.is_none() {
                weekly_used = Some(used);
                weekly_limit = Some(NORMALIZED_BASE);
                weekly_reset_at = reset_time;
                if plan_name.is_none() {
                    plan_name = group_plan.map(|s| s.to_string());
                }
            } else if !is_weekly && session_used.is_none() {
                session_used = Some(used);
                session_limit = Some(NORMALIZED_BASE);
                session_reset_at = reset_time;
                if plan_name.is_none() {
                    plan_name = group_plan.map(|s| s.to_string());
                }
            }
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
