use super::*;

// =====================================================================
// Custom (user-defined) adapter
// =====================================================================

/// Generic adapter for user-created providers stored in the DB.
///
/// Unlike the built-in adapters whose config is hardcoded, a
/// `CustomAdapter` derives its `base_url`, `auth_type`, `format`, and
/// `extra_headers` from the [`providers::Provider`] row at construction
/// time. This enables model refresh, chat routing, and other code paths
/// that require a `ProviderAdapter` to work with custom providers.
pub struct CustomAdapter {
    config: ProviderAdapterConfig,
}

impl CustomAdapter {
    /// Build a `CustomAdapter` from a DB provider row.
    ///
    /// Maps [`providers::AuthType`] → [`AdapterAuthType`] and
    /// [`providers::ProviderFormat`] → [`AdapterFormat`]. Extra headers
    /// are deserialised from the JSON string stored in the row (falling
    /// back to an empty vec on parse failure).
    pub fn from_provider_row(provider: &crate::providers::Provider) -> Self {
        let auth_type = match provider.auth_type {
            crate::providers::AuthType::Bearer => AdapterAuthType::Bearer,
            crate::providers::AuthType::XApiKey => AdapterAuthType::XApiKey,
            crate::providers::AuthType::GoogApiKey => AdapterAuthType::GoogApiKey,
            // OAuth tokens are still passed as Bearer on the wire.
            crate::providers::AuthType::OAuth => AdapterAuthType::Bearer,
            crate::providers::AuthType::None => AdapterAuthType::None,
        };

        let format = match provider.format {
            crate::providers::ProviderFormat::Openai => AdapterFormat::Openai,
            crate::providers::ProviderFormat::Anthropic => AdapterFormat::Anthropic,
            crate::providers::ProviderFormat::Mixed => AdapterFormat::Mixed,
            crate::providers::ProviderFormat::Gemini => AdapterFormat::Gemini,
            crate::providers::ProviderFormat::Responses => AdapterFormat::Responses,
        };

        let extra_headers: Vec<(String, String)> = provider
            .extra_headers_json
            .as_deref()
            .and_then(|json_str| {
                // The JSON is stored as an object like
                // `{"X-Title":"openproxy"}` — deserialize into a HashMap
                // and convert to Vec<(String, String)>.
                let map: Option<std::collections::HashMap<String, String>> =
                    serde_json::from_str(json_str).ok();
                map.map(|m| m.into_iter().collect())
            })
            .unwrap_or_default();

        Self {
            config: ProviderAdapterConfig {
                id: provider.id.clone(),
                base_url: provider.base_url.clone(),
                auth_type,
                format,
                extra_headers,
            },
        }
    }
}

#[async_trait]
impl ProviderAdapter for CustomAdapter {
    fn id(&self) -> &ProviderId {
        &self.config.id
    }

    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn build_chat_url(&self, target_format: TargetFormat, model: &ModelId) -> String {
        match self.config.format {
            AdapterFormat::Openai => format!("{}/chat/completions", self.config.base_url),
            AdapterFormat::Anthropic => format!("{}/messages", self.config.base_url),
            AdapterFormat::Gemini => {
                format!(
                    "{}/models/{}:streamGenerateContent?alt=sse",
                    self.config.base_url,
                    model.as_str()
                )
            }
            AdapterFormat::Responses => format!("{}/responses", self.config.base_url),
            AdapterFormat::Mixed => match target_format {
                TargetFormat::Openai => format!("{}/chat/completions", self.config.base_url),
                TargetFormat::Anthropic => format!("{}/messages", self.config.base_url),
                TargetFormat::Gemini => format!("{}/chat/completions", self.config.base_url),
                TargetFormat::Responses => format!("{}/responses", self.config.base_url),
            },
        }
    }

    fn build_auth_header(&self, api_key: &str) -> (String, String) {
        match self.config.auth_type {
            AdapterAuthType::Bearer => ("Authorization".into(), format!("Bearer {}", api_key)),
            AdapterAuthType::XApiKey => ("x-api-key".into(), api_key.to_string()),
            AdapterAuthType::GoogApiKey => ("x-goog-api-key".into(), api_key.to_string()),
            AdapterAuthType::None => (String::new(), String::new()),
        }
    }

