use super::{
    Arc, CoreError, Deserialize, DiscoveredModel, ModelId, Result, TargetFormat, UpstreamClient,
    upstream_get_json,
};

// =====================================================================
// Ollama Cloud
// =====================================================================

declare_openai_adapter!(
    /// Adapter for <https://ollama.com>.
    ///
    /// Ollama Cloud speaks OpenAI-compatible `/v1/chat/completions` with
    /// Bearer auth. Model IDs use Ollama's `:` convention (e.g.
    /// `gemma4:31b`, `qwen3.5:397b`) — the colon is valid inside JSON
    /// strings so no special escaping is needed in the request body.
    OllamaCloudAdapter,
    id: "ollama-cloud",
    name: "Ollama Cloud",
    base_url: "https://ollama.com/v1",
    models_url: "https://ollama.com/api/tags",
    custom_impl: {
        async fn fetch_models(
            &self,
            upstream_client: &Arc<UpstreamClient>,
            api_key: &str,
        ) -> Result<Vec<DiscoveredModel>> {
            let url = self.models_url().ok_or_else(|| {
                CoreError::Internal("ollama-cloud: models_url is None (impossible)".into())
            })?;

            let auth = format!("Bearer {api_key}");
            let body = upstream_get_json(
                upstream_client,
                &url,
                &[("Authorization", &auth)],
            )
            .await
            .map_err(|e| CoreError::UpstreamConnection(format!("ollama-cloud /api/tags: {e}")))?;

            let payload: OllamaTagsResponse =
                <OllamaTagsResponse as serde::Deserialize>::deserialize(&body)
                    .map_err(|e| CoreError::Parse(format!("ollama-cloud /api/tags parse: {e}")))?;

            let out = payload
                .models
                .into_iter()
                .map(|m| {
                    let id = m.name.unwrap_or_default();
                    let family = derive_ollama_family(&id);
                    let display_name = m.display_name.or_else(|| Some(id.clone()));
                    let m_type = openproxy_types::capabilities::infer_model_type(&id);
                    let caps = openproxy_types::capabilities::infer_capabilities(&id);
                    let in_mods =
                        openproxy_types::capabilities::infer_input_modalities_for_model(&id, &caps);
                    let out_mods = openproxy_types::capabilities::infer_output_modalities(&id);
                    DiscoveredModel {
                        model_id: ModelId::new(id),
                        display_name,
                        target_format: TargetFormat::Openai,
                        context_length: None,
                        max_output_tokens: None,
                        input_modalities: Some(in_mods.into_iter().map(String::from).collect()),
                        output_modalities: Some(out_mods.into_iter().map(String::from).collect()),
                        model_type: Some(m_type.to_string()),
                        family,
                        capabilities: Some(caps),
                    }
                })
                .collect();
            Ok(out)
        }
    }
);

/// Response shape of `GET https://ollama.com/api/tags`.
#[derive(Debug, Deserialize)]
struct OllamaTagsResponse {
    #[serde(default)]
    models: Vec<OllamaTagEntry>,
}

#[derive(Debug, Deserialize)]
struct OllamaTagEntry {
    name: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
}

/// Best-effort family extraction from an Ollama model id.
fn derive_ollama_family(id: &str) -> Option<String> {
    let lower = id.to_ascii_lowercase();
    if lower.contains("deepseek") {
        return Some("deepseek".into());
    }
    if lower.contains("kimi") {
        return Some("kimi".into());
    }
    if lower.contains("glm") {
        return Some("glm".into());
    }
    if lower.contains("minimax") {
        return Some("minimax".into());
    }
    if lower.contains("gemma") {
        return Some("gemma".into());
    }
    if lower.contains("nemotron") {
        return Some("nemotron".into());
    }
    if lower.contains("qwen") {
        return Some("qwen".into());
    }
    None
}
