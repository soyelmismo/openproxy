use crate::error::ApiError;
use axum::response::IntoResponse;

const MAX_TOOL_CALLS: usize = 64;
const MAX_ID_LEN: usize = 128;

pub(crate) fn sanitize_tool_calls(messages: &mut Vec<openproxy_types::OpenAIMessage>) {
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

pub(crate) fn inject_deepseek_reasoning_if_needed(parsed: &mut openproxy_types::OpenAIRequest) {
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

pub(crate) async fn read_request_body_capped(
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
