use super::{
    AdapterAuthType, AdapterFormat, Arc, CoreError, DiscoveredModel, ModelId, OpenAIModelEntry,
    ProviderAdapter, ProviderAdapterConfig, ProviderMetadata, Result, TargetFormat, UpstreamClient,
    upstream_get_json,
};

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
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CustomAdapter {
    config: ProviderAdapterConfig,
}

impl CustomAdapter {
    /// Build a `CustomAdapter` directly from `ProviderAdapterConfig`.
    pub fn from_config(config: ProviderAdapterConfig) -> Self {
        Self { config }
    }

    /// Build a `CustomAdapter` from a DB provider row.
    ///
    /// Parses the row's `auth_type` and `format` strings into the
    /// strongly-typed adapter enums, deserializes `extra_headers_json`
    /// (defaulting to empty on parse error or `NULL`), and fills the
    /// rest of the config fields.
    pub fn from_provider_row(provider: &openproxy_types::Provider) -> Self {
        let auth_type = match provider.auth_type {
            openproxy_types::AuthType::OAuth | openproxy_types::AuthType::Bearer => {
                AdapterAuthType::Bearer
            }
            openproxy_types::AuthType::XApiKey => AdapterAuthType::XApiKey,
            openproxy_types::AuthType::GoogApiKey => AdapterAuthType::GoogApiKey,
            openproxy_types::AuthType::None => AdapterAuthType::None,
        };

        let format = provider.format;

        let extra_headers: Vec<(String, String)> = provider
            .extra_headers_json
            .as_deref()
            .and_then(|raw| {
                serde_json::from_str::<std::collections::HashMap<String, String>>(raw).ok()
            })
            .map(|map| map.into_iter().collect())
            .unwrap_or_default();

        Self {
            config: ProviderAdapterConfig {
                id: provider.id.clone(),
                name: provider.name.clone(),
                base_url: provider.base_url.clone(),
                auth_type,
                format,
                extra_headers,
                anonymous_fallback: false, // Custom adapters don't have this yet, false by default
                rate_limit_scope: provider.rate_limit_scope.as_str().to_string(),
            },
        }
    }

    pub fn from_provider(provider: &openproxy_types::Provider) -> Self {
        Self::from_provider_row(provider)
    }
}

