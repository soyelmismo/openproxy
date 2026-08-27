use crate::{error::ApiError, state::AppState};
use axum::{extract::State, http::HeaderMap, response::IntoResponse};
use openproxy_core::api_keys as core_api_keys;
use openproxy_types::{CoreError, ids::ApiKeyId};
use std::sync::Arc;

/// Extracted parsed JSON payload for the chat endpoint.
#[derive(Clone)]
pub struct ParsedChatRequest {
    pub parsed: Arc<openproxy_types::OpenAIRequest>,
    pub bytes: bytes::Bytes,
}

/// Result of a successful chat authentication — the key id plus any
/// per-key restrictions that need to be enforced after routing.
#[derive(Clone, Debug)]
pub struct ValidatedApiToken {
    pub key_id: ApiKeyId,
    pub allowed_models: Option<Vec<String>>,
    pub allowed_combos: Option<Vec<i64>>,
    pub blacklisted_providers: Option<Vec<String>>,
    pub blacklisted_models: Option<Vec<String>>,
}

impl ValidatedApiToken {
    pub fn is_combo_allowed(&self, combo_id: i64) -> bool {
        match &self.allowed_combos {
            Some(allowed) if !allowed.is_empty() => allowed.contains(&combo_id),
            _ => true,
        }
    }

    pub fn is_provider_allowed(&self, provider_id: &str) -> bool {
        if let Some(blacklisted) = &self.blacklisted_providers
            && blacklisted.iter().any(|p| p == provider_id || p == "*")
        {
            return false;
        }
        true
    }

    pub fn is_model_allowed(&self, model: &str, provider_id: Option<&str>) -> bool {
        let (prov_from_model, bare_model) = model
            .split_once('/')
            .map_or((None, model), |(p, rest)| (Some(p), rest));

        let full_id = provider_id.and_then(|p| {
            if model.starts_with(&format!("{p}/")) {
                None
            } else {
                Some(format!("{p}/{model}"))
            }
        });

        if let Some(allowed) = &self.allowed_models
            && !allowed.is_empty()
            && !allowed
                .iter()
                .any(|m| matches_any_model_pattern(m, model, bare_model, full_id.as_deref()))
        {
            return false;
        }

        if let Some(blacklisted_provs) = &self.blacklisted_providers
            && is_provider_blacklisted(blacklisted_provs, provider_id, prov_from_model)
        {
            return false;
        }

        if let Some(blacklisted) = &self.blacklisted_models
            && blacklisted
                .iter()
                .any(|b| matches_any_model_pattern(b, model, bare_model, full_id.as_deref()))
        {
            return false;
        }

        true
    }
}

fn pattern_matches(spec: &str, candidate: &str) -> bool {
    if spec == "*" || spec == candidate {
        return true;
    }
    if let Some(prefix) = spec.strip_suffix('*')
        && candidate.starts_with(prefix)
    {
        return true;
    }
    if let Some(suffix) = spec.strip_prefix('*')
        && candidate.ends_with(suffix)
    {
        return true;
    }
    false
}

fn matches_any_model_pattern(
    pattern: &str,
    model: &str,
    bare_model: &str,
    full_id: Option<&str>,
) -> bool {
    pattern_matches(pattern, model)
        || pattern_matches(pattern, bare_model)
        || full_id.is_some_and(|f| pattern_matches(pattern, f))
}

fn is_provider_blacklisted(
    blacklisted_provs: &[String],
    provider_id: Option<&str>,
    prov_from_model: Option<&str>,
) -> bool {
    if let Some(p) = provider_id
        && blacklisted_provs.iter().any(|bp| bp == p || bp == "*")
    {
        return true;
    }
    if let Some(p) = prov_from_model
        && blacklisted_provs.iter().any(|bp| bp == p || bp == "*")
    {
        return true;
    }
    false
}

fn extract_bearer_or_api_key_token(headers: &HeaderMap) -> Option<&str> {
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
        .filter(|t| !t.is_empty())
}

fn check_anonymous_fallback(state: &AppState) -> Result<Option<ValidatedApiToken>, ApiError> {
    let active = core_api_keys::count_active(&state.db_pool().reader()).map_err(ApiError)?;
    if active == 0 && state.config().server.allow_anonymous {
        tracing::debug!(
            target: "openproxy::auth",
            "anonymous request admitted (no active api keys configured)"
        );
        return Ok(None);
    }
    Err(ApiError(CoreError::Auth("missing api key".into())))
}

