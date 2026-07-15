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
use openproxy_core::{capabilities, combos, models};

use crate::{
    error::{ApiError, ApiResult},
    state::AppState,
};

/// Default context length to report when neither the DB column nor
/// the heuristic knows the model. 128k is the modern chat default and
/// matches what OpenRouter returns for unknown models.
const DEFAULT_CONTEXT_LENGTH: i64 = 128_000;

/// Default max output tokens when neither the DB nor the heuristic
/// has a value. 8 192 is the conservative Claude / GPT-4-class cap.
const DEFAULT_MAX_OUTPUT_TOKENS: i64 = 8_192;

pub async fn list_models(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    // MEDIUM-4 fix: require a chat-scope API key for /v1/models.
    // This matches OpenAI's behaviour (their /v1/models requires auth)
    // and prevents unauthenticated catalog enumeration.
    //
    // The anonymous fallback (when zero active keys exist) is preserved
    // so first-boot before the bootstrap key is created still works.
    let _api_key_id = match authenticate_chat_or_anonymous(&state, &headers) {
        Ok(id) => id,
        Err(e) => return ApiResult::err(e),
    };

    // Use try_writer_for to avoid blocking under admin lock contention.
    // The model list is bounded (typically <1000 rows) so 5s is plenty.
    crate::api_try! {
        let w = state
            .db_pool()
            .try_writer_for(std::time::Duration::from_secs(5))
            .ok_or_else(|| {
                ApiError(openproxy_core::CoreError::ServiceUnavailable(
                    "database busy; retry in a few seconds".into(),
                ))
            })?;
        let rows = models::list_active_all(&w)?;
        let combo_rows = combos::list_combos(&w)?;

        let mut data: Vec<serde_json::Value> =
            rows.into_iter().map(|m| build_model_entry(&m)).collect();
        for c in &combo_rows {
            // Compute the effective context window: explicit override
            // on the combo row, or auto-compute (min across all
            // targets including sub-combos recursively).
            let effective_cw = if c.context_window.is_some() {
                c.context_window
            } else {
                combos::compute_effective_context_window(&w, c.id).unwrap_or(None)
            };
            data.push(build_combo_entry(c, effective_cw));
        }

        Ok(Json(serde_json::json!({
            "object": "list",
            "data": data,
        })))
    }
}

