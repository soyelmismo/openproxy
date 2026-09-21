use super::{
    AdapterAuthType, AdapterFormat, Arc, DiscoveredModel, ModelId, ProviderAdapter,
    ProviderAdapterConfig, ProviderId, Result, TargetFormat, UpstreamClient,
};

/// Adapter for local or self-hosted Laya (ModernBERT/mmBERT) System One models.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct LayaAdapter {
    config: ProviderAdapterConfig,
}

impl LayaAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("laya"),
                name: "Laya (Self-Hosted)".into(),
                anonymous_fallback: true,
                rate_limit_scope: "account".into(),
                base_url: "http://localhost:8770/v1".into(),
                auth_type: AdapterAuthType::None,
                format: AdapterFormat::SystemOne,
                extra_headers: vec![],
            },
        }
    }
}

crate::adapters::derive_default_from_new!(LayaAdapter);

impl ProviderAdapter for LayaAdapter {
    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn is_anonymous_fallback(&self) -> bool {
        true
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
            family: Some("laya".into()),
            capabilities: None,
        };

        std::future::ready(Ok(vec![
            base("laya", "Convai Laya (ModernBERT)", 8_192, 1_024),
            base(
                "laya-multilingual",
                "Convai Laya Multilingual (mmBERT)",
                8_192,
                1_024,
            ),
            base(
                "laya-typed-decisions",
                "Convai Laya Typed Decisions",
                8_192,
                1_024,
            ),
        ]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_laya_adapter_config() {
        let adapter = LayaAdapter::new();
        assert_eq!(adapter.id().as_str(), "laya");
        assert!(adapter.is_anonymous_fallback());
        assert_eq!(adapter.format(), AdapterFormat::SystemOne);
        assert_eq!(
            adapter.build_system_one_url(),
            "http://localhost:8770/v1/systemone"
        );
    }
}