impl ProviderAdapter for CustomAdapter {
    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn metadata(&self) -> ProviderMetadata {
        ProviderMetadata::custom_default()
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

        // Build headers: auth header + extra headers configured for this provider.
        let bearer_auth = format!("Bearer {api_key}");
        let mut headers: Vec<(&str, &str)> =
            Vec::with_capacity(1 + self.config.extra_headers.len());
        if !api_key.is_empty() {
            match self.config.auth_type {
                AdapterAuthType::Bearer | AdapterAuthType::OAuth => {
                    headers.push(("Authorization", bearer_auth.as_str()));
                }
                AdapterAuthType::XApiKey => headers.push(("x-api-key", api_key)),
                AdapterAuthType::GoogApiKey => headers.push(("x-goog-api-key", api_key)),
                AdapterAuthType::None => {
                    headers.push(("Authorization", bearer_auth.as_str()));
                }
            }
        }
        for (k, v) in &self.config.extra_headers {
            headers.push((k.as_str(), v.as_str()));
        }

        let body = upstream_get_json(upstream_client, &url, &headers)
            .await
            .map_err(|e| {
                CoreError::UpstreamConnection(format!("{} /models: {e}", self.config.id))
            })?;

        // Try OpenAI format first: {"data": [{"id": "...", ...}]}
        if let Some(arr) = body.get("data").and_then(|v| v.as_array()) {
            let target_format = match self.config.format {
                AdapterFormat::Anthropic => TargetFormat::Anthropic,
                AdapterFormat::Gemini => TargetFormat::Gemini,
                AdapterFormat::Responses => TargetFormat::Responses,
                AdapterFormat::Atomesus => TargetFormat::Atomesus,
                // For Mixed providers, default to Openai; the model's
                // stored target_format in the DB will be used at routing
                // time.
                AdapterFormat::Openai | AdapterFormat::Mixed => TargetFormat::Openai,
            };

            let models: Vec<DiscoveredModel> = arr
                .iter()
                .filter_map(|raw| {
                    let entry: OpenAIModelEntry = serde::Deserialize::deserialize(raw).ok()?;
                    let id = entry.id;
                    let m_type = openproxy_types::capabilities::infer_model_type(&id);
                    let caps = openproxy_types::capabilities::infer_capabilities(&id);
                    let in_mods =
                        openproxy_types::capabilities::infer_input_modalities_for_model(&id, &caps);
                    let out_mods = openproxy_types::capabilities::infer_output_modalities(&id);
                    let family = openproxy_types::capabilities::infer_family(&id);
                    Some(DiscoveredModel {
                        display_name: Some(id.clone()),
                        model_id: ModelId::new(id),
                        target_format,
                        context_length: None,
                        max_output_tokens: None,
                        input_modalities: Some(in_mods.into_iter().map(String::from).collect()),
                        output_modalities: Some(out_mods.into_iter().map(String::from).collect()),
                        model_type: Some(m_type.to_string()),
                        family,
                        capabilities: Some(caps),
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
                        .map_or_else(|| id.to_string(), std::string::ToString::to_string);
                    let m_type = openproxy_types::capabilities::infer_model_type(id);
                    let caps = openproxy_types::capabilities::infer_capabilities(id);
                    let in_mods =
                        openproxy_types::capabilities::infer_input_modalities_for_model(id, &caps);
                    let out_mods = openproxy_types::capabilities::infer_output_modalities(id);
                    let family = openproxy_types::capabilities::infer_family(id);
                    Some(DiscoveredModel {
                        model_id: ModelId::new(id.to_string()),
                        display_name: Some(display_name),
                        target_format: TargetFormat::Gemini,
                        context_length: None,
                        max_output_tokens: None,
                        input_modalities: Some(in_mods.into_iter().map(String::from).collect()),
                        output_modalities: Some(out_mods.into_iter().map(String::from).collect()),
                        model_type: Some(m_type.to_string()),
                        family,
                        capabilities: Some(caps),
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

#[cfg(test)]
mod tests {
    use super::*;
    use openproxy_types::ProviderId;

    #[test]
    fn test_build_chat_url() {
        let adapter = CustomAdapter::from_config(ProviderAdapterConfig {
            id: ProviderId::new("test"),
            name: "Test".to_string(),
            base_url: "https://api.test.com/v1".to_string(),
            auth_type: AdapterAuthType::Bearer,
            format: AdapterFormat::Mixed,
            extra_headers: vec![],
            anonymous_fallback: false,
            rate_limit_scope: "account".into(),
        });

        let model = ModelId::new("model-a");
        assert_eq!(
            adapter.build_chat_url(TargetFormat::Openai, &model),
            "https://api.test.com/v1/chat/completions"
        );
        assert_eq!(
            adapter.build_chat_url(TargetFormat::Anthropic, &model),
            "https://api.test.com/v1/messages"
        );
        assert_eq!(
            adapter.build_chat_url(TargetFormat::Gemini, &model),
            "https://api.test.com/v1/chat/completions"
        );
        assert_eq!(
            adapter.build_chat_url(TargetFormat::Responses, &model),
            "https://api.test.com/v1/responses"
        );
    }

    #[test]
    fn test_custom_adapter_metadata_deletable() {
        let adapter = CustomAdapter::from_config(ProviderAdapterConfig {
            id: ProviderId::new("custom-provider"),
            name: "Custom Provider".to_string(),
            base_url: "https://api.custom.com/v1".to_string(),
            auth_type: AdapterAuthType::Bearer,
            format: AdapterFormat::Openai,
            extra_headers: vec![],
            anonymous_fallback: false,
            rate_limit_scope: "account".into(),
        });

        let meta = adapter.metadata();
        assert!(!meta.built_in, "custom adapter must not be built-in");
        assert!(meta.deletable, "custom adapter must be deletable");
    }
}
