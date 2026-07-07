use super::*;

pub struct CodexAdapter {
    config: ProviderAdapterConfig,
}

impl CodexAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("codex"),
                base_url: "https://chatgpt.com/backend-api/codex".into(),
                auth_type: AdapterAuthType::Bearer,
                format: AdapterFormat::Openai,
                extra_headers: vec![],
            },
        }
    }

    fn hardcoded_models(&self) -> Vec<DiscoveredModel> {
        [
            ("gpt-5.5", "GPT-5.5"),
            ("gpt-5.5-xhigh", "GPT-5.5 (xhigh)"),
            ("gpt-5.5-high", "GPT-5.5 (high)"),
            ("gpt-5.5-medium", "GPT-5.5 (medium)"),
            ("gpt-5.5-low", "GPT-5.5 (low)"),
            ("gpt-5.4", "GPT-5.4"),
            ("gpt-5.4-xhigh", "GPT-5.4 (xhigh)"),
            ("gpt-5.4-high", "GPT-5.4 (high)"),
            ("gpt-5.4-medium", "GPT-5.4 (medium)"),
            ("gpt-5.4-low", "GPT-5.4 (low)"),
            ("gpt-5.4-mini", "GPT-5.4 Mini"),
            ("gpt-5.3-codex", "GPT-5.3 Codex"),
            ("gpt-5.3-codex-spark", "GPT-5.3 Codex Spark"),
        ]
        .into_iter()
        .map(|(id, name)| DiscoveredModel {
            model_id: ModelId::new(id),
            display_name: Some(name.to_string()),
            target_format: TargetFormat::Openai,
            context_length: Some(400_000),
            max_output_tokens: Some(32_768),
            input_modalities: None,
            output_modalities: None,
            model_type: Some("chat".to_string()),
            family: Some("gpt".to_string()),
            capabilities: Some(crate::capabilities::ModelCapabilities {
                vision: Some(false),
                tool_calling: Some(true),
                reasoning: Some(true),
                thinking: Some(true),
                attachment: None,
                structured_output: None,
                temperature: None,
            }),
        })
        .collect()
    }
}

impl Default for CodexAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ProviderAdapter for CodexAdapter {
    fn id(&self) -> &ProviderId {
        &self.config.id
    }

    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn build_chat_url(&self, _target_format: TargetFormat, _model: &ModelId) -> String {
        format!("{}/responses", self.config.base_url)
    }

    fn build_auth_header(&self, api_key: &str) -> (String, String) {
        ("Authorization".into(), format!("Bearer {}", api_key))
    }

    fn build_headers(
        &self,
        api_key: &str,
        _target_format: TargetFormat,
        _model: &ModelId,
    ) -> Vec<(String, String)> {
        let (name, value) = self.build_auth_header(api_key);
        vec![
            (name, value),
            ("Content-Type".into(), "application/json".into()),
            ("Origin".into(), "https://chatgpt.com".into()),
            ("originator".into(), "codex_cli_rs".into()),
            (
                "Version".into(),
                crate::executor_codex::codex_client_version(),
            ),
            (
                "User-Agent".into(),
                crate::executor_codex::codex_user_agent(),
            ),
        ]
    }

    fn models_url(&self) -> Option<String> {
        None
    }

    async fn fetch_models(
        &self,
        _upstream_client: &Arc<UpstreamClient>,
        _api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        Ok(self.hardcoded_models())
    }

    async fn execute_custom(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        req: Arc<crate::pipeline::PipelineRequest>,
        resolved_target: &crate::pipeline::context::ResolvedTarget,
        ctx: Option<crate::adapters::CustomExecutionContext>,
    ) -> Option<std::result::Result<crate::translation::OpenAIResponse, CoreError>> {
        let custom_meta = resolved_target.custom_meta.as_ref()?;
        let mut custom_req = (*req.openai_request).clone();
        custom_req.model = resolved_target.model.model_id.as_str().to_string();

        Some(
            crate::executor_codex::execute_codex(
                upstream_client,
                &custom_meta.access_token,
                custom_meta.codex_workspace_id.as_deref(),
                &custom_req,
                req.client_disconnected.clone(),
                req.stream_sink.as_ref(),
                None,
                ctx,
                resolved_target.target.account_id,
            )
            .await,
        )
    }
}
