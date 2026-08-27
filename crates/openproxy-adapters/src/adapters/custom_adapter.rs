use super::{
    AdapterAuthType, Arc, CoreError, DiscoveredModel, OpenAIModelEntry, ProviderAdapter,
    ProviderAdapterConfig, ProviderMetadata, Result, TargetFormat, UpstreamClient,
    build_discovered_model_full, build_discovered_model_with, upstream_get_json,
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

fn auth_header_for_type<'a>(
    auth_type: AdapterAuthType,
    api_key: &'a str,
    bearer_buf: &'a str,
) -> Option<(&'static str, &'a str)> {
    if api_key.is_empty() {
        return None;
    }
    match auth_type {
        AdapterAuthType::Bearer | AdapterAuthType::OAuth | AdapterAuthType::None => {
            Some(("Authorization", bearer_buf))
        }
        AdapterAuthType::XApiKey => Some(("x-api-key", api_key)),
        AdapterAuthType::GoogApiKey => Some(("x-goog-api-key", api_key)),
    }
}

fn build_custom_headers<'a>(
    auth_type: AdapterAuthType,
    api_key: &'a str,
    bearer_buf: &'a str,
    extra_headers: &'a [(String, String)],
) -> Vec<(&'a str, &'a str)> {
    let mut headers = Vec::with_capacity(1 + extra_headers.len());
    if let Some((k, v)) = auth_header_for_type(auth_type, api_key, bearer_buf) {
        headers.push((k, v));
    }
    for (k, v) in extra_headers {
        headers.push((k.as_str(), v.as_str()));
    }
    headers
}

fn parse_custom_openai_models(
    body: &serde_json::Value,
    target_format: TargetFormat,
) -> Option<Vec<DiscoveredModel>> {
    let arr = body.get("data")?.as_array()?;
    let models = arr
        .iter()
        .filter_map(|raw| {
            let entry: OpenAIModelEntry = serde::Deserialize::deserialize(raw).ok()?;
            Some(build_discovered_model_with(entry.id, target_format))
        })
        .collect();
    Some(models)
}

fn parse_custom_gemini_models(body: &serde_json::Value) -> Option<Vec<DiscoveredModel>> {
    let arr = body.get("models")?.as_array()?;
    let models = arr
        .iter()
        .filter_map(|m| {
            let full_name = m.get("name")?.as_str()?;
            let id = full_name.strip_prefix("models/").unwrap_or(full_name);
            let display_name = m
                .get("displayName")
                .and_then(|v| v.as_str())
                .map(ToString::to_string);
            Some(build_discovered_model_full(
                id.to_string(),
                display_name,
                TargetFormat::Gemini,
                None,
                None,
            ))
        })
        .collect();
    Some(models)
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

        let bearer_auth = format!("Bearer {api_key}");
        let headers = build_custom_headers(
            self.config.auth_type,
            api_key,
            &bearer_auth,
            &self.config.extra_headers,
        );

        let body = upstream_get_json(upstream_client, &url, &headers)
            .await
            .map_err(|e| {
                CoreError::UpstreamConnection(format!("{} /models: {e}", self.config.id))
            })?;

        if let Some(err_obj) = body.get("error") {
            let msg = err_obj
                .get("message")
                .and_then(|v| v.as_str())
                .or_else(|| err_obj.as_str())
                .unwrap_or("upstream returned error payload");
            return Err(CoreError::UpstreamConnection(format!(
                "{} /models: {msg}",
                self.config.id
            )));
        }

        if let Some(false) = body.get("success").and_then(|v| v.as_bool()) {
            return Err(CoreError::UpstreamConnection(format!(
                "{} /models: upstream returned success=false",
                self.config.id
            )));
        }

        if let Some(models) =
            parse_custom_openai_models(&body, self.config.format.default_target_format())
        {
            if models.is_empty() {
                return Err(CoreError::UpstreamConnection(format!(
                    "{} /models: empty model array returned from upstream",
                    self.config.id
                )));
            }
            return Ok(models);
        }

        if let Some(models) = parse_custom_gemini_models(&body) {
            if models.is_empty() {
                return Err(CoreError::UpstreamConnection(format!(
                    "{} /models: empty model array returned from upstream",
                    self.config.id
                )));
            }
            return Ok(models);
        }

        Err(CoreError::UpstreamConnection(format!(
            "custom adapter: /models response from {} has no recognised shape (expected 'data' or 'models' array)",
            self.config.id
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::AdapterFormat;
    use openproxy_types::{ModelId, ProviderId};

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

    #[test]
    fn test_parse_custom_openai_models_valid() {
        let json = serde_json::json!({
            "data": [
                { "id": "gpt-4o" },
                { "id": "claude-3-5-sonnet" }
            ]
        });
        let models = parse_custom_openai_models(&json, TargetFormat::Openai).unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].model_id.as_str(), "gpt-4o");
        assert_eq!(models[1].model_id.as_str(), "claude-3-5-sonnet");
    }

    #[test]
    fn test_parse_custom_openai_models_error_or_empty() {
        let error_json = serde_json::json!({
            "success": false,
            "error": {
                "message": "Internal error",
                "code": "INTERNAL_ERROR"
            }
        });
        assert!(parse_custom_openai_models(&error_json, TargetFormat::Openai).is_none());

        let empty_json = serde_json::json!({ "data": [] });
        let models = parse_custom_openai_models(&empty_json, TargetFormat::Openai).unwrap();
        assert!(models.is_empty());
    }
}
