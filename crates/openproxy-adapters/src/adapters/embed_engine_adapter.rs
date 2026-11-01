use super::{
    AdapterAuthType, AdapterFormat, Arc, DiscoveredModel, ModelId, ProviderAdapter,
    ProviderAdapterConfig, ProviderId, Result, TargetFormat, UpstreamClient,
};

/// Adapter for the native EmbeddingGemma 2 engine.
///
/// The engine runs in-process behind a worker sandbox, so this adapter exists
/// to give the provider a row, a routable identity and a model list. No request
/// ever leaves the host: `dispatch_single_embedding` short-circuits on the
/// provider id before any URL is built, so `base_url` is inert bookkeeping.
///
/// Mirrors [`super::laya::LayaAdapter`], the other native engine.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct EmbedEngineAdapter {
    config: ProviderAdapterConfig,
}

impl EmbedEngineAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("embeddinggemma"),
                name: "EmbeddingGemma 2 (Native)".into(),
                anonymous_fallback: true,
                rate_limit_scope: "account".into(),
                base_url: "http://127.0.0.1:8787/v1".into(),
                auth_type: AdapterAuthType::None,
                format: AdapterFormat::Openai,
                extra_headers: vec![],
            },
        }
    }
}

crate::adapters::derive_default_from_new!(EmbedEngineAdapter);

impl ProviderAdapter for EmbedEngineAdapter {
    crate::adapters::impl_adapter_config!();

    fn is_anonymous_fallback(&self) -> bool {
        true
    }

    fn fetch_models(
        &self,
        _upstream_client: &Arc<UpstreamClient>,
        _api_key: &str,
    ) -> impl std::future::Future<Output = Result<Vec<DiscoveredModel>>> + Send {
        // Two aliases, one per task side. `dispatch_single_embedding` picks the
        // prompt prefix from the alias: anything containing "doc" embeds corpus
        // text, everything else is a query. Mixing the two sides collapses
        // retrieval quality, so both are exposed rather than one generic name.
        let base = |id: &str, name: &str| DiscoveredModel {
            model_id: ModelId::new(id),
            display_name: Some(name.into()),
            target_format: TargetFormat::Openai,
            // Embeddings consume tokens but emit a vector, so output length is
            // meaningless; the reference provider leaves both null.
            context_length: None,
            max_output_tokens: None,
            input_modalities: Some(vec!["text".into()].into()),
            output_modalities: Some(vec!["text".into()].into()),
            model_type: Some("embedding".into()),
            family: Some("embeddinggemma".into()),
            capabilities: None,
        };

        std::future::ready(Ok(vec![
            base("embeddinggemma", "EmbeddingGemma 2 (query)"),
            base("embeddinggemma-doc", "EmbeddingGemma 2 (document)"),
        ]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_embed_engine_adapter_config() {
        let adapter = EmbedEngineAdapter::new();
        assert_eq!(adapter.id().as_str(), "embeddinggemma");
        assert!(adapter.is_anonymous_fallback());
        assert_eq!(adapter.format(), AdapterFormat::Openai);
        assert_eq!(adapter.auth_type(), AdapterAuthType::None);
        assert_eq!(
            adapter.build_embeddings_url(&openproxy_types::ModelId::new("embeddinggemma")),
            "http://127.0.0.1:8787/v1/embeddings"
        );
    }

    #[tokio::test]
    async fn test_embed_engine_fetch_models() {
        let adapter = EmbedEngineAdapter::new();
        let client = Arc::new(UpstreamClient::new());
        let models = adapter
            .fetch_models(&client, "")
            .await
            .expect("fetch_models should succeed");

        assert_eq!(models.len(), 2);
        assert_eq!(models[0].model_id.as_str(), "embeddinggemma");
        assert_eq!(models[1].model_id.as_str(), "embeddinggemma-doc");
        for model in &models {
            assert_eq!(model.target_format, TargetFormat::Openai);
            assert_eq!(model.model_type.as_deref(), Some("embedding"));
        }
    }

    #[test]
    fn test_document_alias_is_distinguishable_from_query() {
        // dispatch_single_embedding routes on `contains("doc")`; guard the
        // contract the adapter's model list depends on.
        let adapter = EmbedEngineAdapter::new();
        assert!("embeddinggemma-doc".contains("doc"));
        assert!(!adapter.id().as_str().contains("doc"));
    }
}
