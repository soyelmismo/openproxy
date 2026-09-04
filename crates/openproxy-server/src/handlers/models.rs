//! `GET /v1/models` — OpenAI-compatible response with enriched
//! capabilities.
//!
//! Based on OmniRoute's catalog format so clients like Cursor and Cline
//! can auto-detect context windows, vision support, tool calling, etc.
//!
//! The shape is the union of:
//! - The OpenAI `/v1/models` contract (`id`, `object`, `created`,
//!   `owned_by`, plus a list-shaped envelope with `object: "list"`).
//! - OmniRoute's capability fields (`context_length`,
//!   `max_input_tokens`, `max_output_tokens`, `input_modalities`,
//!   `output_modalities`, `capabilities`, `type`, `family`).
//!
//! Capability values prefer the operator-edited values stored in the
//! `models` table; the [`openproxy_core::capabilities`] heuristic is
//! the fallback for any field that is `NULL` on the row. This means
//! rows discovered before migration 000014 still produce a fully-
//! populated response on the first request after the migration (and
//! also get backfilled to the DB by
//! [`openproxy_core::seed::backfill_model_metadata`]).
//!
//! In addition to the real models in the `models` table, this handler
//! also surfaces every combo as a synthetic `combo:<name>` entry.
//! This mirrors OmniRoute's "combo as virtual model" behaviour:
//! clients that consume the catalog (Cursor, Cline, the dashboard's
//! model picker) can address a combo by its alias and the chat path
//! resolves the alias to the combo's target list.

use axum::{Json, extract::State, http::HeaderMap};
use openproxy_core::capabilities::resolve_effective_model_type;
use openproxy_core::{capabilities, models};
use openproxy_types::CoreError;

use crate::{error::ApiError, state::AppState};

pub fn router() -> axum::Router<AppState> {
    axum::Router::new().route("/models", axum::routing::get(list_models))
}

/// Default context length to report when neither the DB column nor
/// the heuristic knows the model. 128k is the modern chat default and
/// matches what OpenRouter returns for unknown models.
const DEFAULT_CONTEXT_LENGTH: i64 = 128_000;

/// Default max output tokens when neither the DB nor the heuristic
/// has a value. 8 192 is the conservative Claude / GPT-4-class cap.
const DEFAULT_MAX_OUTPUT_TOKENS: i64 = 8_192;