/// Authenticate with a chat-scope key, OR allow anonymous when zero
/// active keys exist (first-boot window). Returns the key id if
/// authenticated, or None if anonymous.
fn authenticate_chat_or_anonymous(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<openproxy_core::ids::ApiKeyId>, ApiError> {
    use openproxy_core::api_keys;

    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::trim);

    let Some(token) = token else {
        // No Authorization header — allow only if zero active keys.
        // `count_active` is a SELECT COUNT(*) — use the READER so this
        // fallback check doesn't serialize through the writer mutex.
        let r = state.db_pool().reader();
        let active = api_keys::count_active(&r).map_err(ApiError)?;
        if active == 0 {
            return Ok(None); // anonymous
        }
        return Err(ApiError(openproxy_core::CoreError::Auth(
            "missing api key".into(),
        )));
    };

    if token.is_empty() {
        return Err(ApiError(openproxy_core::CoreError::Auth(
            "missing api key".into(),
        )));
    }

    let key_hash = api_keys::hash_key(token);
    // Auth is a SELECT by hash — use the READER so the /v1/models path
    // doesn't serialize through the writer mutex (same fix as the
    // admin and chat auth paths).
    let r = state.db_pool().reader();
    let key = api_keys::get_by_hash(&r, &key_hash).map_err(ApiError)?;
    let key =
        key.ok_or_else(|| ApiError(openproxy_core::CoreError::Auth("invalid api key".into())))?;

    if !key.is_active {
        return Err(ApiError(openproxy_core::CoreError::Auth(
            "api key revoked or inactive".into(),
        )));
    }

    if let Some(exp) = &key.expires_at
        && openproxy_core::api_keys::is_expired(Some(exp), chrono::Utc::now()).map_err(|e| {
            ApiError(openproxy_core::CoreError::Internal(format!(
                "expires_at check: {e}"
            )))
        })?
    {
        return Err(ApiError(openproxy_core::CoreError::Auth(
            "api key expired".into(),
        )));
    }

    if !key.scopes.iter().any(|s| s == "chat") {
        return Err(ApiError(openproxy_core::CoreError::Auth(
            "api key lacks required scope".into(),
        )));
    }

    // Fire-and-forget the `last_used_at` UPDATE on a blocking thread.
    // The /v1/models path no longer blocks on acquiring the writer
    // mutex; `touch_last_used` already throttles itself to 5-minute
    // writes (see `LAST_USED_THROTTLE_SECS` in `api_keys.rs`).
    let pool = std::sync::Arc::clone(state.db_pool());
    let key_id = key.id;
    tokio::task::spawn_blocking(move || {
        let w = pool.writer();
        let _ = api_keys::touch_last_used(&w, key_id);
    });
    Ok(Some(key.id))
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
    serde_json::json!({
        "id": id,
        "object": "model",
        "created": unix_now_secs(),
        "owned_by": "combo",
        "permission": [],
        "root": id,
        "parent": null,
        // The effective context window: either the operator-set
        // override, or the auto-computed minimum across all targets
        // (including sub-combos recursively). `null` when no target
        // has a known context_length.
        "context_length": effective_context_window,
        "max_input_tokens": effective_context_window,
        "max_output_tokens": null,
        "input_modalities": ["text"],
        "output_modalities": ["text"],
        "capabilities": {},
        "type": "chat",
        "family": "combo",
    })
}

/// Project one `core::models::Model` row to the enriched OpenAI-shape
/// JSON object the public endpoint returns. Lifted out of the handler
/// body so it can be unit-tested without spinning up an axum router.
fn build_model_entry(m: &models::Model) -> serde_json::Value {
    let model_id = m.model_id.as_str();
    let provider_id = m.provider_id.as_str();

    // The `id` field is the proxy-level identifier. The convention
    // (mirrored by LiteLLM, OpenRouter, and most proxies that expose
    // a unified /v1/models surface) is `<provider>/<upstream_model_id>`.
    // The leading `<provider>/` prefix disambiguates models that share
    // an upstream id across providers; any further `/` in the
    // upstream id is part of the upstream name and is left intact.
    //
    // Example: provider `openrouter` + upstream `nex-agi/nex-n2-pro:free`
    // → id `openrouter/nex-agi/nex-n2-pro:free`.
    //
    // The chat path strips this prefix before talking to the upstream
    // (see `handlers::chat::run_pipeline`), so the id round-trips
    // safely: the client sends back what it sees in the catalog.
    let full_id = format!("{}/{}", provider_id, model_id);

    // Capabilities: prefer the stored JSON blob; fall back to the
    // heuristic. The fallback runs through the same `from_json`
    // helper for symmetry with the DB path.
    let caps = if let Some(json) = m.capabilities_json.as_deref() {
        capabilities::ModelCapabilities::from_json(Some(json))
    } else {
        capabilities::infer_capabilities(model_id)
    };

    // Context length: DB → heuristic → generic default. The default
    // is the same value OpenRouter itself returns for unknown models,
    // so clients see a sane number even for hand-curated custom rows
    // we have no metadata for.
    let context_length = m
        .context_length
        .or_else(|| capabilities::infer_context_length(model_id))
        .unwrap_or(DEFAULT_CONTEXT_LENGTH);

    // Max output tokens: same DB → heuristic → default chain.
    let max_output_tokens = m
        .max_output_tokens
        .or_else(|| capabilities::infer_max_output_tokens(model_id))
        .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);

    // Input / output modalities: stored JSON, falling back to the
    // heuristic when the column is NULL or unparseable. We deliberately
    // swallow parse errors here — the heuristic is a better answer
    // than a 500.
    let input_modalities: Vec<String> = match m
        .input_modalities_json
        .as_deref()
        .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
    {
        Some(v) => v,
        None => capabilities::infer_input_modalities(&caps)
            .iter()
            .map(|s| s.to_string())
            .collect(),
    };
    let output_modalities: Vec<String> = match m
        .output_modalities_json
        .as_deref()
        .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
    {
        Some(v) => v,
        None => capabilities::infer_output_modalities()
            .iter()
            .map(|s| s.to_string())
            .collect(),
    };

    serde_json::json!({
        // OpenAI-spec fields.
        "id": full_id,
        "object": "model",
        "created": unix_now_secs(),
        "owned_by": provider_id,
        "permission": [],
        // `root` mirrors `id` (no aliasing) so SDKs that read root for
        // a stable handle see the same string. The previous version
        // pointed root at the bare model_id; the new shape keeps root
        // aligned with `id` to avoid surprising tools that compare them.
        "root": full_id,
        "parent": null,
        // OmniRoute-style capability fields.
        "context_length": context_length,
        "max_input_tokens": context_length,
        "max_output_tokens": max_output_tokens,
        "input_modalities": input_modalities,
        "output_modalities": output_modalities,
        // The `capabilities` object: built field-by-field so `null`
        // values are omitted entirely. Using `serde_json::Map` keeps
        // the wire shape clean even for models where only one or two
        // capabilities are known.
        "capabilities": build_capabilities_object(&caps),
        "type": m.model_type,
        "family": m.family,
    })
}

