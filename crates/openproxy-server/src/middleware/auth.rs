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
    pub key: Arc<core_api_keys::ApiKey>,
    pub key_id: ApiKeyId,
}

impl ValidatedApiToken {
    pub fn is_combo_allowed(&self, combo_id: i64) -> bool {
        match &self.key.allowed_combos {
            Some(allowed) if !allowed.is_empty() => allowed.contains(&combo_id),
            _ => true,
        }
    }

    pub fn is_provider_allowed(&self, provider_id: &str) -> bool {
        if let Some(blacklisted) = &self.key.blacklisted_providers
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

        if let Some(allowed) = &self.key.allowed_models
            && !allowed.is_empty()
            && !allowed
                .iter()
                .any(|m| matches_any_model_pattern(m, model, bare_model, full_id.as_deref()))
        {
            return false;
        }

        if let Some(blacklisted_provs) = &self.key.blacklisted_providers
            && is_provider_blacklisted(blacklisted_provs, provider_id, prov_from_model)
        {
            return false;
        }

        if let Some(blacklisted) = &self.key.blacklisted_models
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
    if spec
        .strip_suffix('*')
        .is_some_and(|p| candidate.starts_with(p))
    {
        return true;
    }
    let Some(suffix) = spec.strip_prefix('*') else {
        return false;
    };
    candidate.ends_with(suffix)
}

fn matches_any_model_pattern(
    pattern: &str,
    model: &str,
    bare_model: &str,
    full_id: Option<&str>,
) -> bool {
    if pattern_matches(pattern, model) {
        return true;
    }
    if pattern_matches(pattern, bare_model) {
        return true;
    }
    let Some(f) = full_id else {
        return false;
    };
    pattern_matches(pattern, f)
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
    let active = core_api_keys::count_active(&state.db_pool().reader()).map_err(|e| {
        tracing::error!(%e, "db error counting active keys");
        ApiError(CoreError::Auth("missing api key".into()))
    })?;
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
        key,
    }))
}

