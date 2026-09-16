use super::diagnostics::log_anthropic_translation_diagnostics;
use super::identity::normalize_claude_client_identity;
use crate::translation::types::{AnthropicMessage, AnthropicRequest, DEFAULT_MAX_TOKENS};
use openproxy_types::{OpenAIMessage, OpenAIRequest};
use serde_json::json;

pub fn openai_to_anthropic(
    req: &OpenAIRequest,
    override_model: &str,
    override_messages: &[OpenAIMessage],
    override_stream: bool,
) -> AnthropicRequest {
    let (system, conversation) = build_anthropic_conversation(override_messages);

    let tools = req
        .tools
        .as_ref()
        .map(|tools| {
            tools
                .iter()
                .filter_map(translate_openai_tool_to_anthropic)
                .collect::<Vec<_>>()
        })
        .filter(|t: &Vec<serde_json::Value>| !t.is_empty());

    let mut extra = serde_json::Map::new();
    let effort_opt = req
        .extra
        .get("reasoning_effort")
        .and_then(|v| v.as_str())
        .or_else(|| req.extra.get("thinking_effort").and_then(|v| v.as_str()));

    let (max_tokens_override, thinking_val) = if let Some(effort) = effort_opt {
        let (val, budget) = match effort {
            "none" => (json!({"type": "disabled"}), 0u32),
            "low" => (json!({"type": "enabled", "budget_tokens": 2048}), 2048),
            "medium" => (json!({"type": "enabled", "budget_tokens": 8192}), 8192),
            "high" => (json!({"type": "enabled", "budget_tokens": 16384}), 16384),
            "max" | "xhigh" => (json!({"type": "enabled", "budget_tokens": 32768}), 32768),
            s => {
                let b = s.parse::<u32>().unwrap_or(8192);
                (json!({"type": "enabled", "budget_tokens": b}), b)
            }
        };
        (budget, Some(val))
    } else if let Some(client_thinking) = req.extra.get("thinking") {
        let budget = client_thinking
            .get("budget_tokens")
            .and_then(|v| v.as_u64())
            .map_or(0, |b| b as u32);
        (budget, Some(client_thinking.clone()))
    } else {
        (0, None)
    };

    if let Some(tv) = thinking_val {
        extra.insert("thinking".to_string(), tv);
    }

    let base_max_tokens = req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
    let effective_max_tokens = if max_tokens_override > 0 {
        base_max_tokens.max(max_tokens_override + 1024)
    } else {
        base_max_tokens
    };

    AnthropicRequest {
        model: override_model.to_string(),
        messages: conversation,
        max_tokens: effective_max_tokens,
        system,
        temperature: req.temperature,
        top_p: req.top_p,
        top_k: req.top_k,
        stop_sequences: req.stop.clone(),
        tools,
        tool_choice: req
            .tool_choice
            .as_ref()
            .and_then(translate_openai_tool_choice_to_anthropic),
        metadata: req
            .user
            .as_ref()
            .map(|u| serde_json::json!({ "user_id": u })),
        stream: override_stream,
        extra,
    }
}

fn flush_assistant_text(conv: &mut Vec<AnthropicMessage>, pending: &mut Vec<String>) {
    if !pending.is_empty() {
        let text = pending.join("\n\n");
        conv.push(AnthropicMessage {
            role: "assistant".to_string(),
            content: serde_json::Value::String(text),
        });
        pending.clear();
    }
}

fn flush_tool_results(conv: &mut Vec<AnthropicMessage>, pending: &mut Vec<serde_json::Value>) {
    if !pending.is_empty() {
        conv.push(AnthropicMessage {
            role: "user".to_string(),
            content: serde_json::Value::Array(std::mem::take(pending)),
        });
    }
}

fn convert_assistant_tool_calls(
    tool_calls: &[serde_json::Value],
    pending_text: &mut Vec<String>,
    msg_text: &str,
) -> Vec<serde_json::Value> {
    let mut blocks: Vec<serde_json::Value> = Vec::new();
    if !pending_text.is_empty() {
        let text = pending_text.join("\n\n");
        blocks.push(json!({"type": "text", "text": text}));
        pending_text.clear();
    }
    if !msg_text.is_empty() {
        blocks.push(json!({"type": "text", "text": msg_text}));
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
        if !name.is_empty() {
            blocks.push(json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input,
            }));
        }
    }
    if blocks.is_empty() {
        blocks.push(json!({"type": "text", "text": ""}));
    }
    blocks
}

