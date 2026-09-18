use super::{
    AdapterAuthType, AdapterFormat, Arc, Bytes, CancellationToken, CoreError, DiscoveredModel,
    HeaderValue, ModelId, ProviderAdapter, ProviderAdapterConfig, ProviderId, Result, TargetFormat,
    TimeoutProfile, UpstreamClient, UpstreamRequest,
};
use crate::spoofer::{AntigravitySpoofer, ClientSpoofer};

pub mod quota;
#[cfg(test)]
mod tests;
pub mod tokens;

pub use quota::{
    NORMALIZED_BASE, normalize_quota_fraction, parse_antigravity_models_response,
    parse_antigravity_user_quota_summary, prune_plan_cache,
};
pub use tokens::{
    COUNT_TOKENS_URL, LOAD_CODE_ASSIST_URL, ONBOARD_USER_URL, SENTINEL_SIGNATURE, count_tokens,
    load_code_assist, onboard_user, parse_total_tokens,
};

crate::define_jump_map! {
    /// Jump map for Antigravity physical model translation.
    pub fn map_antigravity_physical_model(model: &str) -> &str {
        "gemini-3.1-pro" | "gemini-3.1-pro-high" | "gemini-3.1-pro-medium" | "gemini-3-pro" => "gemini-pro-agent",
        "gemini-3.5-flash" | "gemini-3.5-flash-high" | "gemini-3.5-flash-medium" | "gemini-3-flash" => "gemini-3-flash-agent",
        other => other,
    }
}

pub const DEFAULT_ANTIGRAVITY_BASE_URL: &str = "https://daily-cloudcode-pa.googleapis.com";

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
    pub fn parse_models_response(body: &serde_json::Value) -> Option<Vec<DiscoveredModel>> {
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
        if self.id().as_str() == "antigravity" || self.id().as_str() == "agy" {
            meta.supports_quota = true;
            meta.quota_refresh_supported = true;
        }
        meta
    }

    fn build_chat_url(&self, _target_format: TargetFormat, _model: &ModelId) -> String {
        format!(
            "{}/v1internal:streamGenerateContent?alt=sse",
            self.config.base_url
        )
    }

    fn models_url(&self) -> Option<String> {
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
        crate::adapters::gemini::serialize_gemini_request(req, messages)
    }

    fn translate_non_streaming_response(
        &self,
        _target_format: TargetFormat,
        response_body: serde_json::Value,
    ) -> std::result::Result<openproxy_types::OpenAIResponse, CoreError> {
        crate::adapters::gemini::deserialize_gemini_response(&response_body)
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
                tokens::inject_sentinel_thought_signatures(contents, physical_model);
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
        if let Some(token) = access_token {
            Some(quota::fetch_antigravity_quota_local(upstream_client, token).await)
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
    if let Err(e) = crate::antigravity_headers::insert_bearer(&mut req, api_key) {
        tracing::warn!("antigravity build bearer header for {endpoint}: {e} — skipping endpoint");
        return None;
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