fn validate_key_record(key: &core_api_keys::ApiKey, required_scope: &str) -> Result<(), ApiError> {
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
) -> Result<Arc<core_api_keys::ApiKey>, ApiError> {
    let key_hash = core_api_keys::hash_key(token);
    let key = if let Some(cached) = state.get_cached_api_key(&key_hash) {
        cached
    } else {
        let r = state.db_pool().reader();
        let fetched = core_api_keys::get_by_hash(&r, &key_hash)
            .map_err(|e| {
                tracing::error!(%e, "db error looking up api key");
                ApiError(CoreError::Auth("invalid api key".into()))
            })?
            .ok_or_else(|| ApiError(CoreError::Auth("invalid api key".into())))?;
        let arc_key = Arc::new(fetched);
        state.cache_api_key(Arc::clone(&arc_key));
        arc_key
    };

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
    let mut last_assistant_tool_calls: Vec<String> = Vec::new();

    messages.retain_mut(|msg| match msg.role.as_str() {
        "assistant" => {
            last_assistant_tool_calls = extract_assistant_tool_call_ids(msg);
            true
        }
        "tool" => {
            let Some(pos) = find_matching_tool_call(msg, &last_assistant_tool_calls) else {
                return false;
            };
            last_assistant_tool_calls.remove(pos);
            true
        }
        _ => {
            last_assistant_tool_calls.clear();
            true
        }
    });

    prune_unfulfilled_tool_calls(messages);
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
    if parsed
        .model
        .as_bytes()
        .windows(8)
        .any(|w| w.eq_ignore_ascii_case(b"deepseek"))
    {
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

pub(crate) fn normalize_responses_content_parts(
    parts: &[serde_json::Value],
) -> Option<serde_json::Value> {
    if parts.is_empty() {
        return None;
    }
    let all_text = parts.iter().all(|p| {
        let t = p.get("type").and_then(|v| v.as_str()).unwrap_or("text");
        matches!(t, "text" | "input_text" | "output_text")
    });
    if all_text {
        let mut combined = String::new();
        for p in parts {
            if let Some(t) = p.get("text").and_then(|s| s.as_str()) {
                combined.push_str(t);
            }
        }
        return Some(serde_json::Value::String(combined));
    }
    let normalized: Vec<serde_json::Value> = parts
        .iter()
        .map(|p| {
            if let serde_json::Value::Object(mut map) = p.clone() {
                map.remove("annotations");
                if let Some(t) = map.get("type").and_then(|v| v.as_str()) {
                    if t == "input_text" || t == "output_text" {
                        map.insert(
                            "type".to_string(),
                            serde_json::Value::String("text".to_string()),
                        );
                    } else if t == "input_image" {
                        map.insert(
                            "type".to_string(),
                            serde_json::Value::String("image_url".to_string()),
                        );
                    }
                }
                serde_json::Value::Object(map)
            } else {
                p.clone()
            }
        })
        .collect();
    Some(serde_json::Value::Array(normalized))
}

pub(crate) fn normalize_responses_tools(
    tools: Option<Vec<serde_json::Value>>,
) -> Option<Vec<serde_json::Value>> {
    let tools = tools?;
    let normalized: Vec<serde_json::Value> = tools
        .into_iter()
        .map(|tool| {
            if let serde_json::Value::Object(mut map) = tool {
                if map.contains_key("function") {
                    return serde_json::Value::Object(map);
                }
                let is_function = map
                    .get("type")
                    .and_then(|v| v.as_str())
                    .is_none_or(|t| t == "function");
                if is_function
                    && (map.contains_key("name")
                        || map.contains_key("parameters")
                        || map.contains_key("description"))
                {
                    let mut fn_obj = serde_json::Map::new();
                    if let Some(name) = map.remove("name") {
                        fn_obj.insert("name".to_string(), name);
                    }
                    if let Some(desc) = map.remove("description") {
                        fn_obj.insert("description".to_string(), desc);
                    }
                    if let Some(params) = map.remove("parameters") {
                        fn_obj.insert("parameters".to_string(), params);
                    }
                    if let Some(strict) = map.remove("strict") {
                        fn_obj.insert("strict".to_string(), strict);
                    }
                    map.insert(
                        "type".to_string(),
                        serde_json::Value::String("function".to_string()),
                    );
                    map.insert("function".to_string(), serde_json::Value::Object(fn_obj));
                }
                serde_json::Value::Object(map)
            } else {
                tool
            }
        })
        .collect();
    Some(normalized)
}

/// Translate a Responses-protocol request body into the internal
/// OpenAIRequest shape the pipeline consumes. Mirrors the logic in
/// `handlers::responses::translate_responses_to_openai` but is
/// invoked from `auth_middleware` so the routing layer can resolve
/// the combo using the synthetic OpenAI payload.
pub(crate) fn translate_responses_to_openai(
    req: &openproxy_types::ResponsesRequest,
) -> openproxy_types::OpenAIRequest {
    use openproxy_types::{OpenAIMessage, ResponsesInputItem};

    let mut messages: Vec<OpenAIMessage> = Vec::with_capacity(req.input.len() + 1);

    let already_has_instructions = req.input.first().is_some_and(|item| match item {
        ResponsesInputItem::Message { role, content }
            if role == "system" || role == "developer" =>
        {
            req.instructions
                .as_deref()
                .is_some_and(|inst| match content {
                    openproxy_types::ResponsesContent::Plain(s) => s.trim() == inst.trim(),
                    openproxy_types::ResponsesContent::Parts(parts) => parts
                        .first()
                        .and_then(|p| p.get("text"))
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|t| t.trim() == inst.trim()),
                })
        }
        _ => false,
    });

    if !already_has_instructions
        && let Some(instructions) = req.instructions.as_deref().filter(|s| !s.is_empty())
    {
        messages.push(OpenAIMessage {
            role: "system".to_string(),
            content: Some(serde_json::Value::String(instructions.to_string())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::new(),
        });
    }

    let mut pending_reasoning: Option<String> = None;
    let flush_reasoning = |msgs: &mut Vec<OpenAIMessage>, r: String| {
        if let Some(last_msg) = msgs
            .last_mut()
            .filter(|m| m.role == "assistant" && !m.extra.contains_key("reasoning_content"))
        {
            last_msg.extra.insert(
                "reasoning_content".to_string(),
                serde_json::Value::String(r),
            );
            return;
        }
        let mut synth_extra = serde_json::Map::new();
        synth_extra.insert(
            "reasoning_content".to_string(),
            serde_json::Value::String(r),
        );
        msgs.push(OpenAIMessage {
            role: "assistant".to_string(),
            content: Some(serde_json::Value::String(String::new())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: synth_extra,
        });
    };

    for item in &req.input {
        match item {
            ResponsesInputItem::Reasoning { .. } => {
                if let Some(r_text) = item.reasoning_text() {
                    if let Some(prev) = pending_reasoning.as_mut() {
                        if !prev.is_empty() && !r_text.is_empty() {
                            prev.push('\n');
                        }
                        prev.push_str(&r_text);
                    } else {
                        pending_reasoning = Some(r_text);
                    }
                }
            }
            ResponsesInputItem::Message { role, content } => {
                let content_value = match content {
                    openproxy_types::ResponsesContent::Plain(s) => {
                        Some(serde_json::Value::String(s.clone()))
                    }
                    openproxy_types::ResponsesContent::Parts(parts) => {
                        normalize_responses_content_parts(parts)
                    }
                };
                let mut extra = serde_json::Map::new();
                if role == "assistant" {
                    if let Some(r) = pending_reasoning.take() {
                        extra.insert(
                            "reasoning_content".to_string(),
                            serde_json::Value::String(r),
                        );
                    }
                } else if let Some(r) = pending_reasoning.take() {
                    flush_reasoning(&mut messages, r);
                }
                messages.push(OpenAIMessage {
                    role: role.clone(),
                    content: content_value,
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                    extra,
                });
            }
            ResponsesInputItem::FunctionCall {
                call_id,
                name,
                arguments,
            } => {
                let tool_call = serde_json::json!({
                    "id": call_id,
                    "type": "function",
                    "function": { "name": name, "arguments": arguments }
                });
                let mut extra = serde_json::Map::new();
                if let Some(r) = pending_reasoning.take() {
                    extra.insert(
                        "reasoning_content".to_string(),
                        serde_json::Value::String(r),
                    );
                }
                messages.push(OpenAIMessage {
                    role: "assistant".to_string(),
                    content: None,
                    name: None,
                    tool_call_id: None,
                    tool_calls: Some(vec![tool_call]),
                    extra,
                });
            }
            ResponsesInputItem::FunctionCallOutput { call_id, output } => {
                if let Some(r) = pending_reasoning.take() {
                    flush_reasoning(&mut messages, r);
                }
                messages.push(OpenAIMessage {
                    role: "tool".to_string(),
                    content: Some(serde_json::Value::String(output.clone())),
                    name: None,
                    tool_call_id: Some(call_id.clone()),
                    tool_calls: None,
                    extra: serde_json::Map::new(),
                });
            }
            ResponsesInputItem::Unknown => {
                tracing::debug!("POST /v1/responses auth_middleware: unknown input item dropped");
            }
        }
    }

    if let Some(r) = pending_reasoning.take() {
        flush_reasoning(&mut messages, r);
    }

    let max_tokens = req.max_output_tokens.or_else(|| {
        req.extra
            .get("max_tokens")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
    });

    let mut extra = req.extra.clone();
    extra.remove("input");
    extra.remove("max_output_tokens");
    if let Some(instructions) = req.instructions.as_deref()
        && !instructions.is_empty()
    {
        extra.insert(
            "instructions".to_string(),
            serde_json::Value::String(instructions.to_string()),
        );
    }

    openproxy_types::OpenAIRequest {
        model: req.model.clone(),
        messages,
        tools: normalize_responses_tools(req.tools.clone()),
        tool_choice: req.tool_choice.clone(),
        user: None,
        extra,
        temperature: req.temperature,
        max_tokens,
        top_p: req.top_p,
        top_k: None,
        stream: req.stream,
        stop: None,
    }
}

async fn read_request_body_capped(
    body: axum::body::Body,
    limit: usize,
) -> Result<bytes::Bytes, Box<axum::response::Response>> {
    match axum::body::to_bytes(body, limit).await {
        Ok(b) => Ok(b),
        Err(e) => {
            let err_str = e.to_string();
            if err_str.contains("length limit exceeded") {
                Err(Box::new(axum::response::IntoResponse::into_response(
                    axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                )))
            } else {
                Err(Box::new(
                    ApiError(openproxy_types::CoreError::Parse(err_str)).into_response(),
                ))
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
    let path = parts.uri.path();
    let is_responses = path == "/v1/responses"
        || path == "/responses"
        || path.starts_with("/v1/responses?")
        || path.starts_with("/responses?")
        || path.ends_with("/responses");

    let auth_result = authenticate(&state, &parts.headers)?;

    let bytes = match read_request_body_capped(body, 32 * 1024 * 1024).await {
        Ok(b) => b,
        Err(resp) => return Ok(*resp),
    };

    let mut parsed: openproxy_types::OpenAIRequest = if is_responses {
        let responses_req: openproxy_types::ResponsesRequest = serde_json::from_slice(&bytes)
            .map_err(|e| {
                crate::error::ApiError(openproxy_types::CoreError::Parse(format!(
                    "Invalid Responses request: {e}"
                )))
            })?;
        if responses_req.model.trim().is_empty() {
            return Err(ApiError(CoreError::Validation("model is required".into())));
        }
        translate_responses_to_openai(&responses_req)
    } else {
        let mut req: openproxy_types::OpenAIRequest =
            serde_json::from_slice(&bytes).map_err(|e| {
                crate::error::ApiError(openproxy_types::CoreError::Parse(e.to_string()))
            })?;

        if req.messages.is_empty()
            && let Some(input_val) = req.extra.remove("input")
            && let Ok(input_items) = serde_json::from_value::<
                Vec<openproxy_types::ResponsesInputItem>,
            >(input_val.clone())
            .or_else(|_| {
                serde_json::from_value::<String>(input_val).map(|s| {
                    vec![openproxy_types::ResponsesInputItem::Message {
                        role: "user".to_string(),
                        content: openproxy_types::ResponsesContent::Plain(s),
                    }]
                })
            })
        {
            let max_output = req
                .extra
                .remove("max_output_tokens")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            let synthetic_req = openproxy_types::ResponsesRequest {
                model: req.model.clone(),
                instructions: None,
                input: input_items,
                tools: req.tools.clone(),
                tool_choice: req.tool_choice.clone(),
                stream: req.stream,
                max_output_tokens: max_output,
                temperature: req.temperature,
                top_p: req.top_p,
                previous_response_id: None,
                extra: req.extra.clone(),
            };
            req = translate_responses_to_openai(&synthetic_req);
        }
        req.tools = normalize_responses_tools(req.tools);
        req
    };

    sanitize_tool_calls(&mut parsed.messages);
    for msg in &mut parsed.messages {
        msg.sanitize_name();
    }
    inject_deepseek_reasoning_if_needed(&mut parsed);

    if parsed.model.is_empty() && parts.uri.path().starts_with("/v1/images") {
        parsed.model = "dall-e-2".to_string();
    }

    let requested_model = &parsed.model;
    if let Some(token) = &auth_result {
        if !token.key.scopes.iter().any(|s| s == "chat") {
            return Err(ApiError(CoreError::Auth(
                "api key lacks required scope".into(),
            )));
        }

        if !token.is_model_allowed(requested_model, None) {
            return Err(ApiError(CoreError::Auth(format!(
                "model '{requested_model}' not allowed or blacklisted for this key"
            ))));
        }

        verify_combo_authorization(&state, Some(token), requested_model)?;
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
