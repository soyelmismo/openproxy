use crate::adapters::{AdapterAuthType, AdapterFormat, ProviderAdapter, ProviderAdapterConfig};
use openproxy_types::{DiscoveredModel, ModelId, ProviderId, TargetFormat};

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct MockAdapter {
    pub config: ProviderAdapterConfig,
    #[serde(skip)]
    pub call_count: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
    #[serde(skip)]
    pub models_to_return: Option<Vec<DiscoveredModel>>,
    pub fail_fetch: bool,
}

impl MockAdapter {
    pub fn new(id: &str, base_url: String, format: AdapterFormat) -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new(id),
                base_url,
                auth_type: AdapterAuthType::Bearer,
                format,
                extra_headers: Vec::new(),
            },
            call_count: None,
            models_to_return: None,
            fail_fetch: false,
        }
    }

    pub fn with_discovery(
        id: &str,
        models: Vec<DiscoveredModel>,
    ) -> (Self, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut adapter = Self::new(id, String::new(), AdapterFormat::Openai);
        adapter.call_count = Some(counter.clone());
        adapter.models_to_return = Some(models);
        (adapter, counter)
    }

    pub fn failing_discovery(id: &str) -> Self {
        let mut adapter = Self::new(id, String::new(), AdapterFormat::Openai);
        adapter.fail_fetch = true;
        adapter
    }
}

impl std::fmt::Debug for MockAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockAdapter")
            .field("config", &self.config)
            .field("fail_fetch", &self.fail_fetch)
            .finish()
    }
}

impl ProviderAdapter for MockAdapter {
    fn id(&self) -> &ProviderId {
        &self.config.id
    }
    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }
    fn build_chat_url(&self, _target_format: TargetFormat, _model: &ModelId) -> String {
        self.config.base_url.clone()
    }
    fn build_auth_header(&self, api_key: &str) -> Option<(String, String)> {
        Some(("Authorization".into(), format!("Bearer {}", api_key)))
    }
    fn build_headers(
        &self,
        api_key: &str,
        _target_format: TargetFormat,
        _model: &ModelId,
    ) -> Vec<(String, String)> {
        let mut headers = vec![("Content-Type".into(), "application/json".into())];
        if let Some(auth) = self.build_auth_header(api_key) {
            headers.push(auth);
        }
        headers
    }
    fn models_url(&self) -> Option<String> {
        None
    }
    async fn fetch_models(
        &self,
        _upstream_client: &std::sync::Arc<crate::upstream::UpstreamClient>,
        _api_key: &str,
    ) -> openproxy_types::Result<Vec<DiscoveredModel>> {
        if self.fail_fetch {
            return Err(openproxy_types::error::CoreError::Internal(
                "simulated upstream 500".into(),
            ));
        }
        if let Some(counter) = &self.call_count {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        if let Some(models) = &self.models_to_return {
            Ok(models.clone())
        } else {
            Ok(Vec::new())
        }
    }
}
