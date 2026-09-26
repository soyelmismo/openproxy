use crate::adapters::codex::apply_codex_spoofing_headers;
use crate::spoofer::{current_codex_ua, current_codex_version};
use crate::upstream::{CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest};
use openproxy_types::{CoreError, DiscoveredModel, ModelId, Result, TargetFormat};
use std::sync::Arc;

pub const CODEX_MODELS_URL: &str = "https://chatgpt.com/backend-api/codex/models";
pub const CODEX_UPSTREAM_MODELS_RAW_URL: &str =
    "https://raw.githubusercontent.com/openai/codex/main/codex-rs/models-manager/models.json";

/// Returns the official bundled model catalog for Codex (from codex-rs/models-manager/models.json).
pub fn codex_static_models() -> Vec<DiscoveredModel> {
    hardcoded_models()
}

pub fn build_hardcoded_codex_model(
    (id, name, ctx, _max_ctx, vision): (&str, &str, i64, i64, bool),
) -> DiscoveredModel {
    let input_modalities = if vision {
        Some(vec!["text".to_string(), "image".to_string()].into())
    } else {
        Some(vec!["text".to_string()].into())
    };
    let caps = openproxy_types::ModelCapabilities {
        vision: Some(vision),
        tool_calling: Some(true),
        reasoning: Some(true),
        thinking: Some(true),
        ..Default::default()
    };
    DiscoveredModel {
        model_id: ModelId::new(id),
        display_name: Some(name.to_string()),
        target_format: TargetFormat::Responses,
        context_length: Some(ctx),
        max_output_tokens: Some(32_768),
        input_modalities,
        output_modalities: Some(vec!["text".to_string()].into()),
        model_type: Some("chat".to_string()),
        family: Some("gpt".to_string()),
        capabilities: Some(caps),
    }
}

pub fn hardcoded_models() -> Vec<DiscoveredModel> {
    // Models from codex-rs/models-manager/models.json (official Codex bundled catalog)
    // Updated from https://github.com/openai/codex
    [
        ("gpt-6-astra", "GPT-6-Astra", 272_000, 872_000, true),
        ("gpt-6-sol", "GPT-6-Sol", 272_000, 872_000, true),
        ("gpt-6-luna", "GPT-6-Luna", 272_000, 872_000, true),
        ("gpt-5.6-sol", "GPT-5.6-Sol", 272_000, 872_000, true),
        ("gpt-5.6-terra", "GPT-5.6-Terra", 272_000, 872_000, true),
        ("gpt-5.6-luna", "GPT-5.6-Luna", 272_000, 872_000, true),
        (
            "gpt-daybreak-blue-latest",
            "Daybreak Blue",
            272_000,
            872_000,
            true,
        ),
        (
            "gpt-daybreak-red-latest",
            "Daybreak Red",
            372_000,
            372_000,
            true,
        ),
        ("gpt-5.5", "GPT-5.5", 272_000, 272_000, true),
        ("gpt-5.4", "GPT-5.4", 272_000, 1_000_000, true),
    ]
    .into_iter()
    .map(build_hardcoded_codex_model)
    .collect()
}

/// Parses upstream Codex models JSON response into a list of DiscoveredModel.
/// Supports `{ "models": [...] }`, `{ "data": [...] }`, or top-level arrays.
pub fn parse_codex_models_json(bytes: &[u8]) -> Result<Vec<DiscoveredModel>> {
    let json: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|e| CoreError::Parse(format!("codex models parse error: {e}")))?;

    let models_arr = json
        .get("models")
        .or_else(|| json.get("data"))
        .and_then(|v| v.as_array())
        .or_else(|| json.as_array())
        .ok_or_else(|| CoreError::Parse("codex models missing 'models' or 'data' array".into()))?;

    let mut items: Vec<(&serde_json::Value, i64)> = models_arr
        .iter()
        .map(|v| {
            let priority = v
                .get("priority")
                .and_then(|p| p.as_i64())
                .unwrap_or(i64::MAX);
            (v, priority)
        })
        .collect();
    items.sort_by_key(|(_, p)| *p);

    let discovered: Vec<DiscoveredModel> = items
        .into_iter()
        .filter_map(|(v, _)| map_codex_model_value(v))
        .collect();

    if discovered.is_empty() {
        return Err(CoreError::Parse(
            "codex models empty after filtering".into(),
        ));
    }

    Ok(discovered)
}

