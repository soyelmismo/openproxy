use super::{
    AdapterAuthType, AdapterFormat, Arc, DiscoveredModel, ModelId, ProviderAdapter,
    ProviderAdapterConfig, ProviderId, Result, TargetFormat, UpstreamClient, UpstreamRequest,
    upstream_get_json,
};
pub use crate::spoofer::CLINE_SPOOFING_HEADERS;
use crate::spoofer::{ClientSpoofer, ClineSpoofer};

pub fn apply_cline_spoofing_headers(req: &mut UpstreamRequest) {
    ClineSpoofer.apply_to_request(req);
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ClineAdapter {
    config: ProviderAdapterConfig,
}

#[derive(serde::Deserialize, Debug)]
struct ClineRecommendedModels {
    #[serde(default)]
    recommended: Vec<ClineModelEntry>,
    #[serde(default)]
    free: Vec<ClineModelEntry>,
    #[serde(default)]
    #[serde(rename = "clinePass")]
    cline_pass: Vec<ClineModelEntry>,
}

#[derive(serde::Deserialize, Debug)]
struct ClineModelEntry {
    id: String,
    #[serde(default)]
    name: String,
}

impl ClineAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("cline"),
                name: "Cline".into(),
                anonymous_fallback: false,
                rate_limit_scope: "account".into(),
                base_url: "https://api.cline.bot/api/v1".into(),
                auth_type: AdapterAuthType::OAuth,
                format: AdapterFormat::Openai,
                extra_headers: ClineSpoofer.headers(),
            },
        }
    }
}

crate::adapters::derive_default_from_new!(ClineAdapter);

fn make_cline_model_id(mut id: String, is_free: bool) -> String {
    if is_free && !id.ends_with(":free") && !id.contains("-free") {
        id.push_str(":free");
    }
    id
}

fn map_cline_entry(entry: ClineModelEntry, is_free: bool) -> DiscoveredModel {
    let id = make_cline_model_id(entry.id, is_free);
    let caps = openproxy_types::ModelCapabilities {
        vision: Some(true),
        tool_calling: Some(true),
        reasoning: Some(true),
        thinking: Some(true),
        attachment: None,
        structured_output: None,
        temperature: None,
    };
    DiscoveredModel {
        model_id: ModelId::new(id),
        display_name: Some(entry.name),
        target_format: TargetFormat::Openai,
        context_length: Some(128_000),
        max_output_tokens: Some(8_192),
        input_modalities: None,
        output_modalities: None,
        model_type: Some("chat".to_string()),
        family: None,
        capabilities: Some(caps),
    }
}

impl ProviderAdapter for ClineAdapter {
    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn metadata(&self) -> openproxy_types::ProviderMetadata {
        let mut meta = openproxy_types::ProviderMetadata::custom_default();
        meta.built_in = true;
        meta.deletable = false;
        meta.supports_quota = false;
        meta.quota_refresh_supported = false;
        meta.requires_oauth = true;
        meta.oauth_refresh_lead_seconds = Some(300);
        meta
    }

    fn build_auth_header(&self, access_token: &str) -> Option<(String, String)> {
        let token = if access_token.starts_with("workos:") {
            access_token.to_string()
        } else {
            format!("workos:{access_token}")
        };
        Some(("Authorization".into(), format!("Bearer {token}")))
    }

    fn models_url(&self) -> Option<String> {
        Some("https://api.cline.bot/api/v1/ai/cline/recommended-models".into())
    }

    async fn fetch_models(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        _api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        let url = self.models_url().ok_or_else(|| {
            openproxy_types::error::CoreError::Internal("missing models_url".into())
        })?;

        let body = upstream_get_json(upstream_client, &url, &[])
            .await
            .map_err(openproxy_types::error::CoreError::UpstreamConnection)?;

        let payload: ClineRecommendedModels =
            serde_json::from_value(body).map_err(|e| {
                openproxy_types::error::CoreError::Parse(format!("cline parse error: {e}"))
            })?;

        let discovered = payload
            .recommended
            .into_iter()
            .map(|e| map_cline_entry(e, false))
            .chain(payload.free.into_iter().map(|e| map_cline_entry(e, true)))
            .chain(
                payload
                    .cline_pass
                    .into_iter()
                    .map(|e| map_cline_entry(e, false)),
            )
            .collect();

        Ok(discovered)
    }

