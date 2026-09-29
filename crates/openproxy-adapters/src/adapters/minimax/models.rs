//! Upstream model discovery and catalog resolution for MiniMax Coding.
//!
//! Dynamically absorbs model definitions, limits, and capabilities from the canonical
//! upstream `MiniMax-AI/minimax-code` repository (`packages/config/src/config.ts`),
//! with thread-safe in-memory caching (`RwLock`), runtime overrides, and offline fallback.

use crate::upstream::{CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest};
use openproxy_types::{DiscoveredModel, ModelId, Result, TargetFormat};
use std::sync::{Arc, RwLock};

/// Canonical raw GitHub URL for upstream MiniMax Code config.
pub const MINIMAX_UPSTREAM_CONFIG_RAW_URL: &str =
    "https://raw.githubusercontent.com/MiniMax-AI/minimax-code/main/packages/config/src/config.ts";

/// Canonical raw GitHub URL for upstream MiniMax Code model catalog.
pub const MINIMAX_UPSTREAM_CATALOG_RAW_URL: &str = "https://raw.githubusercontent.com/MiniMax-AI/minimax-code/main/packages/config/src/minimax-model-catalog.ts";

static DYNAMIC_MINIMAX_MODELS: RwLock<Option<Vec<DiscoveredModel>>> = RwLock::new(None);

/// Returns the currently cached in-memory dynamic models, if any.
pub fn current_dynamic_minimax_models() -> Option<Vec<DiscoveredModel>> {
    DYNAMIC_MINIMAX_MODELS.read().ok()?.clone()
}

/// Sets in-memory dynamic models at runtime without restarting.
pub fn set_dynamic_minimax_models(models: Vec<DiscoveredModel>) {
    if let Ok(mut guard) = DYNAMIC_MINIMAX_MODELS.write() {
        *guard = Some(models);
    }
}

/// Resets in-memory dynamic models (useful for tests and cache clearing).
pub fn reset_dynamic_minimax_models() {
    if let Ok(mut guard) = DYNAMIC_MINIMAX_MODELS.write() {
        *guard = None;
    }
}

fn build_minimax_builtin_model(
    id: &str,
    display: &str,
    cw: i64,
    out: i64,
    vision: bool,
    reasoning: bool,
) -> DiscoveredModel {
    use crate::adapters::discovery::build_discovered_model_full;
    let mut m = build_discovered_model_full(
        id.to_string(),
        Some(display.to_string()),
        TargetFormat::Anthropic,
        Some(cw),
        Some(out),
    );
    if let Some(ref mut caps) = m.capabilities {
        caps.vision = Some(vision);
        caps.tool_calling = Some(true);
        caps.reasoning = Some(reasoning);
        caps.thinking = Some(reasoning);
    }
    if vision {
        m.input_modalities = Some(vec!["text".to_string(), "image".to_string()].into());
    }
    m
}

/// Static built-in fallback models for MiniMax Coding.
pub fn minimax_builtin_models() -> Vec<DiscoveredModel> {
    vec![
        build_minimax_builtin_model(
            "MiniMax-M3.1-Flash-Preview",
            "MiniMax-M3.1-Flash-Preview",
            1_000_000,
            128_000,
            true,
            true,
        ),
        build_minimax_builtin_model("MiniMax-M3", "MiniMax-M3", 1_000_000, 128_000, true, true),
        build_minimax_builtin_model(
            "MiniMax-M2.7-highspeed",
            "MiniMax-M2.7-highspeed",
            200_000,
            128_000,
            false,
            true,
        ),
        build_minimax_builtin_model(
            "MiniMax-M2.7",
            "MiniMax-M2.7",
            200_000,
            128_000,
            false,
            true,
        ),
        build_minimax_builtin_model(
            "minimax-m2.1",
            "MiniMax-M2.1",
            200_000,
            128_000,
            false,
            true,
        ),
        build_minimax_builtin_model("MiniMax-M2", "MiniMax-M2", 200_000, 128_000, false, true),
    ]
}

/// Merges dynamic or upstream models over a base catalog.
/// Matching models update their attributes in-place.
/// Upstream-exclusive models are appended.
/// Base models not in upstream are preserved as fallbacks.
pub fn merge_minimax_models(
    mut base: Vec<DiscoveredModel>,
    incoming: Vec<DiscoveredModel>,
) -> Vec<DiscoveredModel> {
    for in_model in incoming {
        if let Some(pos) = base.iter().position(|m| m.model_id == in_model.model_id) {
            base[pos] = in_model;
        } else {
            base.push(in_model);
        }
    }
    base
}

fn load_env_minimax_models() -> Option<Vec<DiscoveredModel>> {
    let val = std::env::var("OPENPROXY_MINIMAX_MODELS_JSON").ok()?;
    let trimmed = val.trim();
    if trimmed.is_empty() {
        return None;
    }
    serde_json::from_str::<Vec<DiscoveredModel>>(trimmed).ok()
}

