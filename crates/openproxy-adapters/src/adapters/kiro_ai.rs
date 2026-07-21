use super::*;
use openproxy_types::{OpenAIMessage, OpenAIRequest};
use once_cell::sync::Lazy;
use serde_json::Value;

static REGION_RE: Lazy<regex::Regex> = Lazy::new(|| regex::Regex::new(r"[a-z]{2}-[a-z]+-[0-9]").unwrap());

// =====================================================================
// Kiro AI (AWS CodeWhisperer)
// =====================================================================

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
                base_url: "https://codewhisperer.us-east-1.amazonaws.com".into(),
                auth_type: AdapterAuthType::Bearer,
                format: AdapterFormat::Openai,
                extra_headers: vec![],
            },
        }
    }

    fn parse_models_response(&self, json: &serde_json::Value) -> Option<Vec<DiscoveredModel>> {
        let models_arr = json
            .get("models")
            .and_then(|v| v.as_array())
            .or_else(|| json.get("availableModels").and_then(|v| v.as_array()))?;

        let mut discovered = Vec::new();
        for item in models_arr {
            let model_id_str = item
                .get("modelId")
                .and_then(|v| v.as_str())
                .or_else(|| item.get("id").and_then(|v| v.as_str()))?;
            let display_name_str = item
                .get("modelName")
                .and_then(|v| v.as_str())
                .or_else(|| item.get("name").and_then(|v| v.as_str()))
                .unwrap_or(model_id_str);

            discovered.push(DiscoveredModel {
                model_id: ModelId::new(model_id_str),
                display_name: Some(display_name_str.to_string()),
                target_format: TargetFormat::Openai,
                context_length: Some(200_000),
                max_output_tokens: Some(64000),
                input_modalities: None,
                output_modalities: None,
                model_type: Some("chat".to_string()),
                family: None,
                capabilities: Some(openproxy_types::ModelCapabilities {
                    vision: Some(true),
                    tool_calling: Some(true),
                    reasoning: Some(true),
                    thinking: Some(true),
                    attachment: None,
                    structured_output: None,
                    temperature: None,
                }),
            });
        }

        if discovered.is_empty() {
            None
        } else {
            Some(discovered)
        }
    }
}

crate::adapters::derive_default_from_new!(KiroAdapter);

impl ProviderAdapter for KiroAdapter {
    fn id(&self) -> &ProviderId {
        &self.config.id
    }

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

    fn build_auth_header(&self, api_key: &str) -> Option<(String, String)> {
        Some(("Authorization".into(), format!("Bearer {}", api_key)))
    }

    fn build_headers(
        &self,
        api_key: &str,
        _target_format: TargetFormat,
        _model: &ModelId,
    ) -> Vec<(String, String)> {
        let mut headers = vec![
            ("Content-Type".into(), "application/json".into()),
            (
                "x-amz-user-agent".into(),
                "aws-sdk-js/3.0.0 kiro/0.1".into(),
            ),
        ];
        if let Some(auth) = self.build_auth_header(api_key) {
            headers.push(auth);
        }
        headers
    }