fn filter_models_for_key(
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

fn filter_combos_for_key(
    combos: Vec<openproxy_types::Combo>,
    key: Option<&openproxy_core::api_keys::ApiKey>,
) -> Vec<openproxy_types::Combo> {
    match key {
        Some(k) => combos
            .into_iter()
            .filter(|c| {
                if !k.is_combo_allowed(c.id.0) {
                    return false;
                }
                let combo_virtual_id = format!("combo:{}", c.name);
                k.is_model_allowed(&combo_virtual_id, None) || k.is_model_allowed(&c.name, None)
            })
            .collect(),
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
) -> Result<Json<serde_json::Value>, ApiError> {
    let maybe_api_key = authenticate_chat_or_anonymous(&state, &headers)?;

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
        let effective_cw = c.context_window.or_else(|| {
            state
                .services()
                .combos
                .compute_effective_context_window(c.id)
                .unwrap_or(None)
        });
        data.push(build_combo_entry(c, effective_cw));
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

/// Authenticate with a chat-scope key, OR allow anonymous when zero
/// active keys exist (first-boot window). Returns the key if
/// authenticated, or None if anonymous.
fn authenticate_chat_or_anonymous(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<std::sync::Arc<openproxy_core::api_keys::ApiKey>>, ApiError> {
    let token = extract_auth_header_token(headers);

    let Some(token) = token else {
        let active = state.services().api_keys.count_active().map_err(ApiError)?;
        if active == 0 {
            return Ok(None);
        }
        return Err(ApiError(CoreError::Auth("missing api key".into())));
    };

    if token.is_empty() {
        return Err(ApiError(CoreError::Auth("missing api key".into())));
    }

    let key = crate::middleware::auth::verify_key_credentials(state, token, "chat")?;
    Ok(Some(key))
}

/// Project a combo into a synthetic catalog entry. The shape mirrors
/// `build_model_entry` so the catalog stays homogeneous — clients
/// that just iterate `data` see a list of models where some happen
/// to be combos. Capability fields are `null` because a combo is an
/// alias for an operator-chosen list of targets, not a real model;
/// per-model metadata would be misleading.
fn build_combo_entry(
    c: &openproxy_types::Combo,
    effective_context_window: Option<i64>,
) -> serde_json::Value {
    let id = format!("combo:{}", c.name);
    let combo_type = capabilities::infer_model_type(&c.name);
    let empty_caps = capabilities::ModelCapabilities::empty();
    let input_modalities: Vec<String> =
        capabilities::infer_input_modalities_for_model(&c.name, &empty_caps)
            .into_iter()
            .map(std::string::ToString::to_string)
            .collect();
    let output_modalities: Vec<String> = capabilities::infer_output_modalities(&c.name)
        .into_iter()
        .map(std::string::ToString::to_string)
        .collect();

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
        "max_output_tokens": null,
        "input_modalities": input_modalities,
        "output_modalities": output_modalities,
        "capabilities": serde_json::json!({}),
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

/// Project one `core::models::Model` row to the enriched OpenAI-shape
/// JSON object the public endpoint returns. Lifted out of the handler
/// body so it can be unit-tested without spinning up an axum router.
fn build_model_entry(m: &models::Model) -> serde_json::Value {
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
        "type": effective_type,
        "family": family,
    })
}

/// Build the inner `capabilities` JSON object, omitting `null` values
/// so the field is `{ "vision": true, "tool_calling": true, ... }`
/// rather than `{ "vision": true, "tool_calling": true, "reasoning":
/// null, ... }`. The omission makes clients that just look for
/// `if (caps.reasoning)` work correctly.
fn build_capabilities_object(caps: &capabilities::ModelCapabilities) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    let fields: [(&str, Option<bool>); 7] = [
        ("vision", caps.vision),
        ("tool_calling", caps.tool_calling),
        ("reasoning", caps.reasoning),
        ("thinking", caps.thinking),
        ("attachment", caps.attachment),
        ("structured_output", caps.structured_output),
        ("temperature", caps.temperature),
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
mod tests {
    use super::*;
    use openproxy_core::models::{Model, TargetFormat};
    use openproxy_types::{ModelId, ModelRowId, ProviderId};

    fn empty_model() -> Model {
        Model {
            row_id: ModelRowId(1),
            provider_id: ProviderId::new("openrouter"),
            model_id: ModelId::new("openai/gpt-4o"),
            display_name: None,
            target_format: TargetFormat::Openai,
            discovered_at: "2024-01-01 00:00:00".into(),
            expires_at: None,
            timeout_overrides_json: None,
            active: true,
            last_test_status: None,
            last_test_at: None,
            custom: false,
            // All metadata fields empty — exercises the heuristic
            // fallback in `build_model_entry`.
            context_length: None,
            max_output_tokens: None,
            capabilities_json: None,
            family: None,
            model_type: "chat".into(),
            input_modalities_json: None,
            output_modalities_json: None,
            ..Default::default()
        }
    }

    #[test]
    fn gpt4o_falls_back_to_heuristic() {
        let m = empty_model();
        let v = build_model_entry(&m);
        // Vision should be detected from the model_id heuristic.
        let caps = v.get("capabilities").and_then(|c| c.get("vision")).unwrap();
        assert_eq!(caps, &serde_json::Value::Bool(true));
        // Context length should be the heuristic-known 128_000.
        assert_eq!(v.get("context_length").unwrap().as_i64(), Some(128_000));
    }

    #[test]
    fn db_values_override_heuristic() {
        let mut m = empty_model();
        m.context_length = Some(999_999);
        m.capabilities_json = Some(r#"{"vision": false}"#.into());
        let v = build_model_entry(&m);
        // The DB value wins: vision is explicitly `false` (and the
        // field is still present, not omitted, because we got a
        // value).
        let caps = v.get("capabilities").unwrap();
        assert_eq!(caps.get("vision"), Some(&serde_json::Value::Bool(false)));
        assert_eq!(v.get("context_length").unwrap().as_i64(), Some(999_999));
    }

    #[test]
    fn capabilities_object_omits_nulls() {
        let m = empty_model();
        let v = build_model_entry(&m);
        let caps = v.get("capabilities").and_then(|c| c.as_object()).unwrap();
        // For a heuristic-inferred gpt-4o row, the capability fields
        // that are inferable (vision, tool_calling, structured_output,
        // temperature, attachment) are all present. The `reasoning`
        // and `thinking` fields are *not* present — gpt-4o doesn't
        // match the reasoning keywords — which is exactly the
        // omit-on-null contract the test is guarding.
        for key in [
            "vision",
            "tool_calling",
            "structured_output",
            "temperature",
            "attachment",
        ] {
            assert!(caps.contains_key(key), "missing key {key}");
        }
        assert!(
            !caps.contains_key("reasoning"),
            "reasoning should be omitted for a non-reasoning model"
        );
        // `created` is set to a non-zero unix timestamp.
        assert!(v.get("created").unwrap().as_i64().unwrap() > 0);
        // `object: "model"`, `owned_by` round-trips.
        assert_eq!(v.get("object").unwrap().as_str(), Some("model"));
        assert_eq!(v.get("owned_by").unwrap().as_str(), Some("openrouter"));
    }

    #[test]
    fn id_is_provider_prefixed() {
        // The proxy-level id must include the provider prefix so
        // round-tripping through the chat endpoint is unambiguous.
        // The test pins down the exact shape: `<provider>/<upstream_id>`.
        let m = empty_model();
        let v = build_model_entry(&m);
        let id = v
            .get("id")
            .and_then(|x| x.as_str())
            .expect("id is a string");
        // empty_model() uses provider "openrouter" + upstream "openai/gpt-4o".
        assert_eq!(
            id, "openrouter/openai/gpt-4o",
            "id must be provider-prefixed"
        );
        // `root` mirrors `id` to keep SDKs that compare them happy.
        let root = v
            .get("root")
            .and_then(|x| x.as_str())
            .expect("root is a string");
        assert_eq!(root, id, "root mirrors id");
    }

    #[test]
    fn id_handles_already_prefixed_upstream_id() {
        // Upstream ids that already contain a `/` (e.g.
        // `nex-agi/nex-n2-pro:free` from OpenRouter) end up with two
        // slashes in the proxy id: `openrouter/nex-agi/nex-n2-pro:free`.
        // This is the expected behavior — only the first `/` is the
        // provider/upstream separator; any later `/` is part of the
        // upstream model name.
        let mut m = empty_model();
        m.model_id = ModelId::new("nex-agi/nex-n2-pro:free");
        let v = build_model_entry(&m);
        let id = v
            .get("id")
            .and_then(|x| x.as_str())
            .expect("id is a string");
        assert_eq!(id, "openrouter/nex-agi/nex-n2-pro:free");
    }

    #[test]
    fn api_key_model_filtering_logic() {
        let key = openproxy_core::api_keys::ApiKey {
            id: openproxy_types::ApiKeyId(1),
            key_hash: "hash".into(),
            key_prefix: Some("op_live_test".into()),
            label: Some("test".into()),
            scopes: vec!["chat".into()],
            allowed_models: None,
            allowed_combos: None,
            blacklisted_providers: Some(vec!["openrouter".into()]),
            blacklisted_models: Some(vec!["gpt-3.5*".into()]),
            is_active: true,
            revoked_at: None,
            expires_at: None,
            last_used_at: None,
            created_at: "2024-01-01".into(),
            created_by: None,
        };

        let m1 = empty_model(); // provider: openrouter, model: openai/gpt-4o
        let mut m2 = empty_model();
        m2.provider_id = ProviderId::new("openai");
        m2.model_id = ModelId::new("gpt-4o");

        let mut m3 = empty_model();
        m3.provider_id = ProviderId::new("openai");
        m3.model_id = ModelId::new("gpt-3.5-turbo");

        let list = vec![m1, m2, m3];
        let filtered: Vec<_> = list
            .into_iter()
            .filter(|m| key.is_model_allowed(m.model_id.as_str(), Some(m.provider_id.as_str())))
            .collect();

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].provider_id.as_str(), "openai");
        assert_eq!(filtered[0].model_id.as_str(), "gpt-4o");
    }

    #[test]
    fn gemini_flash_lite_model_type_is_chat() {
        let mut m = empty_model();
        m.provider_id = ProviderId::new("gemini");
        m.model_id = ModelId::new("gemini-2.0-flash-lite");
        m.model_type = "audio".into(); // Simulate stale/corrupt DB entry
        m.custom = false;

        let v = build_model_entry(&m);
        assert_eq!(
            v.get("type").and_then(|t| t.as_str()),
            Some("chat"),
            "Gemini flash-lite must be categorized as chat even if DB had stale audio"
        );
    }
}
