use serde_json::{json, Value};
use openproxy_types::{OpenAIMessage, OpenAIRequest};
use crate::translation::types::*;
use crate::translation::helpers::*;

pub fn openai_to_anthropic(
    req: &OpenAIRequest,
    override_model: &str,
    override_messages: &[OpenAIMessage],
    override_stream: bool,
) -> AnthropicRequest {
    let mut system_parts: Vec<String> = Vec::new();
    let mut conversation: Vec<AnthropicMessage> = Vec::with_capacity(override_messages.len());

    // Anthropic requires strictly alternating user/assistant messages.
    // OpenAI's format allows consecutive same-role messages (e.g. a
    // client that sends multiple assistant chunks as separate messages,
    // or multiple tool results). We accumulate both:
    //
    // - `pending_tool_results`: consecutive tool results → merged into
    //   a single user message with [tool_result...] content blocks.
    // - `pending_assistant_text`: consecutive plain-text assistant
    //   messages → merged into a single assistant message by joining
    //   the text with newlines.
    //
    // When a role transition happens, we flush the pending buffer for
    // the previous role before emitting the new role's message. Special
    // case: a user message following tool_results merges them into the
    // SAME user message (tool_result blocks + text block) rather than
    // emitting two consecutive user messages.
    let mut pending_tool_results: Vec<serde_json::Value> = Vec::new();
    let mut pending_assistant_text: Vec<String> = Vec::new();

    // Helper: flush pending assistant text as a single assistant message.
    // Called when a non-assistant message arrives.
    let flush_assistant = |conv: &mut Vec<AnthropicMessage>, pending: &mut Vec<String>| {
        if !pending.is_empty() {
            let text = pending.join("\n\n");
            conv.push(AnthropicMessage {
                role: "assistant".to_string(),
                content: serde_json::Value::String(text),
            });
            pending.clear();
        }
    };

    // Helper: flush pending tool results as a single user message.
    // Called when a non-tool, non-user message arrives.
    let flush_tool_results = |conv: &mut Vec<AnthropicMessage>,
                              pending: &mut Vec<serde_json::Value>| {
        if !pending.is_empty() {
            conv.push(AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::Value::Array(std::mem::take(pending)),
            });
        }
    };

    for m in override_messages {
        let role = m.role.as_str();

        // On role transitions, flush pending buffers for the previous
        // role. Order matters: flush assistant text first, then tool
        // results. We do NOT flush tool results when the incoming
        // message is a user message — the user arm below merges them.
        if role != "assistant" {
            flush_assistant(&mut conversation, &mut pending_assistant_text);
        }
        if role != "tool" && role != "user" {
            flush_tool_results(&mut conversation, &mut pending_tool_results);
        }

        match role {
            "system" => system_parts.push(message_content_to_text(&m.content)),
            "assistant" => {
                if let Some(tool_calls) = m.tool_calls.as_ref() {
                    // Assistant message with tool_calls. First flush
                    // any pending assistant text (from previous
                    // consecutive plain-text assistant messages) into
                    // this same assistant message's content blocks.
                    let mut blocks: Vec<serde_json::Value> = Vec::new();
                    if !pending_assistant_text.is_empty() {
                        let text = pending_assistant_text.join("\n\n");
                        blocks.push(json!({"type": "text", "text": text}));
                        pending_assistant_text.clear();
                    }
                    let text = message_content_to_text(&m.content);
                    if !text.is_empty() {
                        blocks.push(json!({"type": "text", "text": text}));
                    }
                    for tc in tool_calls {
                        let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                        let function = tc.get("function");
                        let name = function
                            .and_then(|f| f.get("name"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let arguments_str = function
                            .and_then(|f| f.get("arguments"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let input: serde_json::Value = if arguments_str.is_empty() {
                            json!({})
                        } else {
                            serde_json::from_str(arguments_str).unwrap_or(json!({}))
                        };
                        if name.is_empty() {
                            continue;
                        }
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": id,
                            "name": name,
                            "input": input,
                        }));
                    }
                    if blocks.is_empty() {
                        blocks.push(json!({"type": "text", "text": ""}));
                    }
                    conversation.push(AnthropicMessage {
                        role: "assistant".to_string(),
                        content: serde_json::Value::Array(blocks),
                    });
                } else {
                    // Plain text assistant message. Accumulate into
                    // pending_assistant_text instead of emitting
                    // immediately — consecutive assistant text messages
                    // will be merged into a single assistant message.
                    let text = message_content_to_text(&m.content);
                    // Skip empty / system-injected markers.
                    if text.is_empty()
                        || text.starts_with("Operation interrupted")
                        || text.starts_with("[System:")
                    {
                        continue;
                    }
                    pending_assistant_text.push(text);
                }
            }
            "user" => {
                // If there are pending tool results, merge them into
                // the SAME user message as the text (content =
                // [tool_result..., text]). Otherwise emit the text as
                // a plain string user message.
                let text = message_content_to_text(&m.content);
                if !pending_tool_results.is_empty() {
                    pending_tool_results.push(json!({"type": "text", "text": text}));
                    conversation.push(AnthropicMessage {
                        role: "user".to_string(),
                        content: serde_json::Value::Array(std::mem::take(
                            &mut pending_tool_results,
                        )),
                    });
                } else {
                    conversation.push(AnthropicMessage {
                        role: "user".to_string(),
                        content: serde_json::Value::String(text),
                    });
                }
            }
            "tool" => {
                let tool_use_id = m.tool_call_id.as_deref().unwrap_or("");
                let content_text = message_content_to_text(&m.content);
                pending_tool_results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "content": content_text,
                }));
            }
            _ => {}
        }
    }

    // Flush any remaining pending buffers.
    flush_assistant(&mut conversation, &mut pending_assistant_text);
    flush_tool_results(&mut conversation, &mut pending_tool_results);

    // Diagnostic logging to catch structure issues.
    log_anthropic_translation_diagnostics(&conversation);

    let system = if system_parts.is_empty() {
        None
    } else {
        Some(serde_json::Value::String(system_parts.join("\n\n")))
    };

    AnthropicRequest {
        model: override_model.to_string(),
        messages: conversation,
        max_tokens: req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        system,
        temperature: req.temperature,
        top_p: req.top_p,
        top_k: req.top_k,
        stop_sequences: req.stop.clone(),
        // Translate OpenAI-shaped `tools` to Anthropic shape. MiniMax
        // (which exposes an Anthropic-compatible API) and real Anthropic
        // both expect `{name, description, input_schema}` — forwarding
        // the OpenAI shape `{type:"function", function:{name, parameters}}`
        // verbatim causes `(2013) function name or parameters is empty`
        // because the upstream looks for `name`/`input_schema` at the
        // top level and finds them missing.
        tools: req
            .tools
            .as_ref()
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(translate_openai_tool_to_anthropic)
                    .collect::<Vec<_>>()
            })
            .filter(|t: &Vec<serde_json::Value>| !t.is_empty()),
        // Translate OpenAI `tool_choice` to Anthropic shape.
        tool_choice: req
            .tool_choice
            .as_ref()
            .and_then(translate_openai_tool_choice_to_anthropic),
        // OpenAI's `user` field maps to Anthropic's `metadata.user_id`
        // (Anthropic reserves metadata for traceability, not for
        // function-calling). When the caller didn't set `user`, we
        // leave metadata None rather than synthesise an empty object.
        metadata: req
            .user
            .as_ref()
            .map(|u| serde_json::json!({ "user_id": u })),
        stream: override_stream,
        extra: Default::default(),
    }
}