/// Parses the upstream `config.ts` TypeScript source file and extracts `MINIMAX_MODELS`.
pub fn parse_minimax_config_ts(content: &str) -> Option<Vec<DiscoveredModel>> {
    let start_idx = content.find("MINIMAX_MODELS")?;
    let after = &content[start_idx..];
    let brace_offset = after.find('{')?;
    let brace_start = start_idx + brace_offset;

    let mut models = Vec::new();
    let mut depth: u32 = 0;
    let mut in_quote: Option<char> = None;
    let mut key_buf = String::new();
    let mut body_buf = String::new();
    let mut current_key: Option<String> = None;

    let chars: Vec<char> = content[brace_start..].chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = in_quote {
            if c == '\\' && i + 1 < chars.len() {
                if depth >= 2 {
                    body_buf.push(c);
                    body_buf.push(chars[i + 1]);
                }
                i += 2;
                continue;
            } else if c == q {
                in_quote = None;
                if depth == 1 {
                    key_buf.push(c);
                } else if depth >= 2 {
                    body_buf.push(c);
                }
            } else if depth == 1 {
                key_buf.push(c);
            } else if depth >= 2 {
                body_buf.push(c);
            }
        } else if c == '"' || c == '\'' || c == '`' {
            in_quote = Some(c);
            if depth == 1 {
                key_buf.push(c);
            } else if depth >= 2 {
                body_buf.push(c);
            }
        } else if c == '{' {
            depth += 1;
            if depth == 2 {
                let cleaned = clean_model_key(&key_buf);
                current_key = Some(cleaned);
                key_buf.clear();
                body_buf.clear();
            } else if depth > 2 {
                body_buf.push(c);
            }
        } else if c == '}' {
            if depth == 2 {
                if let Some(key) = current_key.take()
                    && !key.is_empty()
                    && let Some(model) = parse_single_minimax_model(&key, &body_buf)
                {
                    models.push(model);
                }
                key_buf.clear();
                body_buf.clear();
            } else if depth > 2 {
                body_buf.push(c);
            }
            depth = depth.saturating_sub(1);
            if depth == 0 {
                break;
            }
        } else if depth == 1 {
            key_buf.push(c);
        } else if depth >= 2 {
            body_buf.push(c);
        }
        i += 1;
    }

    if models.is_empty() {
        None
    } else {
        Some(models)
    }
}

fn clean_model_key(raw: &str) -> String {
    raw.trim()
        .trim_start_matches([',', ';', '\n', '\r'])
        .trim()
        .trim_end_matches(':')
        .trim()
        .trim_matches(['"', '\'', '`'])
        .trim()
        .to_string()
}

fn extract_string_property(body: &str, prop: &str) -> Option<String> {
    let prop_idx = body.find(prop)?;
    let after_prop = &body[prop_idx + prop.len()..];
    let colon_idx = after_prop.find(':')?;
    let after_colon = after_prop[colon_idx + 1..].trim_start();
    let quote = after_colon.chars().next()?;
    if quote != '"' && quote != '\'' && quote != '`' {
        return None;
    }
    let rest = &after_colon[quote.len_utf8()..];
    let end_quote = rest.find(quote)?;
    Some(rest[..end_quote].to_string())
}

fn extract_object_block<'a>(body: &'a str, prop: &str) -> Option<&'a str> {
    let prop_idx = body.find(prop)?;
    let after_prop = &body[prop_idx + prop.len()..];
    let brace_start = after_prop.find('{')?;
    let after_brace = &after_prop[brace_start + 1..];
    let brace_end = after_brace.find('}')?;
    Some(&after_brace[..brace_end])
}

fn extract_int_property(body: &str, prop: &str) -> Option<i64> {
    let mut search_from = 0;
    while let Some(rel_idx) = body[search_from..].find(prop) {
        let prop_idx = search_from + rel_idx;
        let after_prop = &body[prop_idx + prop.len()..];
        if let Some(colon_idx) = after_prop.find(':') {
            let after_colon = after_prop[colon_idx + 1..].trim_start();
            let digits: String = after_colon
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if let Ok(num) = digits.parse::<i64>() {
                return Some(num);
            }
        }
        search_from = prop_idx + prop.len();
    }
    None
}

fn extract_array_property<'a>(body: &'a str, prop: &str) -> Option<&'a str> {
    let prop_idx = body.find(prop)?;
    let after_prop = &body[prop_idx + prop.len()..];
    let bracket_start = after_prop.find('[')?;
    let after_bracket = &after_prop[bracket_start + 1..];
    let bracket_end = after_bracket.find(']')?;
    Some(&after_bracket[..bracket_end])
}