    fn wrap_request_body(
        &self,
        body: bytes::Bytes,
        _target_format: TargetFormat,
        _model: &ModelId,
        resolved_target: &openproxy_types::context::ResolvedTarget,
    ) -> std::result::Result<bytes::Bytes, openproxy_types::error::CoreError> {
        let req: openproxy_types::OpenAIRequest = serde_json::from_slice(&body).map_err(|e| {
            openproxy_types::error::CoreError::Validation(format!("Invalid OpenAI request: {}", e))
        })?;

        let profile_arn = resolved_target
            .custom_meta
            .as_ref()
            .and_then(|m| m.kiro_profile_arn.as_deref());

        let kiro_req = build_kiro_request(&req, profile_arn);
        let kiro_bytes = serde_json::to_vec(&kiro_req).map_err(|e| {
            openproxy_types::error::CoreError::Validation(format!(
                "Failed to serialize Kiro request: {}",
                e
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
            return Ok(vec![]);
        }

        let mut region = "us-east-1".to_string();
        if !account_label.is_empty()
            && let Some(m) = REGION_RE.find(account_label)
        {
            region = m.as_str().to_string();
        }

        let endpoints = if region == "us-east-1" {
            vec![
                "https://q.us-east-1.amazonaws.com/ListAvailableModels?origin=AI_EDITOR".to_string(),
                "https://codewhisperer.us-east-1.amazonaws.com/ListAvailableModels?origin=AI_EDITOR".to_string(),
            ]
        } else {
            vec![
                format!("https://q.{region}.amazonaws.com/ListAvailableModels?origin=AI_EDITOR"),
                "https://q.us-east-1.amazonaws.com/ListAvailableModels?origin=AI_EDITOR"
                    .to_string(),
            ]
        };

        for endpoint in &endpoints {
            let mut req = UpstreamRequest::get(endpoint);
            if let Ok(v) = HeaderValue::from_str(&format!("Bearer {api_key}")) {
                req.headers.insert(http::header::AUTHORIZATION, v);
            }
            req.headers.insert(
                http::header::ACCEPT,
                HeaderValue::from_static("application/json"),
            );

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
        provider_specific: Option<&str>,
    ) -> Option<Result<openproxy_types::AccountQuota>> {
        if let Some(token) = access_token {
            Some(
                self.fetch_kiro_quota_local(upstream_client, token, provider_specific)
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
                fetch_error: Some("kiro requires OAuth access token".into()),
                model_details: None,
            }))
        }
    }
}

pub fn kiro_runtime_url(region: &str) -> String {
    let region = if region.is_empty() {
        "us-east-1"
    } else {
        region
    };
    let host = if region == "us-east-1" {
        format!("https://codewhisperer.{}.amazonaws.com", region)
    } else {
        format!("https://q.{}.amazonaws.com", region)
    };
    format!("{}/generateAssistantResponse", host)
}

impl KiroAdapter {
    async fn fetch_kiro_quota_local(
        &self,
        upstream: &Arc<UpstreamClient>,
        access_token: &str,
        provider_specific: Option<&str>,
    ) -> Result<openproxy_types::AccountQuota> {
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
            if let Some(arn) = meta.get("profileArn").and_then(|v| v.as_str()) {
                profile_arn = Some(arn.to_string());
            } else if let Some(arn) = meta.get("profile_arn").and_then(|v| v.as_str()) {
                profile_arn = Some(arn.to_string());
            }
        }

        let base_url = if region == "us-east-1" || region.is_empty() {
            "https://codewhisperer.us-east-1.amazonaws.com".to_string()
        } else {
            format!("https://q.{region}.amazonaws.com")
        };

        let profile_arn = match profile_arn {
            Some(arn) => Some(arn),
            None => {
                let url = format!("{base_url}/");
                let mut req =
                    UpstreamRequest::post_json(&url, bytes::Bytes::from(r#"{"maxResults":10}"#));
                if let Ok(v) = http::HeaderValue::from_str(&format!("Bearer {access_token}")) {
                    req.headers.insert(http::header::AUTHORIZATION, v);
                }
                req.headers.insert(
                    http::header::HeaderName::from_static("x-amz-target"),
                    http::HeaderValue::from_static(
                        "AmazonCodeWhispererService.ListAvailableProfiles",
                    ),
                );
                req.headers.insert(
                    http::header::HeaderName::from_static("x-amz-user-agent"),
                    http::HeaderValue::from_static("aws-sdk-js/3.0.0 kiro/0.1"),
                );

                let cancel = CancellationToken::new();

                match upstream.call(req, TimeoutProfile::OAuth, cancel).await {
                    Ok(resp) if resp.status.is_success() => {
                        if let Ok(body_bytes) = resp.collect().await {
                            if let Ok(value) =
                                serde_json::from_slice::<serde_json::Value>(&body_bytes)
                            {
                                value
                                    .get("profiles")
                                    .and_then(|v| v.as_array())
                                    .and_then(|arr| {
                                        arr.iter()
                                            .find(|p| {
                                                p.get("arn")
                                                    .or_else(|| p.get("profileArn"))
                                                    .and_then(|v| v.as_str())
                                                    .map(|s| s.contains(&format!(":{region}:")))
                                                    .unwrap_or(false)
                                            })
                                            .or_else(|| arr.first())
                                    })
                                    .and_then(|p| {
                                        p.get("arn")
                                            .or_else(|| p.get("profileArn"))
                                            .and_then(|v| v.as_str())
                                    })
                                    .map(|s| s.to_string())
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    }
                    Ok(resp) => {
                        let status_code = resp.status;
                        let body_str =
                            String::from_utf8_lossy(&resp.collect().await.unwrap_or_default())
                                .to_string();
                        tracing::info!(status = %status_code, body = %body_str, "Kiro profile ARN discovery returned non-success; proceeding without profile ARN");
                        None
                    }
                    Err(e) => {
                        tracing::info!(error = %e, "kiro listAvailableProfiles network call failed; proceeding without profile ARN");
                        None
                    }
                }
            }
        };

        let url = format!("{base_url}/");
        let mut payload = serde_json::json!({
            "origin": "AI_EDITOR",
            "resourceType": "AGENTIC_REQUEST"
        });
        if let Some(ref arn) = profile_arn {
            payload["profileArn"] = serde_json::json!(arn);
        }
        let body_bytes = match serde_json::to_vec(&payload) {
            Ok(b) => b,
            Err(e) => {
                tracing::info!(error = %e, "kiro GetUsageLimits serialize payload failed; returning empty quota");
                return Ok(openproxy_types::AccountQuota {
                    session_used: None,
                    session_limit: None,
                    session_reset_at: None,
                    weekly_used: None,
                    weekly_limit: None,
                    weekly_reset_at: None,
                    plan_name: Some("Kiro".to_string()),
                    last_fetched_at: openproxy_types::now_unix_secs_str(),
                    fetch_error: None,
                    model_details: None,
                });
            }
        };

        let mut req = UpstreamRequest::post_json(&url, bytes::Bytes::from(body_bytes));
        if let Ok(v) = http::HeaderValue::from_str(&format!("Bearer {access_token}")) {
            req.headers.insert(http::header::AUTHORIZATION, v);
        }
        req.headers.insert(
            http::header::HeaderName::from_static("x-amz-target"),
            http::HeaderValue::from_static("AmazonCodeWhispererService.GetUsageLimits"),
        );
        req.headers.insert(
            http::header::HeaderName::from_static("x-amz-user-agent"),
            http::HeaderValue::from_static("aws-sdk-js/3.0.0 kiro/0.1"),
        );

        let cancel = CancellationToken::new();
        let resp = match upstream.call(req, TimeoutProfile::OAuth, cancel).await {
            Ok(r) => r,
            Err(e) => {
                tracing::info!(error = %e, "kiro GetUsageLimits network call failed; returning empty quota without error");
                return Ok(openproxy_types::AccountQuota {
                    session_used: None,
                    session_limit: None,
                    session_reset_at: None,
                    weekly_used: None,
                    weekly_limit: None,
                    weekly_reset_at: None,
                    plan_name: Some("Kiro".to_string()),
                    last_fetched_at: openproxy_types::now_unix_secs_str(),
                    fetch_error: None,
                    model_details: None,
                });
            }
        };

        if !resp.status.is_success() {
            let status = resp.status.as_u16();
            let body_str =
                String::from_utf8_lossy(&resp.collect().await.unwrap_or_default()).to_string();
            tracing::info!(status = status, body = %body_str, "Kiro GetUsageLimits returned non-success (likely restricted quota access); returning empty quota without error");
            return Ok(openproxy_types::AccountQuota {
                session_used: None,
                session_limit: None,
                session_reset_at: None,
                weekly_used: None,
                weekly_limit: None,
                weekly_reset_at: None,
                plan_name: Some("Kiro".to_string()),
                last_fetched_at: openproxy_types::now_unix_secs_str(),
                fetch_error: None,
                model_details: None,
            });
        }

        let resp_bytes = resp
            .collect()
            .await
            .map_err(|e| CoreError::UpstreamConnection(format!("kiro GetUsageLimits read: {e}")))?;
        let data: serde_json::Value = serde_json::from_slice(&resp_bytes)
            .map_err(|e| CoreError::Parse(format!("kiro GetUsageLimits parse: {e}")))?;

        let reset_at = data
            .get("nextDateReset")
            .or_else(|| data.get("resetDate"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let usage_list = data.get("usageBreakdownList").and_then(|v| v.as_array());

        let mut session_used = None;
        let mut session_limit = None;

        if let Some(arr) = usage_list {
            for breakdown in arr {
                let resource_type = breakdown
                    .get("resourceType")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if resource_type.to_lowercase() == "agentic_request" {
                    let current = breakdown
                        .get("currentUsageWithPrecision")
                        .and_then(|v| v.as_f64())
                        .or_else(|| breakdown.get("currentUsage").and_then(|v| v.as_f64()))
                        .map(|v| v.round() as i64);
                    let limit = breakdown
                        .get("usageLimitWithPrecision")
                        .and_then(|v| v.as_f64())
                        .or_else(|| breakdown.get("usageLimit").and_then(|v| v.as_f64()))
                        .map(|v| v.round() as i64);

                    session_used = current;
                    session_limit = limit;
                    break;
                }
            }
        }

        let plan_name = data
            .get("subscriptionInfo")
            .and_then(|v| v.get("subscriptionTitle"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
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
            model_details: None,
        })
    }
}
/// Request body envelope used by `generateAssistantResponse`.
///
/// Only `conversationState` and (optionally) `profileArn` +
/// `inferenceConfig` are required. The executor builds the
/// `currentMessage` from the most recent `user` message in the
/// OpenAI request, and folds prior turns into
/// `conversationState.history` so multi-turn conversations work.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroRequest {
    #[serde(rename = "conversationState")]
    pub conversation_state: KiroConversationState,
    #[serde(rename = "profileArn", skip_serializing_if = "Option::is_none")]
    pub profile_arn: Option<String>,
    #[serde(rename = "inferenceConfig", skip_serializing_if = "Option::is_none")]
    pub inference_config: Option<KiroInferenceConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroConversationState {
    #[serde(rename = "currentMessage")]
    pub current_message: KiroCurrentMessage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history: Option<Vec<KiroHistoryItem>>,
    #[serde(rename = "chatTriggerType")]
    pub chat_trigger_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroCurrentMessage {
    #[serde(rename = "userInputMessage")]
    pub user_input_message: KiroUserInputMessage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroUserInputMessage {
    pub content: String,
    #[serde(rename = "modelId")]
    pub model_id: String,
    pub origin: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroHistoryItem {
    #[serde(rename = "userInputMessage")]
    pub user_input_message: KiroUserInputMessage,
    #[serde(rename = "assistantResponseMessage")]
    pub assistant_response_message: KiroAssistantResponseMessage,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct KiroAssistantResponseMessage {
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroInferenceConfig {
    #[serde(rename = "maxTokens", skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(rename = "topP", skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
}

/// Build a [`KiroRequest`] from an OpenAI [`OpenAIRequest`].
///
/// The conversion rules:
/// - `model` → `conversationState.currentMessage.userInputMessage.modelId`
/// - Last `user` message → `currentMessage.userInputMessage.content`
/// - Preceding `user`/`assistant` turns → `conversationState.history`
/// - `max_tokens` / `temperature` / `top_p` / `stop` → `inferenceConfig`
/// - `stream` is dropped (Kiro is always-on for the protocol
///   variant we use; the streaming variant is the EventStream
///   binary format and is a follow-up)
pub const KIRO_DEFAULT_MODEL: &str = "kiro-default-model";

fn build_kiro_request(openai: &OpenAIRequest, profile_arn: Option<&str>) -> KiroRequest {
    let (history_msgs, current_msg) = split_history(openai);

    let history: Vec<KiroHistoryItem> = history_msgs
        .chunks(2)
        .filter_map(|pair| {
            if let [user, assistant] = pair {
                Some(KiroHistoryItem {
                    user_input_message: KiroUserInputMessage {
                        content: user
                            .content
                            .as_ref()
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        model_id: KIRO_DEFAULT_MODEL.to_string(),
                        origin: "AI_EDITOR".to_string(),
                    },
                    assistant_response_message: KiroAssistantResponseMessage {
                        content: assistant
                            .content
                            .as_ref()
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    },
                })
            } else {
                None
            }
        })
        .collect();

    let inference_config = if openai.max_tokens.is_some()
        || openai.temperature.is_some()
        || openai.top_p.is_some()
        || openai.stop.is_some()
    {
        Some(KiroInferenceConfig {
            max_tokens: openai.max_tokens,
            temperature: openai.temperature,
            top_p: openai.top_p,
            stop: openai.stop.clone(),
        })
    } else {
        None
    };

    KiroRequest {
        conversation_state: KiroConversationState {
            current_message: KiroCurrentMessage {
                user_input_message: KiroUserInputMessage {
                    content: current_msg
                        .as_ref()
                        .and_then(|m| m.content.as_ref())
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    model_id: openai.model.clone(),
                    origin: "AI_EDITOR".to_string(),
                },
            },
            history: if history.is_empty() {
                None
            } else {
                Some(history)
            },
            chat_trigger_type: "MANUAL".to_string(),
        },
        profile_arn: profile_arn.map(|s| s.to_string()),
        inference_config,
    }
}

/// Split the OpenAI messages into the (history, current_user_message)
/// pair. Kiro's `currentMessage` is always a single user turn, so
/// we keep the most recent user message out of the history list.
fn split_history(req: &OpenAIRequest) -> (Vec<&OpenAIMessage>, Option<OpenAIMessage>) {
    if req.messages.is_empty() {
        return (Vec::new(), None);
    }
    let last_user_idx = req
        .messages
        .iter()
        .rposition(|m| m.role == "user")
        .unwrap_or(req.messages.len() - 1);
    let history: Vec<&OpenAIMessage> = req.messages[..last_user_idx].iter().collect();
    let current = req.messages[last_user_idx].clone();
    (history, Some(current))
}