/// Resolve the caller from the `Authorization` header.
pub(crate) fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<ValidatedApiToken>, ApiError> {
    let Some(token) = extract_bearer_or_api_key_token(headers) else {
        return check_anonymous_fallback(state);
    };

    let key = verify_key_credentials(state, token, "chat")?;

    Ok(Some(ValidatedApiToken {
        key_id: key.id,
        allowed_models: key.allowed_models,
        allowed_combos: key.allowed_combos,
        blacklisted_providers: key.blacklisted_providers,
        blacklisted_models: key.blacklisted_models,
    }))
}

fn validate_key_record(
    key: &core_api_keys::ApiKey,
    required_scope: &str,
) -> Result<(), ApiError> {
    if !key.is_active {
        return Err(ApiError(CoreError::Auth(
            "api key revoked or inactive".into(),
        )));
    }

    if let Some(exp) = &key.expires_at
        && core_api_keys::is_expired(Some(exp), chrono::Utc::now())
            .map_err(|e| ApiError(CoreError::Internal(format!("expires_at check: {e}"))))?
    {
        return Err(ApiError(CoreError::Auth("api key expired".into())));
    }

    if !key.scopes.iter().any(|s| s == required_scope) {
        return Err(ApiError(CoreError::Auth(
            "api key lacks required scope".into(),
        )));
    }
    Ok(())
}

/// Validate an API key credential against active keys
pub(crate) fn verify_key_credentials(
    state: &AppState,
    token: &str,
    required_scope: &str,
) -> Result<core_api_keys::ApiKey, ApiError> {
    let key_hash = core_api_keys::hash_key(token);
    let r = state.db_pool().reader();
    let key = core_api_keys::get_by_hash(&r, &key_hash)
        .map_err(ApiError)?
        .ok_or_else(|| ApiError(CoreError::Auth("invalid api key".into())))?;

    validate_key_record(&key, required_scope)?;

    let pool = Arc::clone(state.db_pool());
    let key_id = key.id;
    tokio::task::spawn_blocking(move || {
        let w = pool.writer();
        let _ = core_api_keys::touch_last_used(&w, key_id);
    });

    Ok(key)
}

fn verify_combo_authorization(
    state: &AppState,
    auth: Option<&ValidatedApiToken>,
    model_name: &str,
) -> Result<(), ApiError> {
    let Ok(openproxy_core::routing::RoutingPlan::Combo { combo_id, .. }) =
        openproxy_core::routing::resolve(&state.db_pool().reader(), model_name)
    else {
        return Ok(());
    };

    if let Some(auth) = auth
        && !auth.is_combo_allowed(combo_id.0)
    {
        return Err(ApiError(CoreError::Auth(
            "combo not allowed for this key".into(),
        )));
    }
    Ok(())
}

/// Authenticate the request against active API keys and verify model/combo authorization.
pub(crate) fn authenticate_and_authorize_model(
    state: &AppState,
    headers: &HeaderMap,
    model_name: &str,
) -> Result<Option<ApiKeyId>, ApiError> {
    let auth_result = authenticate(state, headers)?;

    if let Some(token) = &auth_result
        && !token.is_model_allowed(model_name, None)
    {
        return Err(ApiError(CoreError::Auth(format!(
            "model '{model_name}' not allowed or blacklisted for this key"
        ))));
    }

    verify_combo_authorization(state, auth_result.as_ref(), model_name)?;
    Ok(auth_result.as_ref().map(|r| r.key_id))
}

const MAX_TOOL_CALLS: usize = 64;
const MAX_ID_LEN: usize = 128;

fn sanitize_tool_calls(messages: &mut Vec<openproxy_types::OpenAIMessage>) {
    *messages = retain_valid_tool_messages(std::mem::take(messages));
    prune_unfulfilled_tool_calls(messages);
}

fn retain_valid_tool_messages(
    messages: Vec<openproxy_types::OpenAIMessage>,
) -> Vec<openproxy_types::OpenAIMessage> {
    let mut valid_messages = Vec::with_capacity(messages.len());
    let mut last_assistant_tool_calls: Vec<String> = Vec::new();

    for mut msg in messages {
        match msg.role.as_str() {
            "assistant" => {
                last_assistant_tool_calls = extract_assistant_tool_call_ids(&mut msg);
                valid_messages.push(msg);
            }
            "tool" => {
                if let Some(pos) = find_matching_tool_call(&msg, &last_assistant_tool_calls) {
                    last_assistant_tool_calls.remove(pos);
                    valid_messages.push(msg);
                }
            }
            _ => {
                last_assistant_tool_calls.clear();
                valid_messages.push(msg);
            }
        }
    }
    valid_messages
}

