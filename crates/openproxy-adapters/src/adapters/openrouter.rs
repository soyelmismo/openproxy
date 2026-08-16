use super::{Deserialize, ProviderAdapterConfig, ProviderAdapter, ProviderId, AdapterAuthType, AdapterFormat, Arc, UpstreamClient, Result, DiscoveredModel, upstream_get_json, ModelId, TargetFormat, UpstreamRequest, CancellationToken, TimeoutProfile};

// =====================================================================
// OpenRouter
// =====================================================================

/// Adapter for <https://openrouter.ai>.
///
/// OpenRouter is OpenAI-only on the wire: every model is served through
/// `POST /chat/completions` regardless of which upstream actually answers
/// behind the scenes.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct OpenRouterAdapter {
    config: ProviderAdapterConfig,
}

impl OpenRouterAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("openrouter"),
                name: "OpenRouter".into(),
                anonymous_fallback: false,
                rate_limit_scope: "account".into(),
                base_url: "https://openrouter.ai/api/v1".into(),
                auth_type: AdapterAuthType::Bearer,
                format: AdapterFormat::Openai,
                extra_headers: vec![
                    ("HTTP-Referer".into(), "https://openproxy.local".into()),
                    ("X-Title".into(), "openproxy".into()),
                ],
            },
        }
    }
}

crate::adapters::derive_default_from_new!(OpenRouterAdapter);

impl ProviderAdapter for OpenRouterAdapter {
    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn metadata(&self) -> openproxy_types::ProviderMetadata {
        let mut meta = openproxy_types::ProviderMetadata::custom_default();
        meta.built_in = true;
        meta.deletable = false;
        meta.supports_quota = true;
        meta.quota_refresh_supported = true;
        meta
    }

    async fn fetch_models(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        let url = self.models_url().ok_or_else(|| {
            openproxy_types::error::CoreError::Internal("openrouter has no models_url".into())
        })?;

        let auth = format!("Bearer {api_key}");
        let body = upstream_get_json(upstream_client, &url, &[("Authorization", &auth)])
            .await
            .map_err(openproxy_types::error::CoreError::UpstreamConnection)?;

        let arr = body.get("data").and_then(|v| v.as_array()).ok_or_else(|| {
            openproxy_types::error::CoreError::Parse(
                "openrouter response missing 'data' array".into(),
            )
        })?;

        let models: Vec<DiscoveredModel> = arr
            .iter()
            .filter_map(|raw| {
                let mut entry: OpenRouterModelEntry = serde::Deserialize::deserialize(raw).ok()?;
                let id_string = entry.id.take()?;

                // Derive capabilities from supported_parameters.
                let caps = derive_capabilities(&entry);

                // Derive model_type from id and modalities.
                let model_type =
                    infer_model_type_openrouter(&id_string, entry.architecture.as_ref());

                // Extract modalities (skip empty arrays so they serialize
                // as NULL rather than `[]`).
                let input_modalities = entry.architecture.as_mut().and_then(|a| {
                    if a.input_modalities.is_empty() {
                        None
                    } else {
                        Some(std::mem::take(&mut a.input_modalities))
                    }
                });
                let output_modalities = entry.architecture.as_mut().and_then(|a| {
                    if a.output_modalities.is_empty() {
                        None
                    } else {
                        Some(std::mem::take(&mut a.output_modalities))
                    }
                });

                // Context: prefer top-level, fallback to top_provider.
                let context_length = entry
                    .context_length
                    .or_else(|| entry.top_provider.as_ref().and_then(|t| t.context_length));

                // Max output: from top_provider.
                let max_output_tokens = entry
                    .top_provider
                    .as_ref()
                    .and_then(|t| t.max_completion_tokens);

                // Family: derive from canonical_slug or hugging_face_id or id.
                let family = entry
                    .canonical_slug
                    .or(entry.hugging_face_id)
                    .or_else(|| derive_family_from_id(&id_string));

                let display_name = entry.name.or_else(|| Some(id_string.clone()));
                Some(DiscoveredModel {
                    model_id: ModelId::new(id_string),
                    display_name,
                    // OpenRouter is OpenAI-only on the wire for chat completions.
                    target_format: TargetFormat::Openai,
                    context_length,
                    max_output_tokens,
                    input_modalities,
                    output_modalities,
                    model_type: Some(model_type),
                    family,
                    capabilities: Some(caps),
                })
            })
            .collect();

        Ok(models)
    }