    fn build_headers(
        &self,
        api_key: &str,
        _target_format: TargetFormat,
        _model: &ModelId,
    ) -> Vec<(String, String)> {
        let (name, value) = self.build_auth_header(api_key);
        let mut headers = Vec::with_capacity(2 + self.config.extra_headers.len());
        if !name.is_empty() && !api_key.is_empty() {
            headers.push((name, value));
        }
        headers.push(("Content-Type".into(), "application/json".into()));
        for (k, v) in &self.config.extra_headers {
            headers.push((k.clone(), v.clone()));
        }
        headers
    }

    fn models_url(&self) -> Option<String> {
        Some(format!("{}/models", self.config.base_url))
    }

    async fn fetch_models(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        let url = self.models_url().ok_or_else(|| {
            CoreError::Internal(format!(
                "{}: models_url is None (impossible)",
                self.config.id
            ))
        })?;

        // Build auth header based on the provider's auth type.
        let auth_headers: Vec<(&str, String)> = if api_key.is_empty() {
            vec![]
        } else {
            match self.config.auth_type {
                AdapterAuthType::Bearer => vec![("Authorization", format!("Bearer {api_key}"))],
                AdapterAuthType::XApiKey => vec![("x-api-key", api_key.to_string())],
                AdapterAuthType::GoogApiKey => vec![("x-goog-api-key", api_key.to_string())],
                AdapterAuthType::None => vec![],
            }
        };

        let body = upstream_get_json(upstream_client, &url, &auth_headers)
            .await
            .map_err(|e| {
                CoreError::UpstreamConnection(format!("{} /models: {e}", self.config.id))
            })?;

        // Try OpenAI format first: {"data": [{"id": "...", ...}]}
        if let Some(arr) = body.get("data").and_then(|v| v.as_array()) {
            let target_format = match self.config.format {
                AdapterFormat::Openai => TargetFormat::Openai,
                AdapterFormat::Anthropic => TargetFormat::Anthropic,
                AdapterFormat::Gemini => TargetFormat::Gemini,
                AdapterFormat::Responses => TargetFormat::Responses,
                // For Mixed providers, default to Openai; the model's
                // stored target_format in the DB will be used at routing
                // time.
                AdapterFormat::Mixed => TargetFormat::Openai,
            };

            let models: Vec<DiscoveredModel> = arr
                .iter()
                .filter_map(|raw| {
                    let entry: OpenAIModelEntry = serde_json::from_value(raw.clone()).ok()?;
                    let id = entry.id;
                    Some(DiscoveredModel {
                        model_id: ModelId::new(id.clone()),
                        display_name: Some(id),
                        target_format,
                        context_length: None,
                        max_output_tokens: None,
                        input_modalities: None,
                        output_modalities: None,
                        model_type: None,
                        family: None,
                        capabilities: None,
                    })
                })
                .collect();
            return Ok(models);
        }

        // Try Gemini format: {"models": [{"name": "models/...", ...}]}
        if let Some(arr) = body.get("models").and_then(|v| v.as_array()) {
            let models: Vec<DiscoveredModel> = arr
                .iter()
                .filter_map(|m| {
                    let full_name = m.get("name").and_then(|v| v.as_str())?;
                    let id = full_name.strip_prefix("models/").unwrap_or(full_name);
                    let display_name = m
                        .get("displayName")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| id.to_string());
                    Some(DiscoveredModel {
                        model_id: ModelId::new(id.to_string()),
                        display_name: Some(display_name),
                        target_format: TargetFormat::Gemini,
                        context_length: None,
                        max_output_tokens: None,
                        input_modalities: None,
                        output_modalities: None,
                        model_type: None,
                        family: None,
                        capabilities: None,
                    })
                })
                .collect();
            return Ok(models);
        }

        // Unrecognised response shape — return empty rather than
        // erroring so the provider can still be used with manually
        // added models.
        tracing::warn!(
            provider = %self.config.id,
            url = %url,
            "custom adapter: /models response has no recognised shape (expected 'data' or 'models' array); returning empty list"
        );
        Ok(vec![])
    }
}