fn parse_single_minimax_model(key: &str, body: &str) -> Option<DiscoveredModel> {
    if key.is_empty() || key.contains('{') || key.contains('}') {
        return None;
    }

    let display_name = extract_string_property(body, "name").unwrap_or_else(|| key.to_string());

    let mut max_cw: Option<i64> = None;
    if let Some(cw_options) = extract_array_property(body, "contextWindowOptions") {
        for part in cw_options.split(',') {
            if let Ok(num) = part.trim().parse::<i64>() {
                max_cw = Some(max_cw.map_or(num, |m| m.max(num)));
            }
        }
    }

    let limit_block = extract_object_block(body, "limit").unwrap_or(body);
    let limit_context = extract_int_property(limit_block, "context")
        .or_else(|| extract_int_property(body, "context"))
        .unwrap_or(200_000);
    let context_length = max_cw.unwrap_or(limit_context);
    let max_output_tokens = extract_int_property(limit_block, "output")
        .or_else(|| extract_int_property(body, "output"))
        .unwrap_or(128_000);

    let has_image = body.contains("\"image\"") || body.contains("'image'");
    let has_video = body.contains("\"video\"") || body.contains("'video'");
    let mut input_modalities = vec!["text".to_string()];
    if has_image {
        input_modalities.push("image".to_string());
    }
    if has_video {
        input_modalities.push("video".to_string());
    }

    let has_tools = body.contains("tool_call") || body.contains("tool_calling");
    let has_reasoning = body.contains("reasoning");
    let has_thinking = body.contains("thinking");

    let mut caps = openproxy_types::capabilities::infer_capabilities(key);
    caps.vision = Some(has_image);
    caps.tool_calling = Some(has_tools);
    caps.reasoning = Some(has_reasoning);
    caps.thinking = Some(has_reasoning || has_thinking);

    Some(DiscoveredModel {
        model_id: ModelId::new(key),
        display_name: Some(display_name),
        target_format: TargetFormat::Anthropic,
        context_length: Some(context_length),
        max_output_tokens: Some(max_output_tokens),
        input_modalities: Some(input_modalities.into()),
        output_modalities: Some(vec!["text".to_string()].into()),
        model_type: Some("chat".to_string()),
        family: Some("minimax".to_string()),
        capabilities: Some(caps),
    })
}

async fn fetch_and_parse_ts(
    upstream_client: &Arc<UpstreamClient>,
    url: &str,
) -> Option<Vec<DiscoveredModel>> {
    let mut req = UpstreamRequest::get(url);
    req.headers.insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("text/plain, application/javascript, */*"),
    );
    req.headers.insert(
        http::header::USER_AGENT,
        http::HeaderValue::from_static("OpenProxy-ModelDiscovery/1.0"),
    );

    let cancel = CancellationToken::new();
    let resp = upstream_client
        .call(req, TimeoutProfile::ModelDiscovery, cancel)
        .await
        .ok()?;

    if !resp.status.is_success() {
        return None;
    }

    let body = resp.collect().await.ok()?;
    let text = String::from_utf8_lossy(&body);
    parse_minimax_config_ts(&text)
}

/// Attempts to fetch and parse the upstream model catalog from the official GitHub repository.
pub async fn try_fetch_upstream_minimax_models(
    upstream_client: &Arc<UpstreamClient>,
) -> Option<Vec<DiscoveredModel>> {
    if let Ok(url) = std::env::var("OPENPROXY_MINIMAX_CONFIG_URL") {
        return fetch_and_parse_ts(upstream_client, &url).await;
    }

    if let Some(models) =
        fetch_and_parse_ts(upstream_client, MINIMAX_UPSTREAM_CATALOG_RAW_URL).await
    {
        return Some(models);
    }

    fetch_and_parse_ts(upstream_client, MINIMAX_UPSTREAM_CONFIG_RAW_URL).await
}

/// Environment JSON override (`OPENPROXY_MINIMAX_MODELS_JSON`), then upstream
/// definitions, then this session's cache, then the OpenAI `/models` endpoint for a
/// BYOK (`sk-...`) key, then the built-in catalog.
pub async fn fetch_minimax_models_pipeline(
    upstream_client: &Arc<UpstreamClient>,
    api_key: &str,
) -> Result<Vec<DiscoveredModel>> {
    if let Some(env_models) = load_env_minimax_models() {
        return Ok(env_models);
    }

    if let Some(upstream_models) = try_fetch_upstream_minimax_models(upstream_client).await {
        let merged = merge_minimax_models(minimax_builtin_models(), upstream_models);
        set_dynamic_minimax_models(merged.clone());
        return Ok(merged);
    }

    if let Some(cached) = current_dynamic_minimax_models() {
        return Ok(cached);
    }

    let trimmed = api_key.trim();
    if trimmed.starts_with("sk-")
        && let Ok(models) = crate::adapters::fetch_openai_models(
            "https://api.minimax.io/v1/models",
            upstream_client,
            trimmed,
            "minimax",
            TargetFormat::Anthropic,
        )
        .await
        && !models.is_empty()
    {
        return Ok(merge_minimax_models(minimax_builtin_models(), models));
    }

    Ok(minimax_builtin_models())
}