    async fn fetch_quota(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Option<Result<openproxy_types::AccountQuota>> {
        // OpenRouter's fetcher catches its own errors and maps them to AccountQuota fields.
        // It never actually returns an `Err(CoreError)`.
        Some(
            self.fetch_openrouter_quota_local(upstream_client, api_key)
                .await,
        )
    }
}

impl OpenRouterAdapter {
    async fn fetch_openrouter_quota_local(
        &self,
        upstream: &Arc<UpstreamClient>,
        api_key: &str,
    ) -> Result<openproxy_types::AccountQuota> {
        let url = "https://openrouter.ai/api/v1/key";

        let mut req = UpstreamRequest::get(url);
        if let Ok(v) = http::HeaderValue::from_str(&format!("Bearer {api_key}")) {
            req.headers.insert(http::header::AUTHORIZATION, v);
        }

        let cancel = CancellationToken::new();
        let response = match upstream.call(req, TimeoutProfile::Quota, cancel).await {
            Ok(r) => r,
            Err(e) => {
                return Ok(openproxy_types::AccountQuota {
                    session_used: None,
                    session_limit: None,
                    session_reset_at: None,
                    weekly_used: None,
                    weekly_limit: None,
                    weekly_reset_at: None,
                    plan_name: None,
                    last_fetched_at: openproxy_types::now_unix_secs_str(),
                    fetch_error: Some(format!("network: {e}")),
                    model_details: None,
                });
            }
        };

        if !response.status.is_success() {
            let status = response.status.as_u16();
            let body = response.collect().await.unwrap_or_default();
            let snippet = String::from_utf8_lossy(&body)
                .chars()
                .take(200)
                .collect::<String>();
            return Ok(openproxy_types::AccountQuota {
                session_used: None,
                session_limit: None,
                session_reset_at: None,
                weekly_used: None,
                weekly_limit: None,
                weekly_reset_at: None,
                plan_name: None,
                last_fetched_at: openproxy_types::now_unix_secs_str(),
                fetch_error: Some(format!("HTTP {status}: {snippet}")),
                model_details: None,
            });
        }

        let body = match response.collect().await {
            Ok(b) => b,
            Err(e) => {
                return Ok(openproxy_types::AccountQuota {
                    session_used: None,
                    session_limit: None,
                    session_reset_at: None,
                    weekly_used: None,
                    weekly_limit: None,
                    weekly_reset_at: None,
                    plan_name: None,
                    last_fetched_at: openproxy_types::now_unix_secs_str(),
                    fetch_error: Some(format!("collect: {e}")),
                    model_details: None,
                });
            }
        };

        let json: serde_json::Value = match serde_json::from_slice(&body) {
            Ok(b) => b,
            Err(e) => {
                return Ok(openproxy_types::AccountQuota {
                    session_used: None,
                    session_limit: None,
                    session_reset_at: None,
                    weekly_used: None,
                    weekly_limit: None,
                    weekly_reset_at: None,
                    plan_name: None,
                    last_fetched_at: openproxy_types::now_unix_secs_str(),
                    fetch_error: Some(format!("parse: {e}")),
                    model_details: None,
                });
            }
        };

        Ok(parse_openrouter_quota(
            &json,
            &openproxy_types::now_unix_secs_str(),
        ))
    }
}

fn parse_openrouter_quota(
    body: &serde_json::Value,
    last_fetched_at: &str,
) -> openproxy_types::AccountQuota {
    let data = body.get("data");

    let raw_usage = data.and_then(|d| d.get("usage")).and_then(serde_json::Value::as_f64);
    let raw_limit = data.and_then(|d| d.get("limit")).and_then(serde_json::Value::as_f64);
    let is_free = data
        .and_then(|d| d.get("is_free_tier"))
        .and_then(serde_json::Value::as_bool)
        .is_some_and(|b| b);
    let rate_limit = data.and_then(|d| d.get("rate_limit"));

    let session_used = raw_usage.filter(|u| *u >= 0.0).map(|u| (u * 100.0) as i64);
    let session_limit = raw_limit.filter(|l| *l > 0.0).map(|l| (l * 100.0) as i64);

    let plan_name = if is_free {
        "OpenRouter (free tier)".to_string()
    } else {
        "OpenRouter".to_string()
    };

    let rate_limit_text = rate_limit.and_then(format_rate_limit_suffix);

    let plan_name = match rate_limit_text {
        Some(rl) => format!("{plan_name} · {rl}"),
        None => plan_name,
    };

    let no_numeric_data = session_used.is_none() && session_limit.is_none();
    let fetch_error = if data.is_none() {
        Some("missing 'data' in response".to_string())
    } else if no_numeric_data {
        Some("usage not configured".to_string())
    } else {
        None
    };

    openproxy_types::AccountQuota {
        session_used,
        session_limit,
        session_reset_at: None,
        weekly_used: None,
        weekly_limit: None,
        weekly_reset_at: None,
        plan_name: Some(plan_name),
        last_fetched_at: last_fetched_at.to_string(),
        fetch_error,
        model_details: None,
    }
}

fn format_rate_limit_suffix(rl: &serde_json::Value) -> Option<String> {
    let reqs = rl.get("requests").and_then(serde_json::Value::as_i64)?;
    let interval = rl.get("interval").and_then(|v| v.as_str())?;

    if reqs < 0 {
        return None;
    }

    let unit = match interval.chars().last() {
        Some('s') => "sec",
        Some('m') => "min",
        Some('h') => "hr",
        Some('d') => "day",
        _ => return None,
    };

    Some(format!("{reqs} req/{unit}"))
}

#[derive(Debug, Deserialize)]
struct OpenRouterArchitecture {
    #[serde(default)]
    input_modalities: Vec<String>,
    #[serde(default)]
    output_modalities: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterTopProvider {
    #[serde(default)]
    context_length: Option<i64>,
    #[serde(default)]
    max_completion_tokens: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterModelEntry {
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    canonical_slug: Option<String>,
    #[serde(default)]
    hugging_face_id: Option<String>,
    #[serde(default)]
    context_length: Option<i64>,
    #[serde(default)]
    architecture: Option<OpenRouterArchitecture>,
    #[serde(default)]
    top_provider: Option<OpenRouterTopProvider>,
    #[serde(default)]
    supported_parameters: Option<Vec<String>>,
}

/// Build a [`crate::capabilities::ModelCapabilities`] from an OpenRouter
/// model entry's `supported_parameters` and `architecture`. Each field
/// is set only when there's positive evidence; everything else stays
/// `None` so the public `GET /v1/models` projection can distinguish
/// "unknown" from "explicitly false".
fn derive_capabilities(entry: &OpenRouterModelEntry) -> openproxy_types::ModelCapabilities {
    use openproxy_types::ModelCapabilities;
    let mut caps = ModelCapabilities::empty();

    // vision: from architecture.input_modalities.
    let has_image_input = entry.architecture.as_ref().is_some_and(|a| {
        a.input_modalities
            .iter()
            .any(|m| m == "image" || m == "video")
    });
    if has_image_input {
        caps.vision = Some(true);
        caps.attachment = Some(true);
    }

    // tool_calling / reasoning / structured_output / temperature come
    // straight from the supported_parameters list OpenRouter publishes.
    let params = entry.supported_parameters.as_deref().unwrap_or(&[]);
    if params.iter().any(|p| p == "tools") {
        caps.tool_calling = Some(true);
    }
    if params
        .iter()
        .any(|p| p == "reasoning" || p == "include_reasoning")
    {
        caps.reasoning = Some(true);
        caps.thinking = Some(true);
    }
    if params.iter().any(|p| p == "structured_outputs") {
        caps.structured_output = Some(true);
    }
    if params.iter().any(|p| p == "temperature") {
        caps.temperature = Some(true);
    }

    // If supported_parameters is missing entirely, fall back to the
    // chat-model defaults so the model is still advertised as usable
    // for tool_calling/structured_output/temperature. This matches
    // the heuristic in `capabilities::infer_capabilities` for the
    // no-evidence case.
    if params.is_empty() {
        if caps.tool_calling.is_none() {
            caps.tool_calling = Some(true);
        }
        if caps.structured_output.is_none() {
            caps.structured_output = Some(true);
        }
        if caps.temperature.is_none() {
            caps.temperature = Some(true);
        }
    }

    caps
}

/// Classify a model id into a coarse `model_type` string
/// (`"chat" | "embedding" | "image" | "audio" | "rerank"`) using both
/// the id's name and the `architecture.output_modalities` field.
fn infer_model_type_openrouter(id: &str, architecture: Option<&OpenRouterArchitecture>) -> String {
    let lower = id.to_lowercase();

    if lower.contains("embed") {
        return "embedding".to_string();
    }
    if lower.contains("dall-e")
        || lower.contains("flux")
        || lower.contains("imagen")
        || lower.contains("sdxl")
        || lower.contains("ideogram")
    {
        return "image".to_string();
    }
    if lower.contains("whisper") || lower.contains("tts") || lower.contains("eleven") {
        return "audio".to_string();
    }
    if lower.contains("rerank") {
        return "rerank".to_string();
    }

    // Output modalities: if a model emits image/audio, classify by that
    // even if the name doesn't carry a giveaway keyword.
    if let Some(arch) = architecture {
        if arch.output_modalities.iter().any(|m| m == "image") {
            return "image".to_string();
        }
        if arch.output_modalities.iter().any(|m| m == "audio") {
            return "audio".to_string();
        }
    }

    "chat".to_string()
}

/// Best-effort extraction of a model "family" from a model id. The
/// `canonical_slug` and `hugging_face_id` paths in the adapter's main
/// loop are preferred when present; this is the final fallback for
/// upstreams that only supply the raw id.
fn derive_family_from_id(id: &str) -> Option<String> {
    let name = id.rsplit_once('/').map_or(id, |(_, tail)| tail);
    openproxy_types::capabilities::infer_family(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_format_rate_limit_suffix() {
        assert_eq!(
            format_rate_limit_suffix(&json!({"requests": 10, "interval": "1s"})),
            Some("10 req/sec".to_string())
        );
        assert_eq!(
            format_rate_limit_suffix(&json!({"requests": 100, "interval": "1m"})),
            Some("100 req/min".to_string())
        );
        assert_eq!(
            format_rate_limit_suffix(&json!({"requests": 1000, "interval": "1h"})),
            Some("1000 req/hr".to_string())
        );
        assert_eq!(
            format_rate_limit_suffix(&json!({"requests": 10000, "interval": "1d"})),
            Some("10000 req/day".to_string())
        );
        assert_eq!(
            format_rate_limit_suffix(&json!({"requests": -1, "interval": "1d"})),
            None
        );
        assert_eq!(
            format_rate_limit_suffix(&json!({"requests": 10, "interval": "1w"})),
            None
        );
        assert_eq!(format_rate_limit_suffix(&json!({"interval": "1s"})), None);
        assert_eq!(format_rate_limit_suffix(&json!({"requests": 10})), None);
    }
}
