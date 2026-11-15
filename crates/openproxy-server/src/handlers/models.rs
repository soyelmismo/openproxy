//! `GET /v1/models` — OpenAI-compatible catalog with enriched capabilities,
//! following OmniRoute's format so clients like Cursor and Cline can
//! auto-detect context windows, vision support and tool calling.
//!
//! The shape unions the OpenAI `/v1/models` contract (`id`, `object`,
//! `created`, `owned_by` inside an `object: "list"` envelope) with OmniRoute's
//! capability fields (`context_length`, `max_input_tokens`, `max_output_tokens`,
//! `input_modalities`, `output_modalities`, `capabilities`, `type`, `family`).
//!
//! Capability values prefer the operator-edited columns in the `models` table;
//! [`openproxy_core::capabilities`] heuristics fill any `NULL` field, so rows
//! discovered before migration 000014 still serve a fully-populated response
//! (and get backfilled by [`openproxy_core::seed::backfill_model_metadata`]).
//!
//! Combos are also surfaced as synthetic `combo:<name>` entries, mirroring
//! OmniRoute's "combo as virtual model": clients can address a combo by alias
//! and the chat path resolves the alias to its target list.

use axum::{Json, extract::State, http::HeaderMap};
use openproxy_core::capabilities::resolve_effective_model_type;
use openproxy_core::{capabilities, models};
use openproxy_types::CoreError;

use crate::{error::ApiError, state::AppState};

pub fn router() -> axum::Router<AppState> {
    axum::Router::new().route("/models", axum::routing::get(list_models))
}

/// Default context length when neither the DB column nor the heuristic knows the
/// model: 128k, the modern chat default and what OpenRouter returns for unknowns.
const DEFAULT_CONTEXT_LENGTH: i64 = 128_000;

/// Default max output tokens when neither DB nor heuristic has a value:
/// 8 192, the conservative Claude / GPT-4-class cap.
const DEFAULT_MAX_OUTPUT_TOKENS: i64 = 8_192;

pub(crate) fn filter_models_for_key(
    rows: Vec<models::Model>,
    key: Option<&openproxy_core::api_keys::ApiKey>,
) -> Vec<models::Model> {
    match key {
        Some(k) => rows
            .into_iter()
            .filter(|m| k.is_model_allowed(m.model_id.as_str(), Some(m.provider_id.as_str())))
            .collect(),
        None => rows,
    }
}

/// Exact ACL key, independent of key identity and last-used timestamps.
pub(crate) fn catalog_scope(key: Option<&openproxy_core::api_keys::ApiKey>) -> String {
    match key {
        None => "operator".into(),
        Some(k) => format!(
            "{:?}",
            (
                &k.allowed_models,
                &k.allowed_combos,
                &k.allowed_providers,
                &k.blacklisted_models,
                &k.blacklisted_providers,
                k.only_whitelisted,
            )
        ),
    }
}

pub(crate) fn filter_combos_for_key(
    combos: Vec<openproxy_types::Combo>,
    key: Option<&openproxy_core::api_keys::ApiKey>,
) -> Vec<openproxy_types::Combo> {
    match key {
        Some(k) => {
            // A non-empty `allowed_combos` is the authoritative combo gate: it
            // holds resolved combo ids, exactly what the routing layer enforces
            // (`is_combo_allowed`). The model-name check below is only for the
            // legacy shape where combo names ride inside `allowed_models`.
            // A strict key (`only_whitelisted = 1`) with an empty model list
            // must still list its allowed combos.
            let explicit_combo_gate = k.allowed_combos.as_deref().is_some_and(|a| !a.is_empty());
            combos
                .into_iter()
                .filter(|c| {
                    if !k.is_combo_allowed(c.id.0) {
                        return false;
                    }
                    if explicit_combo_gate {
                        return true;
                    }
                    let combo_virtual_id = format!("combo:{}", c.name);
                    k.is_model_allowed(&combo_virtual_id, None) || k.is_model_allowed(&c.name, None)
                })
                .collect()
        }
        None => combos,
    }
}

