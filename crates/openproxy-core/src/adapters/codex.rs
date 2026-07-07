use super::*;

const DEFAULT_CODEX_CLIENT_VERSION: &str = "0.142.0";

fn safe_env_value(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

pub fn codex_client_version() -> String {
    safe_env_value("OPENPROXY_CODEX_CLIENT_VERSION")
        .or_else(|| safe_env_value("CODEX_CLIENT_VERSION"))
        .unwrap_or_else(|| DEFAULT_CODEX_CLIENT_VERSION.to_string())
}

pub fn codex_user_agent() -> String {
    safe_env_value("OPENPROXY_CODEX_USER_AGENT")
        .or_else(|| safe_env_value("CODEX_USER_AGENT"))
        .unwrap_or_else(|| {
            format!(
                "codex-cli/{} (Windows 10.0.26200; x64)",
                codex_client_version()
            )
        })
}
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
                format: AdapterFormat::Responses,
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
            target_format: TargetFormat::Responses,
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

    fn metadata(&self) -> crate::providers::ProviderMetadata {
        crate::providers::ProviderMetadata {
            built_in: crate::providers::is_builtin(self.id().as_str()),
            deletable: !crate::providers::is_builtin(self.id().as_str()),
            supports_quota: true,
            quota_refresh_supported: false,
        }
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
                crate::adapters::codex::codex_client_version(),
            ),
            (
                "User-Agent".into(),
                crate::adapters::codex::codex_user_agent(),
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
}
