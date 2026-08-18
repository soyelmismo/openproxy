use super::{
    AdapterAuthType, AdapterFormat, Arc, CoreError, DiscoveredModel, ModelId, ProviderAdapter,
    ProviderAdapterConfig, ProviderId, Result, TargetFormat, UpstreamClient,
    build_discovered_model_full, fetch_models_with_auth,
};

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
                name: "Cloudflare Workers AI".into(),
                anonymous_fallback: false,
                rate_limit_scope: "account".into(),
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
        let auth = format!("Bearer {api_key}");
        fetch_models_with_auth(
            &url,
            upstream_client,
            &[("Authorization", &auth)],
            "result",
            "cloudflare",
            |raw| {
                let name = raw.get("name")?.as_str()?;
                let max_tokens = raw
                    .get("max_total_tokens")
                    .and_then(serde_json::Value::as_i64);
                Some(build_discovered_model_full(
                    name.to_string(),
                    Some(name.to_string()),
                    TargetFormat::Openai,
                    max_tokens,
                    max_tokens,
                ))
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
        let has_nulls = view.extra.values().any(serde_json::Value::is_null);
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

#[cfg(test)]
mod tests {
    use super::*;
    use openproxy_types::{OpenAIMessage, OpenAIRequestView};
    use serde_json::json;

    #[test]
    fn test_normalize_openai_request() {
        let adapter = CloudflareWorkersAIAdapter::new();
        let mut extra_map = serde_json::Map::new();
        extra_map.insert("extra_null".to_string(), serde_json::Value::Null);
        extra_map.insert("extra_valid".to_string(), json!(1));

        let stop = None;
        let tools = None;
        let tool_choice = None;
        let user = None;

        let mut view = OpenAIRequestView {
            model: "test-model",
            messages: std::borrow::Cow::Owned(vec![OpenAIMessage {
                role: "user".into(),
                content: Some(json!([{"type": "text", "text": "hello"}])),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: serde_json::Map::default(),
            }]),
            temperature: Some(0.7),
            max_tokens: None,
            top_p: None,
            stop: &stop,
            tools: &tools,
            tool_choice: &tool_choice,
            top_k: None,
            user: &user,
            extra: std::borrow::Cow::Owned(extra_map),
            stream: false,
        };

        adapter.normalize_openai_request(&mut view);

        assert_eq!(view.temperature, None);
        assert!(!view.extra.contains_key("extra_null"));
        assert!(view.extra.contains_key("extra_valid"));
        assert_eq!(view.messages[0].content, Some(json!("hello")));
    }

    #[test]
    fn test_cloudflare_urls_for_account() {
        let adapter = CloudflareWorkersAIAdapter::new();
        let model = ModelId::new("test-model");

        // Normal label
        let chat_url =
            adapter.build_chat_url_for_account(TargetFormat::Openai, &model, "my-account");
        assert_eq!(
            chat_url,
            "https://api.cloudflare.com/client/v4/accounts/my-account/ai/v1/chat/completions"
        );

        let models_url = adapter.models_url_for_account("my-account").unwrap();
        assert_eq!(
            models_url,
            "https://api.cloudflare.com/client/v4/accounts/my-account/ai/models/search"
        );

        // Label with slashes
        let chat_url_slashes =
            adapter.build_chat_url_for_account(TargetFormat::Openai, &model, "my/account");
        assert_eq!(
            chat_url_slashes,
            "https://api.cloudflare.com/client/v4/accounts/myaccount/ai/v1/chat/completions"
        );

        let models_url_slashes = adapter.models_url_for_account("my/account").unwrap();
        assert_eq!(
            models_url_slashes,
            "https://api.cloudflare.com/client/v4/accounts/myaccount/ai/models/search"
        );

        // Label with dots
        let chat_url_dots =
            adapter.build_chat_url_for_account(TargetFormat::Openai, &model, "../account");
        assert_eq!(
            chat_url_dots,
            "https://api.cloudflare.com/client/v4/accounts/account/ai/v1/chat/completions"
        );

        let models_url_dots = adapter.models_url_for_account("../account").unwrap();
        assert_eq!(
            models_url_dots,
            "https://api.cloudflare.com/client/v4/accounts/account/ai/models/search"
        );

        // Empty label
        assert_eq!(adapter.models_url_for_account(""), None);
        assert_eq!(adapter.models_url_for_account("   "), None);
    }
}