fn format_anthropic_models_response(data: Vec<serde_json::Value>) -> serde_json::Value {
    let anthropic_data: Vec<serde_json::Value> = data
        .into_iter()
        .map(|item| {
            let id = item.get("id").and_then(|v| v.as_str()).unwrap_or_default();
            serde_json::json!({
                "type": "model",
                "id": id,
                "display_name": id,
                "created_at": "2024-02-29T00:00:00Z"
            })
        })
        .collect();

    let first_id = anthropic_data
        .first()
        .and_then(|v| v.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let last_id = anthropic_data
        .last()
        .and_then(|v| v.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    serde_json::json!({
        "data": anthropic_data,
        "has_more": false,
        "first_id": first_id,
        "last_id": last_id,
    })
}

pub async fn list_models(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<axum::response::Response, ApiError> {
    let maybe_api_key = authenticate_chat(&state, &headers).await?;

    let flight_key = format!(
        "models:{}:{}",
        catalog_scope(maybe_api_key.as_deref()),
        headers.contains_key("anthropic-version") || headers.contains_key("x-api-key")
    );
    state
        .read_flights()
        .json(flight_key, move || {
            let raw_models = state
                .services()
                .models
                .list_active_all(std::time::Duration::from_secs(5))?;
            let raw_combos = state.services().combos.list_combos()?;

            let rows = filter_models_for_key(raw_models, maybe_api_key.as_deref());
            let combo_rows = filter_combos_for_key(raw_combos, maybe_api_key.as_deref());

            let mut data: Vec<serde_json::Value> =
                rows.into_iter().map(|m| build_model_entry(&m)).collect();
            for c in &combo_rows {
                let effective_cw = state
                    .services()
                    .combos
                    .compute_effective_context_window(c.id)
                    .ok()
                    .flatten()
                    .or(c.context_window);
                let effective_mot = state
                    .services()
                    .combos
                    .compute_effective_max_output_tokens(c.id)
                    .ok()
                    .flatten()
                    .or(c.max_output_tokens);
                let effective_caps = state
                    .services()
                    .combos
                    .compute_effective_capabilities(c.id)
                    .ok()
                    .flatten();
                // One entry per combo, under its bare routing name: the
                // `combo:<name>` virtual id is still routed but not advertised,
                // otherwise every combo appears twice in the model list.
                data.push(build_combo_entry(
                    c,
                    Some(&c.name),
                    effective_cw,
                    effective_mot,
                    effective_caps.as_ref(),
                ));
            }

            let is_anthropic =
                headers.contains_key("anthropic-version") || headers.contains_key("x-api-key");
            if is_anthropic {
                Ok(Json(format_anthropic_models_response(data)))
            } else {
                Ok(Json(serde_json::json!({
                    "object": "list",
                    "data": data,
                })))
            }
        })
        .await
}

fn extract_auth_header_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::trim)
        .or_else(|| {
            headers
                .get("x-api-key")
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
        })
}

/// Authenticate with a chat-scope key. Every /v1 request requires an api key
/// since 2026-11-11 (ghost-request fix): the anonymous first-boot window is
/// gone, so an unauthenticated catalog request is rejected with 401.
async fn authenticate_chat(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<std::sync::Arc<openproxy_core::api_keys::ApiKey>>, ApiError> {
    let token = extract_auth_header_token(headers);

    let Some(token) = token else {
        return Err(ApiError(CoreError::Auth("missing api key".into())));
    };

    if token.is_empty() {
        return Err(ApiError(CoreError::Auth("missing api key".into())));
    }

    let key = crate::middleware::auth::verify_key_credentials(state, token, "chat").await?;
    Ok(Some(key))
}

fn build_supported_parameters(caps: &capabilities::ModelCapabilities) -> Vec<&'static str> {
    let mut params = vec!["max_tokens", "temperature", "top_p", "stream", "stop"];
    if caps.tool_calling == Some(true) {
        params.push("tools");
        params.push("tool_choice");
    }
    if caps.reasoning == Some(true) || caps.thinking == Some(true) {
        params.push("reasoning_effort");
        params.push("thinking");
    }
    if caps.structured_output == Some(true) {
        params.push("response_format");
    }
    params
}

/// Project a combo into a synthetic catalog entry shaped like `build_model_entry`
/// so the catalog stays homogeneous. Capabilities are populated from its effective targets.
pub(crate) fn build_combo_entry(
    c: &openproxy_types::Combo,
    id_override: Option<&str>,
    effective_context_window: Option<i64>,
    effective_max_output_tokens: Option<i64>,
    effective_caps: Option<&capabilities::ModelCapabilities>,
) -> serde_json::Value {
    let id = id_override.map_or_else(
        || format!("combo:{}", c.name),
        std::string::ToString::to_string,
    );
    let empty_caps = capabilities::ModelCapabilities::empty();
    let caps_ref = effective_caps.unwrap_or(&empty_caps);
    let combo_type = if caps_ref.image == Some(true) {
        "image"
    } else {
        capabilities::infer_model_type(&c.name)
    };

    let input_modalities: Vec<String> =
        capabilities::infer_input_modalities_for_model(&c.name, caps_ref)
            .into_iter()
            .map(std::string::ToString::to_string)
            .collect();
    let output_modalities: Vec<String> = capabilities::infer_output_modalities(&c.name)
        .into_iter()
        .map(std::string::ToString::to_string)
        .collect();

    let is_reasoning = caps_ref.reasoning == Some(true) || caps_ref.thinking == Some(true);
    let supported_params = build_supported_parameters(caps_ref);

    let max_output_tokens = effective_max_output_tokens
        .or_else(|| capabilities::infer_max_output_tokens(&c.name))
        .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);

    serde_json::json!({
        "id": id,
        "object": "model",
        "created": unix_now_secs(),
        "owned_by": "combo",
        "permission": [],
        "root": id,
        "parent": null,
        "context_length": effective_context_window,
        "max_input_tokens": effective_context_window,
        "max_output_tokens": max_output_tokens,
        "input_modalities": input_modalities,
        "output_modalities": output_modalities,
        "capabilities": build_capabilities_object(caps_ref),
        "supports_reasoning": is_reasoning,
        "supported_parameters": supported_params,
        "type": combo_type,
        "family": "combo",
    })
}

