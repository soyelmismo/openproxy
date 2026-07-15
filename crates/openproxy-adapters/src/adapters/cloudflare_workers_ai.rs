use super::*;

// =====================================================================
// Cloudflare Workers AI
// =====================================================================

/// Adapter for <https://developers.cloudflare.com/workers-ai/>.
///
/// Workers AI is OpenAI-compatible on the wire but requires the
/// CloudFlare account ID in the URL path. The account ID is stored
/// in the account's `label` field and passed through
/// `build_chat_url_for_account`.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CloudflareWorkersAIAdapter {
    config: ProviderAdapterConfig,
}

impl CloudflareWorkersAIAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("cloudflare-workers-ai"),
                base_url: "https://api.cloudflare.com/client/v4/accounts".into(),
                auth_type: AdapterAuthType::Bearer,
                format: AdapterFormat::Openai,
                extra_headers: vec![],
            },
        }
    }
}

crate::adapters::derive_default_from_new!(CloudflareWorkersAIAdapter);

impl ProviderAdapter for CloudflareWorkersAIAdapter {
    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn build_chat_url(&self, _target_format: TargetFormat, _model: &ModelId) -> String {
        // build_chat_url is the label-less path. Cloudflare's URL
        // template requires the account label, so without it the URL
        // is invalid. Previously this returned a URL with the literal
        // `__missing_account_label__` placeholder, which produced a
        // confusing 404 from upstream. Now we return a clearly-bogus
        // URL with a descriptive sentinel so the failure mode is
        // obvious in logs and error messages.
        //
        // The real chat path goes through `build_chat_url_for_account`
        // (see `Pipeline::execute_single`). This method is only
        // reached by tests or by code paths that didn't resolve the
        // account — both should be fixed to use the for_account
        // variant.
        format!(
            "{}/MISSING_ACCOUNT_LABEL_USE_build_chat_url_for_account/ai/v1/chat/completions",
            self.config.base_url
        )
    }

    fn build_chat_url_for_account(
        &self,
        _target_format: TargetFormat,
        _model: &ModelId,
        account_label: &str,
    ) -> String {
        // Sanitize the account label to prevent path traversal — strip
        // "/" and "." characters that could alter the URL structure.
        let safe_label = account_label.replace(['/', '.'], "");
        format!(
            "{}/{}/ai/v1/chat/completions",
            self.config.base_url, safe_label
        )
    }

    fn models_url(&self) -> Option<String> {
        // Label-less path returns None (no models URL without account_id).
        None
    }

    fn models_url_for_account(&self, account_label: &str) -> Option<String> {
        // B1 (Bug 2): mirror the `fetch_models_for_account` validation.
        // An empty label would build a URL with a double slash
        // (`accounts//ai/models/search`) — return `None` so callers
        // that probe the URL without fetching (e.g. debug diagnostics)
        // also see the missing-label condition. The actual fetch path
        // returns a `Validation` error for the same case.
        if account_label.trim().is_empty() {
            return None;
        }
        // Sanitize the account label to prevent path traversal.
        let safe_label = account_label.replace(['/', '.'], "");
        Some(format!(
            "{}/{}/ai/models/search",
            self.config.base_url, safe_label
        ))
    }

    async fn fetch_models(
        &self,
        _upstream_client: &Arc<UpstreamClient>,
        _api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        Err(CoreError::Internal(
            "cloudflare-workers-ai: use fetch_models_for_account".into(),
        ))
    }

    async fn fetch_models_for_account(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
        account_label: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        // B1 (Bug 2): validate the label is non-empty BEFORE building
        // the URL. An empty `account_label` would produce a URL like
        // `https://api.cloudflare.com/client/v4/accounts//ai/models/search`
        // (note the double slash) which Cloudflare answers with a
        // confusing 404 — the operator sees "upstream connection error:
        // status 404" with no hint that the account label is missing.
        // Returning a `Validation` error here surfaces the actual root
        // cause in the WARN log + the dashboard's Debug Logs view.
        if account_label.trim().is_empty() {
            return Err(CoreError::Validation(
                "cloudflare-workers-ai: account label is empty — \
                 set the account's `label` field to the Cloudflare account ID"
                    .into(),
            ));
        }
        let url = format!(
            "{}/{}/ai/models/search",
            self.config.base_url, account_label
        );
        fetch_models_with_auth(
            &url,
            upstream_client,
            &[("Authorization", format!("Bearer {}", api_key))],
            "result",
            "cloudflare",
            |raw| {
                let name = raw.get("name")?.as_str()?;
                Some(DiscoveredModel {
                    model_id: ModelId::new(name),
                    display_name: Some(name.to_string()),
                    target_format: TargetFormat::Openai,
                    context_length: raw.get("max_total_tokens").and_then(|v| v.as_i64()),
                    max_output_tokens: raw.get("max_total_tokens").and_then(|v| v.as_i64()),
                    input_modalities: None,
                    output_modalities: None,
                    model_type: None,
                    family: None,
                    capabilities: None,
                })
            },
        )
        .await
    }

    fn normalize_openai_request(&self, view: &mut openproxy_types::OpenAIRequestView) {
        // CloudFlare Workers AI OpenAI-compatible endpoint is stricter
        // than OpenAI: it rejects null optional fields, rejects
        // unsupported fields like `temperature` (even as a number),
        // and requires `content` to be a plain string, not a
        // multipart array.

        view.temperature = None;

        // Remove null fields from extra
        let has_nulls = view.extra.values().any(|v| v.is_null());
        if has_nulls {
            let extra_mut = view.extra.to_mut();
            extra_mut.retain(|_, v| !v.is_null());
        }

        // Flatten multipart content arrays to plain strings
        let needs_flattening = view
            .messages
            .iter()
            .any(|msg| matches!(msg.content, Some(serde_json::Value::Array(_))));
        if needs_flattening {
            let messages_mut = view.messages.to_mut();
            for msg in messages_mut.iter_mut() {
                if let Some(serde_json::Value::Array(parts)) = &msg.content {
                    let text = parts
                        .iter()
                        .find_map(|part| {
                            part.get("text")
                                .and_then(|t| t.as_str())
                                .or_else(|| part.get("content").and_then(|c| c.as_str()))
                        })
                        .unwrap_or("")
                        .to_string();
                    msg.content = Some(serde_json::Value::String(text));
                }
            }
        }
    }
}