/// Build the inner `capabilities` JSON object, omitting `null` values
/// so the field is `{ "vision": true, "tool_calling": true, ... }`
/// rather than `{ "vision": true, "tool_calling": true, "reasoning":
/// null, ... }`. The omission makes clients that just look for
/// `if (caps.reasoning)` work correctly.
fn build_capabilities_object(caps: &capabilities::ModelCapabilities) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    if let Some(v) = caps.vision {
        out.insert("vision".into(), serde_json::Value::Bool(v));
    }
    if let Some(v) = caps.tool_calling {
        out.insert("tool_calling".into(), serde_json::Value::Bool(v));
    }
    if let Some(v) = caps.reasoning {
        out.insert("reasoning".into(), serde_json::Value::Bool(v));
    }
    if let Some(v) = caps.thinking {
        out.insert("thinking".into(), serde_json::Value::Bool(v));
    }
    if let Some(v) = caps.attachment {
        out.insert("attachment".into(), serde_json::Value::Bool(v));
    }
    if let Some(v) = caps.structured_output {
        out.insert("structured_output".into(), serde_json::Value::Bool(v));
    }
    if let Some(v) = caps.temperature {
        out.insert("temperature".into(), serde_json::Value::Bool(v));
    }
    serde_json::Value::Object(out)
}

fn unix_now_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use openproxy_core::ids::{ModelId, ModelRowId, ProviderId};
    use openproxy_core::models::{Model, TargetFormat};

    fn empty_model() -> Model {
        Model {
            row_id: ModelRowId(1),
            provider_id: ProviderId::new("openrouter"),
            model_id: ModelId::new("openai/gpt-4o"),
            display_name: None,
            target_format: TargetFormat::Openai,
            discovered_at: "2024-01-01 00:00:00".to_string(),
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
            model_type: "chat".to_string(),
            input_modalities_json: None,
            output_modalities_json: None,
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
        m.capabilities_json = Some(r#"{"vision": false}"#.to_string());
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
            assert!(caps.contains_key(key), "missing key {}", key);
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
        m.model_id = openproxy_core::ids::ModelId::new("nex-agi/nex-n2-pro:free");
        let v = build_model_entry(&m);
        let id = v
            .get("id")
            .and_then(|x| x.as_str())
            .expect("id is a string");
        assert_eq!(id, "openrouter/nex-agi/nex-n2-pro:free");
    }
}
