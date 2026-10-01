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
    let maybe_api_key = authenticate_chat_or_anonymous(&state, &headers).await?;

    crate::error::run_blocking(move || {
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
            let effective_caps = state
                .services()
                .combos
                .compute_effective_capabilities(c.id)
                .ok()
                .flatten();
            data.push(build_combo_entry(
                c,
                None,
                effective_cw,
                effective_caps.as_ref(),
            ));
            data.push(build_combo_entry(
                c,
                Some(&c.name),
                effective_cw,
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

/// Authenticate with a chat-scope key, or allow anonymous when zero active keys
/// exist (first-boot window) AND the operator opted in via
/// `server.allow_anonymous` — the same gate the chat routes apply in
/// `middleware::auth::check_anonymous_fallback`. Without the opt-in the catalog
/// stays private during the first-boot window and after the last key is revoked
/// (e.g. mid-rotation). Returns the key, or `None` when anonymous.
async fn authenticate_chat_or_anonymous(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<std::sync::Arc<openproxy_core::api_keys::ApiKey>>, ApiError> {
    let token = extract_auth_header_token(headers);

    let Some(token) = token else {
        let active = state
            .db_pool()
            .spawn_read(openproxy_core::api_keys::count_active)
            .await?;
        if active == 0 && state.config().server.allow_anonymous {
            return Ok(None);
        }
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
fn build_combo_entry(
    c: &openproxy_types::Combo,
    id_override: Option<&str>,
    effective_context_window: Option<i64>,
    effective_caps: Option<&capabilities::ModelCapabilities>,
) -> serde_json::Value {
    let id = id_override.map_or_else(
        || format!("combo:{}", c.name),
        std::string::ToString::to_string,
    );
    let combo_type = capabilities::infer_model_type(&c.name);
    let empty_caps = capabilities::ModelCapabilities::empty();
    let caps_ref = effective_caps.unwrap_or(&empty_caps);

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
    let fields: [(&str, Option<bool>); 8] = [
        ("vision", caps.vision),
        ("tool_calling", caps.tool_calling),
        ("reasoning", caps.reasoning),
        ("thinking", caps.thinking),
        ("attachment", caps.attachment),
        ("structured_output", caps.structured_output),
        ("temperature", caps.temperature),
        ("decisions", caps.decisions),
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
            // All metadata empty: exercises the heuristic fallback.
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
        // DB value wins, and an explicit `false` is present rather than omitted.
        let caps = v.get("capabilities").unwrap();
        assert_eq!(caps.get("vision"), Some(&serde_json::Value::Bool(false)));
        assert_eq!(v.get("context_length").unwrap().as_i64(), Some(999_999));
    }

    #[test]
    fn capabilities_object_omits_nulls() {
        let m = empty_model();
        let v = build_model_entry(&m);
        let caps = v.get("capabilities").and_then(|c| c.as_object()).unwrap();
        // A heuristic-inferred gpt-4o row yields every inferable capability;
        // `reasoning`/`thinking` match no keyword and must be omitted — the
        // exact omit-on-null contract under test.
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
        // `created` is a non-zero unix timestamp; `object`/`owned_by` round-trip.
        assert!(v.get("created").unwrap().as_i64().unwrap() > 0);
        assert_eq!(v.get("object").unwrap().as_str(), Some("model"));
        assert_eq!(v.get("owned_by").unwrap().as_str(), Some("openrouter"));
    }

    #[test]
    fn id_is_provider_prefixed() {
        // The provider prefix keeps chat-endpoint round-trips unambiguous; exact
        // shape `<provider>/<upstream_id>` is pinned here.
        let m = empty_model();
        let v = build_model_entry(&m);
        let id = v
            .get("id")
            .and_then(|x| x.as_str())
            .expect("id is a string");
        // empty_model() = provider "openrouter" + upstream "openai/gpt-4o".
        assert_eq!(
            id, "openrouter/openai/gpt-4o",
            "id must be provider-prefixed"
        );
        // `root` mirrors `id` for SDKs that compare them.
        let root = v
            .get("root")
            .and_then(|x| x.as_str())
            .expect("root is a string");
        assert_eq!(root, id, "root mirrors id");
    }

    #[test]
    fn id_handles_already_prefixed_upstream_id() {
        // An upstream id that already contains `/` (e.g. OpenRouter's
        // `nex-agi/nex-n2-pro:free`) yields two slashes. Expected: only the first
        // `/` separates provider from upstream; later ones belong to the model name.
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

    #[test]
    fn test_build_capabilities_object_includes_decisions() {
        let caps = capabilities::ModelCapabilities {
            decisions: Some(true),
            vision: Some(true),
            ..Default::default()
        };

        let obj = build_capabilities_object(&caps);
        assert_eq!(obj.get("decisions"), Some(&serde_json::Value::Bool(true)));
        assert_eq!(obj.get("vision"), Some(&serde_json::Value::Bool(true)));
        assert_eq!(obj.get("thinking"), None);
    }

    #[test]
    fn test_reasoning_and_supported_parameters_populated() {
        let mut m = empty_model();
        m.capabilities_json = Some(r#"{"reasoning": true, "tool_calling": true}"#.into());
        let v = build_model_entry(&m);

        assert_eq!(
            v.get("supports_reasoning"),
            Some(&serde_json::Value::Bool(true))
        );
        let params: Vec<&str> = v
            .get("supported_parameters")
            .and_then(|p| p.as_array())
            .unwrap()
            .iter()
            .filter_map(|s| s.as_str())
            .collect();
        assert!(params.contains(&"reasoning_effort"));
        assert!(params.contains(&"thinking"));
        assert!(params.contains(&"tools"));
    }

    #[test]
    fn test_combo_entry_reasoning_and_supported_parameters() {
        let combo = openproxy_types::Combo {
            id: openproxy_types::ComboId(1),
            name: "test-ninja".into(),
            strategy: openproxy_types::combos::Strategy::Priority,
            race_size: 1,
            preventive_rate_limit: false,
            created_at: "2024-01-01".into(),
            context_window: None,
            priority_mode: Default::default(),
            cooldown_mode: Default::default(),
            cooldown_base_secs: None,
            cooldown_max_secs: None,
            cooldown_factor: None,
            lkgp_exploration_rate: None,
            selection_window_secs: None,
            decision_model: None,
            decision_timeout_ms: None,
        };
        let mut caps = capabilities::ModelCapabilities::empty();
        caps.thinking = Some(true);
        caps.tool_calling = Some(true);

        let v = build_combo_entry(&combo, Some("test-ninja"), Some(128_000), Some(&caps));
        assert_eq!(
            v.get("id"),
            Some(&serde_json::Value::String("test-ninja".into()))
        );
        assert_eq!(
            v.get("supports_reasoning"),
            Some(&serde_json::Value::Bool(true))
        );
        let params: Vec<&str> = v
            .get("supported_parameters")
            .and_then(|p| p.as_array())
            .unwrap()
            .iter()
            .filter_map(|s| s.as_str())
            .collect();
        assert!(params.contains(&"reasoning_effort"));
        assert!(params.contains(&"thinking"));
        assert!(params.contains(&"tools"));
        let caps_obj = v.get("capabilities").and_then(|c| c.as_object()).unwrap();
        assert_eq!(
            caps_obj.get("thinking"),
            Some(&serde_json::Value::Bool(true))
        );
    }
}