fn parse_modalities_json_or(
    json_str: Option<&str>,
    fallback: impl FnOnce() -> Vec<String>,
) -> Vec<String> {
    json_str
        .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
        .unwrap_or_else(fallback)
}

/// Project one `core::models::Model` row to the enriched OpenAI-shape JSON the
/// public endpoint returns. Split out of the handler so it is unit-testable
/// without an axum router.
pub(crate) fn build_model_entry(m: &models::Model) -> serde_json::Value {
    let model_id = m.model_id.as_str();
    let provider_id = m.provider_id.as_str();
    let full_id = format!("{provider_id}/{model_id}");

    let caps = m.capabilities_json.as_deref().map_or_else(
        || capabilities::infer_capabilities(model_id),
        |json| capabilities::ModelCapabilities::from_json(Some(json)),
    );

    let context_length = m
        .context_length
        .or_else(|| capabilities::infer_context_length(model_id))
        .unwrap_or(DEFAULT_CONTEXT_LENGTH);

    let max_output_tokens = m
        .max_output_tokens
        .or_else(|| capabilities::infer_max_output_tokens(model_id))
        .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);

    let input_modalities = parse_modalities_json_or(m.input_modalities_json.as_deref(), || {
        capabilities::infer_input_modalities_for_model(model_id, &caps)
            .into_iter()
            .map(std::string::ToString::to_string)
            .collect()
    });

    let output_modalities = parse_modalities_json_or(m.output_modalities_json.as_deref(), || {
        capabilities::infer_output_modalities(model_id)
            .into_iter()
            .map(std::string::ToString::to_string)
            .collect()
    });

    let inferred_type = capabilities::infer_model_type(model_id);
    let effective_type = resolve_effective_model_type(&m.model_type, m.custom, inferred_type);

    let family = m
        .family
        .clone()
        .or_else(|| capabilities::infer_family(model_id).map(Into::into));

    let is_reasoning = caps.reasoning == Some(true) || caps.thinking == Some(true);
    let supported_params = build_supported_parameters(&caps);

    serde_json::json!({
        "id": full_id,
        "object": "model",
        "created": unix_now_secs(),
        "owned_by": provider_id,
        "permission": [],
        "root": full_id,
        "parent": null,
        "context_length": context_length,
        "max_input_tokens": context_length,
        "max_output_tokens": max_output_tokens,
        "input_modalities": input_modalities,
        "output_modalities": output_modalities,
        "capabilities": build_capabilities_object(&caps),
        "supports_reasoning": is_reasoning,
        "supported_parameters": supported_params,
        "type": effective_type,
        "family": family,
    })
}

/// Build the inner `capabilities` object, omitting `null`s so it reads
/// `{"vision": true, "tool_calling": true}` instead of
/// `{"vision": true, "reasoning": null}`. Omission keeps `if (caps.reasoning)`
/// client checks correct.
fn build_capabilities_object(caps: &capabilities::ModelCapabilities) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    let fields: [(&str, Option<bool>); 9] = [
        ("vision", caps.vision),
        ("tool_calling", caps.tool_calling),
        ("reasoning", caps.reasoning),
        ("thinking", caps.thinking),
        ("attachment", caps.attachment),
        ("structured_output", caps.structured_output),
        ("temperature", caps.temperature),
        ("decisions", caps.decisions),
        ("video", caps.video),
    ];
    for (name, val) in fields {
        if let Some(v) = val {
            out.insert(name.into(), serde_json::Value::Bool(v));
        }
    }
    serde_json::Value::Object(out)
}

fn unix_now_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests;