fn extract_assistant_tool_call_ids(msg: &mut openproxy_types::OpenAIMessage) -> Vec<String> {
    let Some(calls) = &mut msg.tool_calls else {
        return Vec::new();
    };
    calls.truncate(MAX_TOOL_CALLS);
    calls
        .iter()
        .filter_map(|call| call.get("id").and_then(|v| v.as_str()))
        .filter(|id| id.len() <= MAX_ID_LEN)
        .map(ToString::to_string)
        .collect()
}

fn find_matching_tool_call(
    msg: &openproxy_types::OpenAIMessage,
    last_assistant_tool_calls: &[String],
) -> Option<usize> {
    let id = msg.tool_call_id.as_deref()?;
    if id.len() > MAX_ID_LEN {
        return None;
    }
    last_assistant_tool_calls.iter().position(|c| c == id)
}

fn prune_unfulfilled_tool_calls(messages: &mut [openproxy_types::OpenAIMessage]) {
    let mut remainder = messages;
    while let Some((msg, tail)) = remainder.split_first_mut() {
        remainder = tail;
        if msg.role == "assistant"
            && let Some(calls) = &mut msg.tool_calls
        {
            calls.retain(|call| is_tool_call_fulfilled(call, remainder));
            if calls.is_empty() {
                msg.tool_calls = None;
            }
        }
    }
}

fn is_tool_call_fulfilled(
    call: &serde_json::Value,
    following_messages: &[openproxy_types::OpenAIMessage],
) -> bool {
    let Some(id) = call.get("id").and_then(|v| v.as_str()) else {
        return false;
    };
    id.len() <= MAX_ID_LEN
        && following_messages
            .iter()
            .take_while(|m| m.role == "tool")
            .take(MAX_TOOL_CALLS)
            .any(|m| m.tool_call_id.as_deref() == Some(id))
}

fn inject_deepseek_reasoning_if_needed(parsed: &mut openproxy_types::OpenAIRequest) {
    if parsed.model.to_lowercase().contains("deepseek") {
        for msg in &mut parsed.messages {
            if msg.role == "assistant" && !msg.extra.contains_key("reasoning_content") {
                msg.extra.insert(
                    "reasoning_content".to_string(),
                    serde_json::Value::String(String::new()),
                );
            }
        }
    }
}

async fn read_request_body_capped(
    body: axum::body::Body,
    limit: usize,
) -> Result<bytes::Bytes, axum::response::Response> {
    match axum::body::to_bytes(body, limit).await {
        Ok(b) => Ok(b),
        Err(e) => {
            let err_str = e.to_string();
            if err_str.contains("length limit exceeded") {
                Err(axum::response::IntoResponse::into_response(
                    axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                ))
            } else {
                Err(ApiError(openproxy_types::CoreError::Parse(err_str)).into_response())
            }
        }
    }
}

pub async fn auth_middleware(
    State(state): State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, crate::error::ApiError> {
    let (mut parts, body) = req.into_parts();
    let auth_result = authenticate(&state, &parts.headers)?;

    let bytes = match read_request_body_capped(body, 32 * 1024 * 1024).await {
        Ok(b) => b,
        Err(resp) => return Ok(resp),
    };

    let mut parsed: openproxy_types::OpenAIRequest = serde_json::from_slice(&bytes)
        .map_err(|e| crate::error::ApiError(openproxy_types::CoreError::Parse(e.to_string())))?;

    sanitize_tool_calls(&mut parsed.messages);
    inject_deepseek_reasoning_if_needed(&mut parsed);

    if parsed.model.is_empty() && parts.uri.path().starts_with("/v1/images") {
        parsed.model = "dall-e-2".to_string();
    }

    let requested_model = &parsed.model;
    if let Some(token) = &auth_result
        && !token.is_model_allowed(requested_model, None)
    {
        return Err(ApiError(CoreError::Auth(format!(
            "model '{requested_model}' not allowed or blacklisted for this key"
        ))));
    }

    parts.extensions.insert(ParsedChatRequest {
        parsed: Arc::new(parsed),
        bytes: bytes::Bytes::clone(&bytes),
    });
    if let Some(res) = auth_result {
        parts.extensions.insert(res);
    }

    let req = axum::extract::Request::from_parts(parts, axum::body::Body::from(bytes));
    Ok(next.run(req).await)
}