    fn wrap_request_body(
        &self,
        body: bytes::Bytes,
        _target_format: TargetFormat,
        _model: &ModelId,
        _resolved_target: &openproxy_types::context::ResolvedTarget,
    ) -> std::result::Result<bytes::Bytes, openproxy_types::error::CoreError> {
        let mut val: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|e| openproxy_types::error::CoreError::Parse(e.to_string()))?;

        if let Some(obj) = val.as_object_mut() {
            patch_cline_request_object(obj);
        }

        let new_body = serde_json::to_vec(&val)
            .map_err(|e| openproxy_types::error::CoreError::Parse(e.to_string()))?;
        Ok(bytes::Bytes::from(new_body))
    }
}

fn normalize_cline_model_name(model_str: &mut String) {
    if let Some(stripped) = model_str
        .strip_suffix(":free")
        .or_else(|| model_str.strip_suffix("-free"))
    {
        *model_str = stripped.to_string();
    }
}

fn patch_cline_request_object(obj: &mut serde_json::Map<String, serde_json::Value>) {
    if let Some(serde_json::Value::String(model_str)) = obj.get_mut("model") {
        normalize_cline_model_name(model_str);
    }
    // Cline backend ALWAYS requires stream: true, else it returns HTTP 500 "empty response content"
    if obj.get("stream").is_none_or(serde_json::Value::is_boolean) {
        obj.insert("stream".to_string(), serde_json::Value::Bool(true));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    #[test]
    fn test_apply_cline_spoofing_headers() {
        let mut req = UpstreamRequest::post_json("http://dummy.com", bytes::Bytes::new());
        apply_cline_spoofing_headers(&mut req);

        for &(k, v) in CLINE_SPOOFING_HEADERS {
            let header_value = req.headers.get(k).expect("header missing");
            assert_eq!(header_value, HeaderValue::from_str(v).unwrap());
        }
    }

    #[test]
    fn test_wrap_request_body_modifies_model_and_stream() {
        let adapter = ClineAdapter::new();
        let json_body = serde_json::json!({
            "model": "somemodel:free",
            "messages": [],
            "stream": false
        });
        let body_bytes = bytes::Bytes::from(serde_json::to_vec(&json_body).unwrap());

        let resolved_target = openproxy_types::context::ResolvedTarget {
            target: openproxy_types::combos::ComboTarget {
                id: openproxy_types::ComboTargetId(1),
                combo_id: openproxy_types::ComboId(1),
                provider_id: openproxy_types::ProviderId::new("cline"),
                account_id: None,
                model_row_id: Some(openproxy_types::ModelRowId(1)),
                sub_combo_id: None,
                priority_order: 0,
                weight: 100,
                active: true,
                rate_limit_scope: openproxy_types::providers::RateLimitScope::Account,
                cooldown_mode: None,
                cooldown_base_secs: None,
                cooldown_max_secs: None,
                cooldown_factor: None,
            },
            model: openproxy_types::Model {
                row_id: openproxy_types::ModelRowId(1),
                provider_id: openproxy_types::ProviderId::new("cline"),
                target_format: openproxy_types::TargetFormat::Openai,
                discovered_at: openproxy_types::now_unix_secs_str(),
                expires_at: None,
                model_id: openproxy_types::ModelId::new("somemodel"),
                display_name: None,
                context_length: None,
                max_output_tokens: None,
                model_type: "chat".to_string(),
                family: None,
                input_modalities_json: None,
                output_modalities_json: None,
                capabilities_json: None,
                timeout_overrides_json: None,
                active: true,
                last_test_status: None,
                last_test_at: None,
                custom: false,
            },
            api_key: "dummy".to_string(),
            api_key_label: None,
            custom_meta: None,
        };

        let result = adapter
            .wrap_request_body(
                body_bytes,
                openproxy_types::TargetFormat::Openai,
                &openproxy_types::ModelId::new("somemodel:free"),
                &resolved_target,
            )
            .expect("should wrap body successfully");

        let wrapped_json: serde_json::Value = serde_json::from_slice(&result).unwrap();
        assert_eq!(
            wrapped_json.get("model").unwrap().as_str().unwrap(),
            "somemodel"
        );
        assert!(wrapped_json.get("stream").unwrap().as_bool().unwrap());
    }
}