/// Translate a single OpenAI-shaped tool definition to Anthropic shape.
///
/// OpenAI: `{"type":"function","function":{"name":"X","description":"Y","parameters":{...}}}`
/// Anthropic: `{"name":"X","description":"Y","input_schema":{...}}`
///
/// Returns `None` when the tool has no `name` or no `function` block —
/// MiniMax rejects tools with empty names with `(2013)`.
fn translate_openai_tool_to_anthropic(tool: &serde_json::Value) -> Option<serde_json::Value> {
    let function = tool.get("function")?;
    let name = function.get("name").and_then(|v| v.as_str())?;
    if name.is_empty() {
        return None;
    }
    let description = function.get("description").and_then(|v| v.as_str());
    // `parameters` (OpenAI) → `input_schema` (Anthropic). Default to
    // an empty object when absent — Anthropic requires `input_schema`
    // to be present and a valid JSON schema object.
    let input_schema = function.get("parameters").cloned().unwrap_or(json!({}));
    Some(json!({
        "name": name,
        "description": description,
        "input_schema": input_schema,
    }))
}

/// Translate OpenAI `tool_choice` to Anthropic `tool_choice`.
///
/// OpenAI shapes:
///   - `"auto"` / `"none"` / `"required"` (string)
///   - `{"type":"function","function":{"name":"X"}}` (object)
///   - `{"type":"auto"}` / `{"type":"none"}` (object form of the strings)
///
/// Anthropic shapes:
///   - `{"type":"auto"}` (let model decide)
///   - `{"type":"none"}` (don't use tools)
///   - `{"type":"any"}` (force a tool call — OpenAI's "required")
///   - `{"type":"tool","name":"X"}` (force a specific tool)
///
/// Returns `None` for unrecognized shapes (which means the field is
/// omitted from the Anthropic request, defaulting to `auto` upstream).
fn translate_openai_tool_choice_to_anthropic(tc: &serde_json::Value) -> Option<serde_json::Value> {
    match tc {
        serde_json::Value::String(s) => match s.as_str() {
            "auto" => Some(json!({"type": "auto"})),
            "none" => Some(json!({"type": "none"})),
            "required" => Some(json!({"type": "any"})),
            _ => None,
        },
        serde_json::Value::Object(obj) => {
            let ty = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match ty {
                "auto" => Some(json!({"type": "auto"})),
                "none" => Some(json!({"type": "none"})),
                "required" => Some(json!({"type": "any"})),
                "function" => {
                    // OpenAI object form: {"type":"function","function":{"name":"X"}}
                    // → Anthropic: {"type":"tool","name":"X"}
                    let name = obj
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|v| v.as_str())?;
                    if name.is_empty() {
                        return None;
                    }
                    Some(json!({"type": "tool", "name": name}))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Convert Anthropic response to OpenAI response.
///
/// - `choices[0].message.content` = concatenation of all text content blocks.
/// - `usage`: `prompt_tokens=input_tokens`, `completion_tokens=output_tokens`,
///   `total_tokens=sum`.
/// - `finish_reason` mapped from `stop_reason` using Anthropic -> OpenAI semantics.
pub fn anthropic_to_openai(resp: &AnthropicResponse) -> OpenAIResponse {
    let combined: String = resp
        .content
        .iter()
        .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join("");

    let prompt_tokens = resp.usage.input_tokens;
    let completion_tokens = resp.usage.output_tokens;
    let total_tokens = prompt_tokens.saturating_add(completion_tokens);

    let message = OpenAIMessage {
        role: "assistant".to_string(),
        content: Some(Value::String(combined)),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        extra: serde_json::Map::new(),
    };

    let choice = OpenAIChoice {
        index: 0,
        message,
        finish_reason: resp.stop_reason.as_deref().map(map_finish_reason),
    };

    OpenAIResponse {
        id: resp.id.clone(),
        object: "chat.completion".to_string(),
        created: 0,
        model: resp.model.clone(),
        choices: vec![choice],
        usage: Some(OpenAIUsage {
            prompt_tokens,
            completion_tokens,
            total_tokens,
        }),
    }
}

/// Map an Anthropic stop_reason value to an OpenAI finish_reason value.
pub fn map_finish_reason(stop_reason: &str) -> String {
    match stop_reason {
        "end_turn" => "stop".to_string(),
        "max_tokens" => "length".to_string(),
        "tool_use" => "tool_calls".to_string(),
        // stop_sequence and unknown values fall back to "stop".
        other => {
            // Treat anything unknown as "stop" to stay close to OpenAI's vocabulary.
            let _ = other;
            "stop".to_string()
        }
    }
}


pub fn anthropic_request_to_openai(req: AnthropicRequest) -> OpenAIRequest {
    let mut messages = Vec::with_capacity(req.messages.len() + req.system.is_some() as usize);
    if let Some(sys) = req.system {
        let sys_str = if let Some(s) = sys.as_str() {
            s.to_string()
        } else if let Some(arr) = sys.as_array() {
            arr.iter()
                .filter_map(|v| v.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            sys.to_string()
        };
        messages.push(OpenAIMessage {
            role: "system".to_string(),
            content: Some(serde_json::Value::String(sys_str)),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        });
    }
    for m in req.messages {
        let mut text_blocks = Vec::new();
        let mut tool_calls = Vec::new();
        let mut tool_results = Vec::new();

        if let Some(arr) = m.content.as_array() {
            for block in arr {
                if let Some(typ) = block.get("type").and_then(|v| v.as_str()) {
                    match typ {
                        "text" => {
                            if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                                text_blocks.push(t.to_string());
                            }
                        }
                        "tool_use" => {
                            if let (Some(id), Some(name), Some(input)) = (
                                block.get("id").and_then(|v| v.as_str()),
                                block.get("name").and_then(|v| v.as_str()),
                                block.get("input"),
                            ) {
                                tool_calls.push(serde_json::json!({
                                    "id": id,
                                    "type": "function",
                                    "function": {
                                        "name": name,
                                        "arguments": serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string())
                                    }
                                }));
                            }
                        }
                        "tool_result" => {
                            if let Some(id) = block.get("tool_use_id").and_then(|v| v.as_str()) {
                                let res_content = block.get("content").unwrap_or(&serde_json::Value::Null);
                                tool_results.push((id.to_string(), res_content.clone()));
                            }
                        }
                        _ => {}
                    }
                }
            }
        } else if let Some(s) = m.content.as_str() {
            text_blocks.push(s.to_string());
        }

        if m.role == "assistant" {
            let tc = if tool_calls.is_empty() { None } else { Some(tool_calls) };
            let content = if text_blocks.is_empty() && tc.is_some() {
                // If there are only tool calls, OpenAI allows content to be null/empty string.
                Some(serde_json::Value::Null)
            } else {
                Some(serde_json::Value::String(text_blocks.join("\n\n")))
            };
            messages.push(OpenAIMessage {
                role: m.role.clone(),
                content,
                name: None,
                tool_call_id: None,
                tool_calls: tc,
                extra: Default::default(),
            });
        } else if m.role == "user" {
            // Anthropic allows tool_result in user messages.
            for (id, content) in tool_results {
                let text_res = if let Some(s) = content.as_str() {
                    s.to_string()
                } else if let Some(arr) = content.as_array() {
                    arr.iter()
                        .filter_map(|v| v.get("text").and_then(|t| t.as_str()))
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    content.to_string()
                };
                messages.push(OpenAIMessage {
                    role: "tool".to_string(),
                    content: Some(serde_json::Value::String(text_res)),
                    name: None,
                    tool_call_id: Some(id),
                    tool_calls: None,
                    extra: Default::default(),
                });
            }
            if !text_blocks.is_empty() {
                messages.push(OpenAIMessage {
                    role: m.role.clone(),
                    content: Some(serde_json::Value::String(text_blocks.join("\n\n"))),
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                    extra: Default::default(),
                });
            }
        } else {
            // Fallback for any other role
            messages.push(OpenAIMessage {
                role: m.role,
                content: Some(m.content),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: Default::default(),
            });
        }
    }

    let tools = req.tools.map(translate_anthropic_tools_to_openai);
    let tool_choice = req.tool_choice.map(translate_anthropic_tool_choice_to_openai);

    let mut extra = req.metadata.map(|m| {
        let mut map = serde_json::Map::new();
        map.insert("metadata".to_string(), m);
        map
    }).unwrap_or_default();

    if let Some(output_config) = req.extra.get("output_config") {
        if let Some(format) = output_config.get("format") {
            if format.get("type").and_then(|v| v.as_str()) == Some("json_schema") {
                if let Some(schema) = format.get("schema") {
                    let response_format = serde_json::json!({
                        "type": "json_schema",
                        "json_schema": {
                            "name": "json_response",
                            "strict": true,
                            "schema": schema
                        }
                    });
                    extra.insert("response_format".to_string(), response_format);
                }
            }
        }
    }

    OpenAIRequest {
        model: req.model,
        messages,
        stream: req.stream,
        temperature: req.temperature,
        max_tokens: Some(req.max_tokens),
        top_p: req.top_p,
        stop: req.stop_sequences,
        tools,
        tool_choice,
        top_k: req.top_k,
        user: None,
        extra,
    }
}

pub fn openai_response_to_anthropic(resp: OpenAIResponse) -> AnthropicResponse {
    let mut content = Vec::new();
    let mut finish_reason = None;
    if let Some(first_choice) = resp.choices.first() {
        if let Some(msg_content) = first_choice.message.content.as_ref() {
            if let Some(s) = msg_content.as_str() {
                if !s.is_empty() {
                    content.push(serde_json::json!({
                        "type": "text",
                        "text": s.to_string()
                    }));
                }
            }
        }
        
        if let Some(tool_calls) = &first_choice.message.tool_calls {
            for tc in tool_calls {
                if let (Some(id), Some(function)) = (tc.get("id"), tc.get("function")) {
                    let name = function.get("name").and_then(|n| n.as_str()).unwrap_or_default();
                    let arguments_str = function.get("arguments").and_then(|a| a.as_str()).unwrap_or("{}");
                    let input = serde_json::from_str::<serde_json::Value>(arguments_str).unwrap_or_else(|_| serde_json::json!({}));
                    content.push(serde_json::json!({
                        "type": "tool_use",
                        "id": id,
                        "name": name,
                        "input": input
                    }));
                }
            }
        }
        
        finish_reason = first_choice.finish_reason.clone();
    }
    
    let anthropic_stop = match finish_reason.as_deref() {
        Some("length") => Some("max_tokens".to_string()),
        Some("tool_calls") | Some("function_call") => Some("tool_use".to_string()),
        Some("content_filter") => Some("stop_sequence".to_string()),
        Some(_) => Some("end_turn".to_string()),
        None => None,
    };
    
    let usage = resp.usage.unwrap_or(OpenAIUsage {
        prompt_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
    });

    AnthropicResponse {
        id: resp.id,
        response_type: "message".to_string(),
        role: "assistant".to_string(),
        content,
        model: resp.model,
        stop_reason: anthropic_stop,
        usage: AnthropicUsage {
            input_tokens: usage.prompt_tokens,
            output_tokens: usage.completion_tokens,
        },
    }
}

/// Diagnostic logging: log the translated message structure so we
/// can debug MiniMax 2013 errors. We log:
/// - the role sequence (to catch consecutive same-role bugs)
/// - tool_use IDs declared by assistant messages
/// - tool_result IDs provided by user messages
/// - whether every tool_use has a matching tool_result
/// This is traced at DEBUG level so it doesn't spam production logs
/// unless RUST_LOG=debug is set.
fn log_anthropic_translation_diagnostics(conversation: &[AnthropicMessage]) {
    if !tracing::enabled!(tracing::Level::DEBUG) {
        return;
    }
    let role_seq: Vec<&str> = conversation.iter().map(|m| m.role.as_str()).collect();
    let mut tool_use_ids: Vec<String> = Vec::new();
    let mut tool_result_ids: Vec<String> = Vec::new();
    for m in conversation {
        if m.role == "assistant" {
            if let Some(arr) = m.content.as_array() {
                for block in arr {
                    if block.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                        && let Some(id) = block.get("id").and_then(|v| v.as_str())
                    {
                        tool_use_ids.push(id.to_string());
                    }
                }
            }
        } else if m.role == "user"
            && let Some(arr) = m.content.as_array()
        {
            for block in arr {
                if block.get("type").and_then(|t| t.as_str()) == Some("tool_result")
                    && let Some(id) = block.get("tool_use_id").and_then(|v| v.as_str())
                {
                    tool_result_ids.push(id.to_string());
                }
            }
        }
    }
    let use_set: std::collections::HashSet<&str> =
        tool_use_ids.iter().map(|s| s.as_str()).collect();
    let result_set: std::collections::HashSet<&str> =
        tool_result_ids.iter().map(|s| s.as_str()).collect();
    let missing_results: Vec<&str> = use_set.difference(&result_set).copied().collect();
    let orphan_results: Vec<&str> = result_set.difference(&use_set).copied().collect();
    tracing::debug!(
        role_sequence = ?role_seq,
        tool_use_count = tool_use_ids.len(),
        tool_result_count = tool_result_ids.len(),
        tool_use_ids = ?tool_use_ids,
        tool_result_ids = ?tool_result_ids,
        missing_results = ?missing_results,
        orphan_results = ?orphan_results,
        "openai_to_anthropic translation result"
    );
    // Also warn if there's a structural problem so it shows up
    // even at default log level.
    if !missing_results.is_empty() || !orphan_results.is_empty() {
        tracing::warn!(
            missing_results = ?missing_results,
            orphan_results = ?orphan_results,
            "translation: tool_use/tool_result ID mismatch — MiniMax will reject with (2013)"
        );
    }
    // Warn on consecutive same-role messages.
    for i in 1..conversation.len() {
        if conversation[i].role == conversation[i - 1].role {
            tracing::warn!(
                idx = i,
                role = %conversation[i].role,
                "translation: consecutive same-role messages — Anthropic/MiniMax rejects this with (2013)"
            );
        }
    }
}

fn translate_anthropic_tools_to_openai(ts: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    ts.into_iter().map(|t| {
        if let Some(obj) = t.as_object() {
            let mut f = serde_json::Map::new();
            if let Some(n) = obj.get("name") { f.insert("name".to_string(), n.clone()); }
            if let Some(d) = obj.get("description") { f.insert("description".to_string(), d.clone()); }
            if let Some(s) = obj.get("input_schema") { f.insert("parameters".to_string(), s.clone()); }
            serde_json::json!({
                "type": "function",
                "function": f
            })
        } else {
            t
        }
    }).collect()
}

fn translate_anthropic_tool_choice_to_openai(tc: serde_json::Value) -> serde_json::Value {
    if let Some(obj) = tc.as_object() {
        if obj.get("type").and_then(|v| v.as_str()) == Some("tool") {
            if let Some(name) = obj.get("name") {
                return serde_json::json!({
                    "type": "function",
                    "function": { "name": name }
                });
            }
        }
    }
    tc
}
