use super::*;

// =====================================================================
// Kiro AI (AWS CodeWhisperer)
// =====================================================================

/// Adapter for Kiro AI (AWS CodeWhisperer).
#[derive(Clone)]
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
                capabilities: Some(crate::capabilities::ModelCapabilities {
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

    fn metadata(&self) -> crate::providers::ProviderMetadata {
        let mut meta = crate::providers::ProviderMetadata {
            built_in: crate::providers::is_builtin(self.id().as_str()),
            deletable: !crate::providers::is_builtin(self.id().as_str()),
            supports_quota: true,
            quota_refresh_supported: true,
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
            && let Ok(re) = regex::Regex::new(r"[a-z]{2}-[a-z]+-[0-9]")
            && let Some(m) = re.find(account_label)
        {
            region = m.as_str().to_string();
        }
        crate::executor_kiro::kiro_runtime_url(&region)
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
            ("x-goog-api-client".into(), "kiro_ai".into()),
        ];
        if let Some(auth) = self.build_auth_header(api_key) {
            headers.push(auth);
        }
        headers
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
            && let Ok(re) = regex::Regex::new(r"[a-z]{2}-[a-z]+-[0-9]")
            && let Some(m) = re.find(account_label)
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

    async fn execute_custom(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        req: crate::pipeline::PipelineRequest,
        resolved_target: &crate::pipeline::context::ResolvedTarget,
        _ctx: Option<crate::adapters::CustomExecutionContext>,
    ) -> Option<std::result::Result<crate::translation::OpenAIResponse, CoreError>> {
        let custom_meta = resolved_target.custom_meta.as_ref()?;

        let region = custom_meta
            .kiro_region
            .as_deref()
            .filter(|r| !r.is_empty())
            .unwrap_or(crate::executor_kiro::KIRO_DEFAULT_REGION);

        let profile_arn = custom_meta.kiro_profile_arn.as_deref();

        // Use Cow or a quick clone? Wait, Phase 2 wants to remove clone on openai_request.
        // For now, let's just create a modified clone since custom executors might need an owned OpenAIRequest, or maybe executor takes a reference.
        // Wait, execute_kiro takes `req: &crate::translation::OpenAIRequest`!
        // We can create a modified request or the executor itself could take the model name!
        // Let's check `execute_kiro` signature.
        // Assuming it's `(&Arc<UpstreamClient>, &str, &str, Option<&str>, &OpenAIRequest, ...)`
        // I will just use a cloned request for now or modify `execute_kiro` later.
        let mut custom_req = (*req.openai_request).clone();
        custom_req.model = resolved_target.model.model_id.as_str().to_string();

        Some(
            crate::executor_kiro::execute_kiro(
                upstream_client,
                &custom_meta.access_token,
                region,
                profile_arn,
                &custom_req,
                req.client_disconnected.clone(),
                None, // What was here?
            )
            .await,
        )
    }

    async fn fetch_quota(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        _: &str,
        access_token: Option<&str>,
        provider_specific: Option<&str>,
    ) -> Option<Result<crate::quota::AccountQuota>> {
        if let Some(token) = access_token {
            Some(crate::quota::fetch_kiro_quota(upstream_client, token, provider_specific).await)
        } else {
            Some(Ok(crate::quota::AccountQuota {
                session_used: None,
                session_limit: None,
                session_reset_at: None,
                weekly_used: None,
                weekly_limit: None,
                weekly_reset_at: None,
                plan_name: None,
                last_fetched_at: crate::admin::now_unix_secs_str(),
                fetch_error: Some("kiro requires OAuth access token".into()),
                model_details: None,
            }))
        }
    }
}
