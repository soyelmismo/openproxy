use super::*;

// =====================================================================
// Antigravity (Cloud Code)
// =====================================================================

/// Adapter for Google's Antigravity (Cloud Code) API.
///
/// Antigravity wraps Gemini requests in a Cloud Code envelope:
/// - Auth: `Authorization: Bearer <token>` (OAuth)
/// - Chat URL: `${base}/v1internal:generateContent`
/// - No model discovery endpoint (models are hardcoded)
#[derive(Clone)]
pub struct AntigravityAdapter {
    config: ProviderAdapterConfig,
}

impl AntigravityAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("antigravity"),
                base_url: "https://daily-cloudcode-pa.googleapis.com".into(),
                auth_type: AdapterAuthType::Bearer,
                format: AdapterFormat::Gemini,
                extra_headers: vec![],
            },
        }
    }

    /// Remap Antigravity upstream model IDs to client-facing IDs.
    fn remap_antigravity_model_id(upstream_id: &str) -> String {
        match upstream_id {
            "gemini-3.5-flash-extra-low" => "gemini-3.5-flash-low",
            "gemini-3.5-flash-low" => "gemini-3.5-flash-medium",
            "gemini-3.5-flash-medium" => "gemini-3.5-flash-high",
            "gemini-3.5-flash-high" => "gemini-3.5-flash-high",
            "gemini-3-flash-agent" => "gemini-3.5-flash-high",
            _ => upstream_id,
        }
        .to_string()
    }

    /// Parse fetchAvailableModels response into DiscoveredModel list.
    fn parse_models_response(&self, body: &serde_json::Value) -> Option<Vec<DiscoveredModel>> {
        let models_obj = body.get("models")?.as_object()?;

        let mut models = Vec::new();
        for (model_id, model_data) in models_obj {
            let upstream_id = model_id.clone();
            let client_id = Self::remap_antigravity_model_id(&upstream_id);

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

            let capabilities = crate::capabilities::ModelCapabilities {
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

    /// Hardcoded model catalog for when no OAuth token is available.
    fn hardcoded_models(&self) -> Vec<DiscoveredModel> {
        vec![
            DiscoveredModel {
                model_id: ModelId::new("gemini-2.5-pro"),
                display_name: Some("Gemini 2.5 Pro".to_string()),
                target_format: TargetFormat::Gemini,
                context_length: Some(1_048_576),
                max_output_tokens: Some(8192),
                input_modalities: None,
                output_modalities: None,
                model_type: Some("chat".to_string()),
                family: None,
                capabilities: Some(crate::capabilities::ModelCapabilities {
                    vision: Some(true),
                    tool_calling: Some(true),
                    reasoning: None,
                    thinking: None,
                    attachment: None,
                    structured_output: None,
                    temperature: None,
                }),
            },
            DiscoveredModel {
                model_id: ModelId::new("gemini-2.5-flash"),
                display_name: Some("Gemini 2.5 Flash".to_string()),
                target_format: TargetFormat::Gemini,
                context_length: Some(1_048_576),
                max_output_tokens: Some(8192),
                input_modalities: None,
                output_modalities: None,
                model_type: Some("chat".to_string()),
                family: None,
                capabilities: Some(crate::capabilities::ModelCapabilities {
                    vision: Some(true),
                    tool_calling: Some(true),
                    reasoning: None,
                    thinking: None,
                    attachment: None,
                    structured_output: None,
                    temperature: None,
                }),
            },
            DiscoveredModel {
                model_id: ModelId::new("claude-sonnet-4-6"),
                display_name: Some("Claude Sonnet 4.6".to_string()),
                target_format: TargetFormat::Anthropic,
                context_length: Some(200_000),
                max_output_tokens: Some(8192),
                input_modalities: None,
                output_modalities: None,
                model_type: Some("chat".to_string()),
                family: None,
                capabilities: Some(crate::capabilities::ModelCapabilities {
                    vision: Some(true),
                    tool_calling: Some(true),
                    reasoning: None,
                    thinking: None,
                    attachment: None,
                    structured_output: None,
                    temperature: None,
                }),
            },
            DiscoveredModel {
                model_id: ModelId::new("claude-opus-4-6-thinking"),
                display_name: Some("Claude Opus 4.6 (Thinking)".to_string()),
                target_format: TargetFormat::Anthropic,
                context_length: Some(200_000),
                max_output_tokens: Some(8192),
                input_modalities: None,
                output_modalities: None,
                model_type: Some("chat".to_string()),
                family: None,
                capabilities: Some(crate::capabilities::ModelCapabilities {
                    vision: Some(true),
                    tool_calling: Some(true),
                    reasoning: None,
                    thinking: None,
                    attachment: None,
                    structured_output: None,
                    temperature: None,
                }),
            },
        ]
    }
}

crate::adapters::derive_default_from_new!(AntigravityAdapter);

impl ProviderAdapter for AntigravityAdapter {
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
            return Ok(self.hardcoded_models());
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

        Ok(self.hardcoded_models())
    }

    async fn execute_custom(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        req: crate::pipeline::PipelineRequest,
        resolved_target: &crate::pipeline::context::ResolvedTarget,
        _ctx: Option<crate::adapters::CustomExecutionContext>,
    ) -> Option<std::result::Result<crate::translation::OpenAIResponse, CoreError>> {
        let custom_meta = resolved_target.custom_meta.as_ref()?;

        let project_id = custom_meta.antigravity_project.as_deref().unwrap_or("");

        let mut custom_req = (*req.openai_request).clone();
        custom_req.model = resolved_target.model.model_id.as_str().to_string();

        Some(
            crate::executor_antigravity::execute_antigravity(
                upstream_client,
                &custom_meta.access_token,
                project_id,
                &custom_req,
                req.client_disconnected.clone(),
                req.stream_sink.as_ref(),
                None,
            )
            .await,
        )
    }
}
