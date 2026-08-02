use super::*;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ClineAdapter {
    config: ProviderAdapterConfig,
}

impl ClineAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("cline"),
                name: "Cline".into(),
                anonymous_fallback: false,
                rate_limit_scope: "account".into(),
                base_url: "https://api.cline.bot/api/v1".into(),
                auth_type: AdapterAuthType::OAuth,
                format: AdapterFormat::Openai,
                extra_headers: vec![
                    ("HTTP-Referer".into(), "https://cline.bot".into()),
                    ("X-Title".into(), "Cline".into()),
                ],
            },
        }
    }

    fn hardcoded_models(&self) -> Vec<DiscoveredModel> {
        [
            ("zai/glm-5.2", "GLM 5.2", 1040000, 128000),
            ("x-ai/grok-4.5", "Grok 4.5", 500000, 500000),
            ("openai/gpt-5.6-sol", "GPT-5.6 Sol", 1050000, 128000),
            ("moonshotai/kimi-k3", "Kimi K3", 1048576, 1048576),
            ("anthropic/claude-opus-4.8", "Claude Opus 4.8", 1000000, 128000),
            ("openrouter/free", "Free Models Router", 200000, 128000),
            ("deepseek/deepseek-v4-flash", "DeepSeek V4 Flash (Free)", 1048576, 65536),
            ("tencent/hy3:free", "Tencent Hy3 (Free)", 262144, 262144),
            ("stepfun/step-3.7-flash", "Step 3.7 Flash (Free)", 256000, 256000),
            ("poolside/laguna-m.1:free", "Laguna M.1 (Free)", 262144, 32768),
            ("google/gemma-4-31b-it:free", "Gemma 4 31B (Free)", 262144, 32768),
            ("nvidia/nemotron-3-ultra-550b-a55b:free", "Nemotron 3 Ultra (Free)", 1000000, 65536),
            ("minimax/minimax-m3", "MiniMax M3 (Free)", 1048576, 65536),
        ]
        .into_iter()
        .map(|(id, name, ctx, out)| DiscoveredModel {
            model_id: ModelId::new(id),
            display_name: Some(name.to_string()),
            target_format: TargetFormat::Openai,
            context_length: Some(ctx),
            max_output_tokens: Some(out),
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
        })
        .collect()
    }
}

crate::adapters::derive_default_from_new!(ClineAdapter);

impl ProviderAdapter for ClineAdapter {
    fn id(&self) -> &ProviderId {
        &self.config.id
    }

    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn metadata(&self) -> openproxy_types::ProviderMetadata {
        let mut meta = openproxy_types::ProviderMetadata::custom_default();
        meta.built_in = true;
        meta.deletable = false;
        meta.supports_quota = false;
        meta.quota_refresh_supported = false;
        meta.requires_oauth = true;
        meta.oauth_refresh_lead_seconds = Some(300);
        meta
    }

    fn build_chat_url(&self, _target_format: TargetFormat, _model: &ModelId) -> String {
        format!("{}/chat/completions", self.config.base_url)
    }

    fn build_auth_header(&self, access_token: &str) -> Option<(String, String)> {
        Some(("Authorization".into(), format!("Bearer {}", access_token)))
    }

    fn build_headers(
        &self,
        api_key: &str,
        _target_format: TargetFormat,
        _model: &ModelId,
    ) -> Vec<(String, String)> {
        let mut headers = Vec::with_capacity(2 + self.config.extra_headers.len());
        if let Some((name, value)) = self.build_auth_header(api_key) {
            headers.push((name, value));
        }
        headers.push(("Content-Type".into(), "application/json".into()));
        for (k, v) in &self.config.extra_headers {
            headers.push((k.clone(), v.clone()));
        }
        headers
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

    async fn fetch_quota(
        &self,
        _upstream_client: &Arc<UpstreamClient>,
        _api_key: &str,
        _access_token: Option<&str>,
        _provider_specific: Option<&str>,
    ) -> Option<Result<openproxy_types::AccountQuota>> {
        None
    }
}
