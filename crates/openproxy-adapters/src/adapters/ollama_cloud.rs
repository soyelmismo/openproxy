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

            let mut auth = String::with_capacity(7 + api_key.len());
            auth.push_str("Bearer ");
            auth.push_str(api_key);
            let body = upstream_get_json(
                upstream_client,
                &url,
                &[("Authorization", &auth)],
            )
            .await
            .map_err(|e| CoreError::UpstreamConnection(format!("ollama-cloud /api/tags: {e}")))?;

            let payload: OllamaTagsResponse =
                serde_json::from_value(body)
                    .map_err(|e| CoreError::Parse(format!("ollama-cloud /api/tags parse: {e}")))?;

            let out = payload
                .models
                .into_iter()
                .map(map_ollama_tag_entry)
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

fn map_ollama_tag_entry(m: OllamaTagEntry) -> DiscoveredModel {
    let id = m.name.unwrap_or_default();
    let family = openproxy_types::capabilities::infer_family(&id);
    let display_name = m.display_name.or_else(|| Some(id.clone()));
    let m_type = openproxy_types::capabilities::infer_model_type(&id);
    let caps = openproxy_types::capabilities::infer_capabilities(&id);
    let in_mods = openproxy_types::capabilities::infer_input_modalities_for_model(&id, &caps);
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_map_ollama_tag_entry() {
        let entry1 = OllamaTagEntry {
            name: Some("gemma4:31b".to_string()),
            display_name: None,
        };
        let m1 = map_ollama_tag_entry(entry1);
        assert_eq!(m1.model_id.as_str(), "gemma4:31b");
        assert_eq!(m1.display_name.as_deref(), Some("gemma4:31b"));

        let entry2 = OllamaTagEntry {
            name: Some("qwen3.5:397b".to_string()),
            display_name: Some("Qwen 3.5".to_string()),
        };
        let m2 = map_ollama_tag_entry(entry2);
        assert_eq!(m2.model_id.as_str(), "qwen3.5:397b");
        assert_eq!(m2.display_name.as_deref(), Some("Qwen 3.5"));

        let entry3 = OllamaTagEntry {
            name: None,
            display_name: None,
        };
        let m3 = map_ollama_tag_entry(entry3);
        assert_eq!(m3.model_id.as_str(), "");
        assert_eq!(m3.display_name.as_deref(), Some(""));
    }
}
