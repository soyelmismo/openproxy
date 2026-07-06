use super::*;

// =====================================================================
// MiniMax (Coding)
// =====================================================================

/// Adapter for MiniMax's Anthropic-compatible coding endpoint.
///
/// The base URL is `https://api.minimax.io` (the bare host, no path). The
/// chat endpoint is reached by appending `/anthropic/v1/messages?beta=true`
/// at request time, and the model-discovery endpoint is reached by
/// appending `/v1/models`. Splitting the two paths this way is what lets
/// the same `base_url` serve both surfaces without one being a substring
/// of the other.
pub struct MiniMaxAdapter {
    config: ProviderAdapterConfig,
}

impl MiniMaxAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("minimax"),
                base_url: "https://api.minimax.io".into(),
                auth_type: AdapterAuthType::Bearer,
                format: AdapterFormat::Anthropic,
                extra_headers: vec![("Anthropic-Version".into(), "2023-06-01".into())],
            },
        }
    }
}

impl Default for MiniMaxAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ProviderAdapter for MiniMaxAdapter {
    fn id(&self) -> &ProviderId {
        &self.config.id
    }

    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn build_chat_url(&self, _target_format: TargetFormat, _model: &ModelId) -> String {
        // MiniMax exposes the Anthropic Messages API at /anthropic/v1/messages.
        // The `?beta=true` query parameter is required to enable the relevant
        // beta features (tool use, prompt caching, etc.).
        format!("{}/anthropic/v1/messages?beta=true", self.config.base_url)
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
        let mut headers = Vec::with_capacity(2 + self.config.extra_headers.len());
        headers.push((name, value));
        headers.push(("Content-Type".into(), "application/json".into()));
        for (k, v) in &self.config.extra_headers {
            headers.push((k.clone(), v.clone()));
        }
        headers
    }

    fn models_url(&self) -> Option<String> {
        // MiniMax exposes its model catalogue at /v1/models (separate from
        // the /anthropic/v1/ chat surface). The auth scheme is the same
        // Bearer token.
        Some(format!("{}/v1/models", self.config.base_url))
    }

    async fn fetch_models(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        let url = self.models_url().ok_or_else(|| {
            CoreError::Internal("minimax: models_url is None (impossible)".into())
        })?;

        // MiniMax returns a small payload and the request is
        // fast; we let `TimeoutProfile::ModelDiscovery` drive the
        // budget (30s headers / 60s body / 120s total).
        let body = upstream_get_json(
            upstream_client,
            &url,
            &[("Authorization", format!("Bearer {api_key}"))],
        )
        .await
        .map_err(|e| CoreError::UpstreamConnection(format!("minimax /v1/models: {e}")))?;

        // MiniMax returns an OpenAI-shaped list: {"object": "list",
        // "data": [{...}, ...]}. We accept a few equivalent shapes to
        // stay forward-compatible.
        let arr = body
            .get("data")
            .or_else(|| body.get("models"))
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                CoreError::Parse("minimax /v1/models: missing 'data' or 'models' array".into())
            })?;

        let out = arr
            .iter()
            .filter_map(|m| {
                let id = m.get("id").and_then(|v| v.as_str())?;
                let name = m
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| id.to_string());
                Some(DiscoveredModel {
                    model_id: ModelId::new(id.to_string()),
                    display_name: Some(name),
                    // MiniMax's chat surface is Anthropic Messages; every
                    // discovered model goes there.
                    target_format: TargetFormat::Anthropic,
                    // MiniMax's /v1/models response doesn't expose
                    // context_length / modalities / capabilities — leave
                    // them as None so the runtime fallback in
                    // `GET /v1/models` fills them in via heuristics.
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
        Ok(out)
    }
}

