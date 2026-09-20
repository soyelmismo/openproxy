use super::{
    AdapterAuthType, AdapterFormat, Arc, DiscoveredModel, ModelId, ProviderAdapter,
    ProviderAdapterConfig, ProviderId, Result, TargetFormat, UpstreamClient,
};

/// Adapter for TypeSafe AI (<https://api.typesafe.ai/v1>).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TypeSafeAdapter {
    config: ProviderAdapterConfig,
}

impl TypeSafeAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("typesafe"),
                name: "TypeSafe AI".into(),
                anonymous_fallback: false,
                rate_limit_scope: "account".into(),
                base_url: "https://api.typesafe.ai/v1".into(),
                auth_type: AdapterAuthType::Bearer,
                format: AdapterFormat::SystemOne,
                extra_headers: vec![],
            },
        }
    }
}

crate::adapters::derive_default_from_new!(TypeSafeAdapter);

impl ProviderAdapter for TypeSafeAdapter {
    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn fetch_models(
        &self,
        _upstream_client: &Arc<UpstreamClient>,
        _api_key: &str,
    ) -> impl std::future::Future<Output = Result<Vec<DiscoveredModel>>> + Send {
        let base = |id: &str, name: &str, ctx: i64, out: i64| DiscoveredModel {
            model_id: ModelId::new(id),
            display_name: Some(name.into()),
            target_format: TargetFormat::SystemOne,
            context_length: Some(ctx),
            max_output_tokens: Some(out),
            input_modalities: Some(vec!["text".into()].into()),
            output_modalities: Some(vec!["text".into()].into()),
            model_type: Some("decision".into()),
            family: Some("typesafe".into()),
            capabilities: None,
        };

        std::future::ready(Ok(vec![
            base("jev-latest", "TypeSafe Jev (Latest)", 8_192, 1_024),
            base("jev-1.13.0", "TypeSafe Jev 1.13.0", 8_192, 1_024),
        ]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_typesafe_adapter_config() {
        let adapter = TypeSafeAdapter::new();
        assert_eq!(adapter.id().as_str(), "typesafe");
        assert_eq!(adapter.format(), AdapterFormat::SystemOne);
        assert_eq!(adapter.build_system_one_url(), "https://api.typesafe.ai/v1/systemone");
    }
}
