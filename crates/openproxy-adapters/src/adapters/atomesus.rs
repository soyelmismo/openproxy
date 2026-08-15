use super::*;

// =====================================================================
// Atomesus Adapter
// =====================================================================

/// Adapter for <https://api.atomesus.com/api>.
///
/// Atomesus uses JWT Bearer auth, custom headers (Origin, Referer), and
/// an SSE-based streaming response format.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AtomesusAdapter {
    config: ProviderAdapterConfig,
}

impl AtomesusAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("atomesus"),
                name: "Atomesus".into(),
                anonymous_fallback: false,
                rate_limit_scope: "account".into(),
                base_url: "https://api.atomesus.com/api".into(),
                auth_type: AdapterAuthType::Bearer,
                format: AdapterFormat::Atomesus,
                extra_headers: vec![
                    ("Origin".into(), "https://www.atomesus.com".into()),
                    ("Referer".into(), "https://www.atomesus.com/".into()),
                    ("Accept".into(), "text/event-stream".into()),
                    (
                        "User-Agent".into(),
                        "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/150.0.0.0 Safari/537.36".into(),
                    ),
                ],
            },
        }
    }
}

crate::adapters::derive_default_from_new!(AtomesusAdapter);

impl ProviderAdapter for AtomesusAdapter {
    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn build_chat_url(&self, _target_format: TargetFormat, _model: &ModelId) -> String {
        format!("{}/chat/atomesus", self.config().base_url)
    }

    fn wrap_request_body(
        &self,
        body: bytes::Bytes,
        _target_format: TargetFormat,
        model: &ModelId,
        _resolved_target: &openproxy_types::context::ResolvedTarget,
    ) -> Result<bytes::Bytes> {
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&body) else {
            return Ok(body);
        };

        // If the body is already in atomesus format (has "message" string), pass it through
        if v.get("message").and_then(|m| m.as_str()).is_some() {
            return Ok(body);
        }

        let mut full_prompt = String::new();
        if let Some(messages) = v.get("messages").and_then(|m| m.as_array()) {
            for msg in messages {
                let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
                let content = if let Some(text) = msg.get("content").and_then(|c| c.as_str()) {
                    text.to_string()
                } else if let Some(arr) = msg.get("content").and_then(|c| c.as_array()) {
                    let mut text_parts = Vec::new();
                    for part in arr {
                        if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                            text_parts.push(t);
                        }
                    }
                    text_parts.join("\n")
                } else {
                    String::new()
                };

                if !content.is_empty() {
                    if !full_prompt.is_empty() {
                        full_prompt.push_str("\n\n");
                    }
                    if role == "system" {
                        full_prompt.push_str(&format!("[System Instructions]\n{}", content));
                    } else if role == "assistant" {
                        full_prompt.push_str(&format!("[Assistant]\n{}", content));
                    } else {
                        full_prompt.push_str(&content);
                    }
                }
            }
        } else if let Some(prompt) = v.get("prompt").and_then(|p| p.as_str()) {
            full_prompt = prompt.to_string();
        }

        let mode = if model.as_str().ends_with("-fast") {
            "fast"
        } else {
            "thinking"
        };

        // Map model_id -> upstream model id
        let upstream_model = model
            .as_str()
            .trim_end_matches("-fast")
            .trim_end_matches("-thinking");

        let atomesus_payload = serde_json::json!({
            "message": full_prompt,
            "stream": true,
            "mode": mode,
            "model": upstream_model,
            "isImageGen": false
        });

        serde_json::to_vec(&atomesus_payload)
            .map(bytes::Bytes::from)
            .map_err(|e| CoreError::Internal(format!("failed to serialize atomesus body: {}", e)))
    }

    async fn fetch_models(
        &self,
        _upstream_client: &Arc<UpstreamClient>,
        _api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        let base = |id: &str, name: &str, ctx: i64, out: i64| DiscoveredModel {
            model_id: ModelId::new(id),
            display_name: Some(name.into()),
            target_format: TargetFormat::Atomesus,
            context_length: Some(ctx),
            max_output_tokens: Some(out),
            input_modalities: Some(vec!["text".into()]),
            output_modalities: Some(vec!["text".into()]),
            model_type: Some("chat".into()),
            family: Some("atomesus".into()),
            capabilities: None,
        };

        Ok(vec![
            // Atomesus 1.5
            base("atomesus-1-5-fast", "Atomesus 1.5 Fast", 128_000, 8_192),
            base(
                "atomesus-1-5-thinking",
                "Atomesus 1.5 Thinking",
                128_000,
                16_384,
            ),
            // Atomesus 2
            base("atomesus-2-fast", "Atomesus 2 Fast", 128_000, 8_192),
            base(
                "atomesus-2-thinking",
                "Atomesus 2 Thinking",
                128_000,
                16_384,
            ),
            // Cipher (paid)
            base("cipher-fast", "Cipher Fast", 128_000, 8_192),
            base("cipher-thinking", "Cipher Thinking", 128_000, 16_384),
        ])
    }
}