fn append_assistant_message(
    m: &OpenAIMessage,
    conversation: &mut Vec<AnthropicMessage>,
    pending_assistant_text: &mut Vec<String>,
) {
    if let Some(tool_calls) = m.tool_calls.as_ref() {
        let text = m.extract_text_cow();
        let blocks = convert_assistant_tool_calls(tool_calls, pending_assistant_text, &text);
        conversation.push(AnthropicMessage {
            role: "assistant".to_string(),
            content: serde_json::Value::Array(blocks),
        });
    } else {
        let text = m.extract_text_cow();
        if !text.is_empty()
            && !text.starts_with("Operation interrupted")
            && !text.starts_with("[System:")
        {
            pending_assistant_text.push(text.into_owned());
        }
    }
}

fn append_user_message(
    m: &OpenAIMessage,
    conversation: &mut Vec<AnthropicMessage>,
    pending_tool_results: &mut Vec<serde_json::Value>,
) {
    let text = m.extract_text_cow();
    if pending_tool_results.is_empty() {
        conversation.push(AnthropicMessage {
            role: "user".to_string(),
            content: serde_json::Value::String(text.into_owned()),
        });
    } else {
        pending_tool_results.push(json!({"type": "text", "text": text}));
        conversation.push(AnthropicMessage {
            role: "user".to_string(),
            content: serde_json::Value::Array(std::mem::take(pending_tool_results)),
        });
    }
}

fn build_anthropic_conversation(
    override_messages: &[OpenAIMessage],
) -> (Option<serde_json::Value>, Vec<AnthropicMessage>) {
    let mut system_parts: Vec<String> = Vec::new();
    let mut conversation: Vec<AnthropicMessage> = Vec::with_capacity(override_messages.len());
    let mut pending_tool_results: Vec<serde_json::Value> = Vec::new();
    let mut pending_assistant_text: Vec<String> = Vec::new();

    for m in override_messages {
        let role = m.role.as_str();
        if role != "assistant" {
            flush_assistant_text(&mut conversation, &mut pending_assistant_text);
        }
        if role != "tool" && role != "user" {
            flush_tool_results(&mut conversation, &mut pending_tool_results);
        }

        match role {
            "system" => {
                let text = m.extract_text();
                system_parts.push(normalize_claude_client_identity(&text).to_string());
            }
            "assistant" => {
                append_assistant_message(m, &mut conversation, &mut pending_assistant_text);
            }
            "user" => {
                append_user_message(m, &mut conversation, &mut pending_tool_results);
            }
            "tool" => {
                let tool_use_id = m.tool_call_id.as_deref().unwrap_or("");
                let content_text = m.extract_text_cow();
                pending_tool_results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "content": content_text,
                }));
            }
            _ => {}
        }
    }

    flush_assistant_text(&mut conversation, &mut pending_assistant_text);
    flush_tool_results(&mut conversation, &mut pending_tool_results);
    log_anthropic_translation_diagnostics(&conversation);

    let system = if system_parts.is_empty() {
        None
    } else {
        Some(serde_json::Value::String(system_parts.join("\n\n")))
    };
    (system, conversation)
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
fn translate_string_tool_choice(s: &str) -> Option<serde_json::Value> {
    match s {
        "auto" => Some(json!({"type": "auto"})),
        "none" => Some(json!({"type": "none"})),
        "required" => Some(json!({"type": "any"})),
        _ => None,
    }
}

fn translate_object_tool_choice(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Option<serde_json::Value> {
    let choice_type = obj.get("type").and_then(|v| v.as_str())?;
    if choice_type == "function" {
        let name = obj
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|v| v.as_str())
            .filter(|n| !n.is_empty())?;
        return Some(json!({"type": "tool", "name": name}));
    }
    translate_string_tool_choice(choice_type)
}

/// Returns `None` for unrecognized shapes (which means the field is
/// omitted from the Anthropic request, defaulting to `auto` upstream).
fn translate_openai_tool_choice_to_anthropic(tc: &serde_json::Value) -> Option<serde_json::Value> {
    match tc {
        serde_json::Value::String(s) => translate_string_tool_choice(s),
        serde_json::Value::Object(obj) => translate_object_tool_choice(obj),
        _ => None,
    }
}
