use crate::upstream::{CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest};
use http::HeaderValue;
use openproxy_types::{CoreError, DiscoveredModel, ModelId, Result, ResultExt, TargetFormat};
use serde::Deserialize;
use std::sync::Arc;

pub(crate) fn insert_upstream_headers(
    req: &mut UpstreamRequest,
    headers: &[(&str, &str)],
) -> std::result::Result<(), String> {
    req.headers.reserve(headers.len());
    for &(k, v) in headers {
        let Ok(hv) = HeaderValue::from_str(v) else {
            continue;
        };
        let name = match header_name(k) {
            Some(n) => n,
            None => http::header::HeaderName::from_bytes(k.as_bytes())
                .map_err(|e| format!("invalid header name '{k}': {e}"))?,
        };
        req.headers.insert(name, hv);
    }
    Ok(())
}

async fn handle_upstream_error_response(
    url: &str,
    response: crate::upstream::UpstreamResponse,
) -> String {
    let status = response.status.as_u16();
    let body = response
        .collect()
        .await
        .map_err(|e| format!("{url}: failed to read error body: {e}"));
    match body {
        Ok(b) => format!("{url}: status {status}: {}", String::from_utf8_lossy(&b)),
        Err(e) => e,
    }
}

/// Fetch raw bytes from an upstream URL via the shared `UpstreamClient`.
pub async fn upstream_get_bytes(
    upstream_client: &Arc<UpstreamClient>,
    url: &str,
    headers: &[(&str, &str)],
) -> std::result::Result<bytes::Bytes, String> {
    let mut req = UpstreamRequest::get(url);
    req.headers.insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("application/json, text/plain, */*"),
    );
    insert_upstream_headers(&mut req, headers)?;

    let cancel = CancellationToken::new();
    let response = upstream_client
        .call(req, TimeoutProfile::ModelDiscovery, cancel)
        .await
        .map_err(|e| format!("{url}: {e}"))?;

    if !response.status.is_success() {
        return Err(handle_upstream_error_response(url, response).await);
    }

    response.collect().await.map_err(|e| format!("{url}: {e}"))
}

pub(crate) async fn upstream_get_json(
    upstream_client: &Arc<UpstreamClient>,
    url: &str,
    headers: &[(&str, &str)],
) -> std::result::Result<serde_json::Value, String> {
    let bytes = upstream_get_bytes(upstream_client, url, headers).await?;
    serde_json::from_slice(&bytes).map_err(|e| format!("{url}: parse: {e}"))
}

/// Map a header name to its typed `http::header::HeaderName` constant when one exists.
pub(crate) fn header_name(name: &str) -> Option<http::header::HeaderName> {
    use http::header;
    match name.len() {
        9 if name.eq_ignore_ascii_case("x-api-key") => {
            Some(http::HeaderName::from_static("x-api-key"))
        }
        10 if name.eq_ignore_ascii_case("user-agent") => Some(header::USER_AGENT),
        12 if name.eq_ignore_ascii_case("content-type") => Some(header::CONTENT_TYPE),
        13 if name.eq_ignore_ascii_case("authorization") => Some(header::AUTHORIZATION),
        14 if name.eq_ignore_ascii_case("x-goog-api-key") => {
            Some(http::HeaderName::from_static("x-goog-api-key"))
        }
        _ => None,
    }
}

#[derive(Deserialize)]
pub(crate) struct OpenAIModelsResponse {
    #[serde(default)]
    pub(crate) data: Vec<OpenAIModelEntry>,
}

#[derive(Deserialize)]
pub(crate) struct OpenAIModelEntry {
    pub(crate) id: String,
}

/// Helper to construct a [`DiscoveredModel`] with full parameters and standard capability inferences.
pub fn build_discovered_model_full(
    id: String,
    display_name: Option<String>,
    target_format: TargetFormat,
    context_length: Option<i64>,
    max_output_tokens: Option<i64>,
) -> DiscoveredModel {
    let m_type = openproxy_types::capabilities::infer_model_type(&id);
    let caps = openproxy_types::capabilities::infer_capabilities(&id);
    let in_mods = openproxy_types::capabilities::infer_input_modalities_for_model(&id, &caps);
    let out_mods = openproxy_types::capabilities::infer_output_modalities(&id);
    let family = openproxy_types::capabilities::infer_family(&id);
    DiscoveredModel {
        display_name: display_name.or_else(|| Some(id.clone())),
        model_id: ModelId::new(id),
        target_format,
        context_length,
        max_output_tokens,
        input_modalities: Some(in_mods.into_iter().map(String::from).collect()),
        output_modalities: Some(out_mods.into_iter().map(String::from).collect()),
        model_type: Some(m_type.to_string()),
        family,
        capabilities: Some(caps),
    }
}

/// Helper to construct a [`DiscoveredModel`] with standard capability inferences.
pub fn build_discovered_model_with(id: String, target_format: TargetFormat) -> DiscoveredModel {
    build_discovered_model_full(id, None, target_format, None, None)
}

/// Fetch and parse an OpenAI-shaped `GET /models` response.
pub(crate) async fn fetch_openai_models(
    url: &str,
    upstream_client: &Arc<UpstreamClient>,
    api_key: &str,
    provider_name: &str,
    target_format: TargetFormat,
) -> Result<Vec<DiscoveredModel>> {
    let auth = format!("Bearer {api_key}");
    let body = upstream_get_json(upstream_client, url, &[("Authorization", &auth)])
        .await
        .ctx_upstream(format!("{provider_name} /models"))?;

    let payload: OpenAIModelsResponse =
        <OpenAIModelsResponse as serde::Deserialize>::deserialize(&body)
            .map_err(|e| CoreError::Parse(format!("{provider_name} /models parse: {e}")))?;

    let out = payload
        .data
        .into_iter()
        .map(|m| build_discovered_model_with(m.id, target_format))
        .collect();
    Ok(out)
}

pub(crate) async fn fetch_models_with_auth<F>(
    url: &str,
    upstream_client: &std::sync::Arc<UpstreamClient>,
    headers: &[(&str, &str)],
    array_key: &str,
    error_prefix: &str,
    mapper: F,
) -> Result<Vec<DiscoveredModel>>
where
    F: Fn(&serde_json::Value) -> Option<DiscoveredModel>,
{
    let body = upstream_get_json(upstream_client, url, headers)
        .await
        .ctx_upstream(format!("{error_prefix} /models"))?;

    let arr = body
        .get(array_key)
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            CoreError::Parse(format!(
                "{error_prefix} /models: missing '{array_key}' array"
            ))
        })?;

    Ok(arr.iter().filter_map(mapper).collect())
}
