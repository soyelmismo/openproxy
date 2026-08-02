use super::*;

pub const CLINE_SPOOFING_HEADERS: &[(&str, &str)] = &[
    ("http-referer", "https://cline.bot"),
    ("x-title", "Cline"),
    ("user-agent", "Cline/4.1.3"),
    ("x-is-multiroot", "false"),
    ("x-client-type", "VSCode Extension"),
    ("x-client-version", "4.1.3"),
    ("x-platform", "Visual Studio Code"),
    ("x-platform-version", "1.96.0"), // Typical VSCode version
    ("x-core-version", "4.1.3"),
];

pub fn apply_cline_spoofing_headers(req: &mut UpstreamRequest) {
    for &(k, v) in CLINE_SPOOFING_HEADERS {
        if let Ok(name) = http::header::HeaderName::try_from(k) {
            if let Ok(val) = http::HeaderValue::try_from(v) {
                req.headers.insert(name, val);
            }
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ClineAdapter {
    config: ProviderAdapterConfig,
}

#[derive(serde::Deserialize, Debug)]
struct ClineRecommendedModels {
    #[serde(default)]
    recommended: Vec<ClineModelEntry>,
    #[serde(default)]
    free: Vec<ClineModelEntry>,
    #[serde(default)]
    #[serde(rename = "clinePass")]
    cline_pass: Vec<ClineModelEntry>,
}

#[derive(serde::Deserialize, Debug)]
struct ClineModelEntry {
    id: String,
    #[serde(default)]
    name: String,
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
                extra_headers: CLINE_SPOOFING_HEADERS
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            },
        }
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
        Some("https://api.cline.bot/api/v1/ai/cline/recommended-models".into())
    }

    async fn fetch_models(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        _api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        let url = self.models_url().unwrap();

        let body = upstream_get_json(upstream_client, &url, &[])
            .await
            .map_err(|e| openproxy_types::error::CoreError::UpstreamConnection(e.to_string()))?;

        let payload: ClineRecommendedModels = serde_json::from_value(body)
            .map_err(|e| openproxy_types::error::CoreError::Parse(format!("cline parse error: {}", e)))?;

        let mut discovered = Vec::new();

        let mut add_models = |entries: Vec<ClineModelEntry>, is_free: bool| {
            for entry in entries {
                let mut id = entry.id.clone();
                if is_free && !id.ends_with(":free") && !id.contains("-free") {
                    id.push_str(":free");
                }
                
                // Fallback capabilities for unknown models
                let caps = openproxy_types::ModelCapabilities {
                    vision: Some(true),
                    tool_calling: Some(true),
                    reasoning: Some(true),
                    thinking: Some(true),
                    attachment: None,
                    structured_output: None,
                    temperature: None,
                };

                discovered.push(DiscoveredModel {
                    model_id: ModelId::new(id),
                    display_name: Some(entry.name),
                    target_format: TargetFormat::Openai,
                    context_length: Some(128000), // Defaulting context
                    max_output_tokens: Some(8192),
                    input_modalities: None,
                    output_modalities: None,
                    model_type: Some("chat".to_string()),
                    family: None,
                    capabilities: Some(caps),
                });
            }
        };

        add_models(payload.recommended, false);
        add_models(payload.free, true);
        add_models(payload.cline_pass, false);

        Ok(discovered)
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