pub fn map_codex_model_value(val: &serde_json::Value) -> Option<DiscoveredModel> {
    let slug = val
        .get("slug")
        .or_else(|| val.get("id"))
        .and_then(|v| v.as_str())?;

    // Exclude internal review model
    if slug == "codex-auto-review" {
        return None;
    }

    let display_name = val
        .get("display_name")
        .or_else(|| val.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or(slug);

    let context_length = val
        .get("context_window")
        .or_else(|| val.get("context_length"))
        .and_then(|v| v.as_i64())
        .unwrap_or(272_000);

    let max_output_tokens = val
        .get("max_output_tokens")
        .and_then(|v| v.as_i64())
        .or(Some(32_768));

    let has_image = val
        .get("input_modalities")
        .and_then(|v| v.as_array())
        .is_none_or(|arr| arr.iter().any(|m| m.as_str() == Some("image")));

    let input_modalities = if has_image {
        Some(vec!["text".to_string(), "image".to_string()].into())
    } else {
        Some(vec!["text".to_string()].into())
    };

    let caps = openproxy_types::ModelCapabilities {
        vision: Some(has_image),
        tool_calling: Some(true),
        reasoning: Some(true),
        thinking: Some(true),
        ..Default::default()
    };

    Some(DiscoveredModel {
        model_id: ModelId::new(slug),
        display_name: Some(display_name.to_string()),
        target_format: TargetFormat::Responses,
        context_length: Some(context_length),
        max_output_tokens,
        input_modalities,
        output_modalities: Some(vec!["text".to_string()].into()),
        model_type: Some("chat".to_string()),
        family: Some("gpt".to_string()),
        capabilities: Some(caps),
    })
}

/// Merges backend-discovered models over the base catalog.
///
/// Matching models update their attributes in-place.
/// Backend-exclusive models (such as `gpt-reserve`) are appended.
/// Base models (such as `gpt-6-astra`, `gpt-6-sol`, `gpt-5.4`) are strictly preserved.
pub fn merge_codex_models(
    mut base: Vec<DiscoveredModel>,
    backend: Vec<DiscoveredModel>,
) -> Vec<DiscoveredModel> {
    for b_model in backend {
        if let Some(pos) = base.iter().position(|m| m.model_id == b_model.model_id) {
            base[pos] = b_model;
        } else {
            base.push(b_model);
        }
    }
    base
}

pub async fn try_fetch_backend_models(
    upstream_client: &Arc<UpstreamClient>,
    api_key: &str,
) -> Option<Vec<DiscoveredModel>> {
    let url = format!(
        "{CODEX_MODELS_URL}?client_version={}",
        current_codex_version()
    );
    let mut req = UpstreamRequest::get(&url);
    if let Ok(v) = http::HeaderValue::from_str(&format!("Bearer {api_key}")) {
        req.headers.insert(http::header::AUTHORIZATION, v);
    }
    req.headers.insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("application/json"),
    );
    apply_codex_spoofing_headers(&mut req);

    let cancel = CancellationToken::new();
    let resp = upstream_client
        .call(req, TimeoutProfile::ModelDiscovery, cancel)
        .await
        .ok()?;

    if !resp.status.is_success() {
        return None;
    }

    let body = resp.collect().await.ok()?;
    parse_codex_models_json(&body).ok()
}

pub async fn try_fetch_upstream_repo_models(
    upstream_client: &Arc<UpstreamClient>,
) -> Option<Vec<DiscoveredModel>> {
    let mut req = UpstreamRequest::get(CODEX_UPSTREAM_MODELS_RAW_URL);
    req.headers.insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("application/json"),
    );
    if let Ok(v) = http::HeaderValue::from_str(&current_codex_ua()) {
        req.headers.insert(http::header::USER_AGENT, v);
    }

    let cancel = CancellationToken::new();
    let resp = upstream_client
        .call(req, TimeoutProfile::ModelDiscovery, cancel)
        .await
        .ok()?;

    if !resp.status.is_success() {
        return None;
    }

    let body = resp.collect().await.ok()?;
    parse_codex_models_json(&body).ok()
}

pub async fn fetch_codex_models_pipeline(
    upstream_client: &Arc<UpstreamClient>,
    api_key: &str,
) -> Result<Vec<DiscoveredModel>> {
    // 1. Fetch base catalog from upstream repo (models.json) merged over embedded static models
    let base_models = if let Some(repo_models) = try_fetch_upstream_repo_models(upstream_client).await {
        merge_codex_models(hardcoded_models(), repo_models)
    } else {
        hardcoded_models()
    };

    // 2. If access token is available, discover dynamic backend models & merge over base catalog
    if !api_key.trim().is_empty()
        && let Some(backend_models) = try_fetch_backend_models(upstream_client, api_key).await
    {
        return Ok(merge_codex_models(base_models, backend_models));
    }

    // 3. Fallback to base models
    Ok(base_models)
}
