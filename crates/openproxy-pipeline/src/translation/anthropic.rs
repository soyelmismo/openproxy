use crate::translation::types::{
    AnthropicMessage, AnthropicRequest, AnthropicResponse, AnthropicUsage, DEFAULT_MAX_TOKENS,
    OpenAIChoice, OpenAIResponse, OpenAIUsage,
};
use openproxy_types::{OpenAIMessage, OpenAIRequest};
use serde_json::{Value, json};

pub const CLAUDE_AGENT_SDK_IDENTITY: &str =
    "You are a Claude agent, built on Anthropic's Claude Agent SDK.";
pub const CLAUDE_CODE_CLI_IDENTITY: &str =
    "You are Claude Code, Anthropic's official CLI for Claude.";

/// Normalize the standalone identity block injected by Claude Agent SDK clients.
///
/// Antigravity's upstream classifies this SDK identity differently from Claude Code's
/// CLI identity and can reject an otherwise identical request with RESOURCE_EXHAUSTED.
/// Keep the match exact so user-authored text that merely mentions the SDK identity is
/// not rewritten.
pub fn normalize_claude_client_identity(text: &str) -> &str {
    if text == CLAUDE_AGENT_SDK_IDENTITY {
        CLAUDE_CODE_CLI_IDENTITY
    } else {
        text
    }
}

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

    AnthropicRequest {
        model: override_model.to_string(),
        messages: conversation,
        max_tokens: req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
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
        extra: Default::default(),
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

fn translate_object_tool_choice(obj: &serde_json::Map<String, serde_json::Value>) -> Option<serde_json::Value> {
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

    let cache_read = resp.usage.cache_read_input_tokens.unwrap_or(0);
    let cache_creation = resp.usage.cache_creation_input_tokens.unwrap_or(0);
    let prompt_tokens = resp
        .usage
        .input_tokens
        .saturating_add(cache_read)
        .saturating_add(cache_creation);
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
            prompt_tokens_details: resp.usage.cache_read_input_tokens.map(|c| {
                openproxy_types::message::PromptTokensDetails {
                    cached_tokens: Some(c),
                }
            }),
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

fn build_openai_system_message(sys: serde_json::Value) -> OpenAIMessage {
    let sys_str = if let Some(s) = sys.as_str() {
        normalize_claude_client_identity(s).to_string()
    } else if let Some(arr) = sys.as_array() {
        arr.iter()
            .filter_map(|v| v.get("text").and_then(|t| t.as_str()))
            .map(normalize_claude_client_identity)
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        sys.to_string()
    };
    OpenAIMessage {
        role: "system".to_string(),
        content: Some(serde_json::Value::String(sys_str)),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        extra: Default::default(),
    }
}

fn parse_anthropic_blocks(
    arr: &[serde_json::Value],
    text_blocks: &mut Vec<String>,
    tool_calls: &mut Vec<serde_json::Value>,
    tool_results: &mut Vec<(String, serde_json::Value)>,
) {
    for block in arr {
        parse_single_anthropic_block(block, text_blocks, tool_calls, tool_results);
    }
}

fn parse_single_anthropic_block(
    block: &serde_json::Value,
    text_blocks: &mut Vec<String>,
    tool_calls: &mut Vec<serde_json::Value>,
    tool_results: &mut Vec<(String, serde_json::Value)>,
) {
    let Some(typ) = block.get("type").and_then(|v| v.as_str()) else {
        return;
    };
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
                tool_results.push((id.to_string(), res_content.to_owned()));
            }
        }
        _ => {}
    }
}

fn convert_anthropic_message_to_openai(m: AnthropicMessage, messages: &mut Vec<OpenAIMessage>) {
    let mut text_blocks = Vec::new();
    let mut tool_calls = Vec::new();
    let mut tool_results = Vec::new();

    if let Some(arr) = m.content.as_array() {
        parse_anthropic_blocks(arr, &mut text_blocks, &mut tool_calls, &mut tool_results);
    } else if let Some(s) = m.content.as_str() {
        text_blocks.push(s.to_string());
    }

    match m.role.as_str() {
        "assistant" => {
            let tc = (!tool_calls.is_empty()).then_some(tool_calls);
            let content = if text_blocks.is_empty() && tc.is_some() {
                Some(serde_json::Value::Null)
            } else {
                Some(serde_json::Value::String(text_blocks.join("\n\n")))
            };
            messages.push(OpenAIMessage {
                role: m.role,
                content,
                name: None,
                tool_call_id: None,
                tool_calls: tc,
                extra: Default::default(),
            });
        }
        "user" => {
            emit_anthropic_user_and_tools(m.role, tool_results, text_blocks, messages);
        }
        _ => {
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
}

fn emit_anthropic_user_and_tools(
    role: String,
    tool_results: Vec<(String, serde_json::Value)>,
    text_blocks: Vec<String>,
    messages: &mut Vec<OpenAIMessage>,
) {
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
            role,
            content: Some(serde_json::Value::String(text_blocks.join("\n\n"))),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        });
    }
}

fn extract_anthropic_json_schema_response_format(extra_req: &serde_json::Map<String, serde_json::Value>) -> Option<serde_json::Value> {
    let output_config = extra_req.get("output_config")?;
    let format = output_config.get("format")?;
    if format.get("type").and_then(|v| v.as_str()) != Some("json_schema") {
        return None;
    }
    let schema = format.get("schema")?;
    Some(serde_json::json!({
        "type": "json_schema",
        "json_schema": {
            "name": "json_response",
            "strict": true,
            "schema": schema
        }
    }))
}

fn build_openai_request_extra(
    metadata: Option<serde_json::Value>,
    extra_req: &serde_json::Map<String, serde_json::Value>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut extra = metadata
        .map(|m| {
            let mut map = serde_json::Map::new();
            map.insert("metadata".to_string(), m);
            map
        })
        .unwrap_or_default();

    if let Some(response_format) = extract_anthropic_json_schema_response_format(extra_req) {
        extra.insert("response_format".to_string(), response_format);
    }
    extra
}

pub fn anthropic_request_to_openai(req: AnthropicRequest) -> OpenAIRequest {
    let mut messages = Vec::with_capacity(req.messages.len() + usize::from(req.system.is_some()));
    if let Some(sys) = req.system {
        messages.push(build_openai_system_message(sys));
    }
    for m in req.messages {
        convert_anthropic_message_to_openai(m, &mut messages);
    }

    let tools = req.tools.map(translate_anthropic_tools_to_openai);
    let tool_choice = req
        .tool_choice
        .map(translate_anthropic_tool_choice_to_openai);
    let extra = build_openai_request_extra(req.metadata, &req.extra);

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

fn map_openai_finish_reason_to_anthropic(finish_reason: Option<&str>) -> Option<String> {
    match finish_reason {
        Some("length") => Some("max_tokens".to_string()),
        Some("tool_calls" | "function_call") => Some("tool_use".to_string()),
        Some("content_filter") => Some("stop_sequence".to_string()),
        Some(_) => Some("end_turn".to_string()),
        None => None,
    }
}

fn build_anthropic_content_from_openai_choice(choice: &crate::translation::Choice) -> Vec<serde_json::Value> {
    let mut content = Vec::new();
    if let Some(s) = choice.message.content.as_ref().and_then(|c| c.as_str()).filter(|s| !s.is_empty()) {
        content.push(serde_json::json!({
            "type": "text",
            "text": s.to_string()
        }));
    }
    if let Some(tool_calls) = &choice.message.tool_calls {
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
    content
}

fn extract_openai_usage(usage_opt: Option<OpenAIUsage>) -> (u32, u32, Option<u32>) {
    let usage = usage_opt.unwrap_or(OpenAIUsage {
        prompt_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
        prompt_tokens_details: None,
    });
    let cached = usage.prompt_tokens_details.and_then(|d| d.cached_tokens);
    (usage.prompt_tokens, usage.completion_tokens, cached)
}

pub fn openai_response_to_anthropic(resp: OpenAIResponse) -> AnthropicResponse {
    let first_choice = resp.choices.first();
    let content = first_choice.map_or_else(Vec::new, build_anthropic_content_from_openai_choice);
    let finish_reason = first_choice.and_then(|c| c.finish_reason.as_deref());
    let anthropic_stop = map_openai_finish_reason_to_anthropic(finish_reason);
    let (input_tokens, output_tokens, cache_read_input_tokens) = extract_openai_usage(resp.usage);

    AnthropicResponse {
        id: resp.id,
        response_type: "message".to_string(),
        role: "assistant".to_string(),
        content,
        model: resp.model,
        stop_reason: anthropic_stop,
        usage: AnthropicUsage {
            input_tokens,
            output_tokens,
            cache_creation_input_tokens: None,
            cache_read_input_tokens,
        },
    }
}

fn collect_message_diagnostic_ids(
    m: &AnthropicMessage,
    tool_use_ids: &mut Vec<String>,
    tool_result_ids: &mut Vec<String>,
) {
    let Some(arr) = m.content.as_array() else {
        return;
    };
    for block in arr {
        let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match (m.role.as_str(), block_type) {
            ("assistant", "tool_use") => {
                if let Some(id) = block.get("id").and_then(|v| v.as_str()) {
                    tool_use_ids.push(id.to_string());
                }
            }
            ("user", "tool_result") => {
                if let Some(id) = block.get("tool_use_id").and_then(|v| v.as_str()) {
                    tool_result_ids.push(id.to_string());
                }
            }
            _ => {}
        }
    }
}

fn collect_diagnostic_tool_ids(conversation: &[AnthropicMessage]) -> (Vec<String>, Vec<String>) {
    let mut tool_use_ids = Vec::new();
    let mut tool_result_ids = Vec::new();
    for m in conversation {
        collect_message_diagnostic_ids(m, &mut tool_use_ids, &mut tool_result_ids);
    }
    (tool_use_ids, tool_result_ids)
}

fn warn_consecutive_same_roles(conversation: &[AnthropicMessage]) {
    for (i, window) in conversation.windows(2).enumerate() {
        if window[0].role == window[1].role {
            tracing::warn!(
                idx = i + 1,
                role = %window[1].role,
                "translation: consecutive same-role messages — Anthropic/MiniMax rejects this with (2013)"
            );
        }
    }
}

fn log_tool_id_diagnostics(tool_use_ids: &[String], tool_result_ids: &[String]) {
    let use_set: std::collections::HashSet<&str> = tool_use_ids.iter().map(std::string::String::as_str).collect();
    let result_set: std::collections::HashSet<&str> = tool_result_ids.iter().map(std::string::String::as_str).collect();
    let missing_results: Vec<&str> = use_set.difference(&result_set).copied().collect();
    let orphan_results: Vec<&str> = result_set.difference(&use_set).copied().collect();

    if !missing_results.is_empty() || !orphan_results.is_empty() {
        tracing::warn!(
            missing_results = ?missing_results,
            orphan_results = ?orphan_results,
            "translation: tool_use/tool_result ID mismatch — MiniMax will reject with (2013)"
        );
    }
}

fn log_anthropic_translation_diagnostics(conversation: &[AnthropicMessage]) {
    if !tracing::enabled!(tracing::Level::DEBUG) {
        return;
    }
    let role_seq: Vec<&str> = conversation.iter().map(|m| m.role.as_str()).collect();
    let (tool_use_ids, tool_result_ids) = collect_diagnostic_tool_ids(conversation);

    tracing::debug!(
        role_sequence = ?role_seq,
        tool_use_count = tool_use_ids.len(),
        tool_result_count = tool_result_ids.len(),
        tool_use_ids = ?tool_use_ids,
        tool_result_ids = ?tool_result_ids,
        "openai_to_anthropic translation result"
    );

    log_tool_id_diagnostics(&tool_use_ids, &tool_result_ids);
    warn_consecutive_same_roles(conversation);
}

fn translate_anthropic_tools_to_openai(ts: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    ts.into_iter()
        .map(|mut t| {
            if let Some(obj) = t.as_object_mut() {
                let mut f = serde_json::Map::new();
                if let Some(n) = obj.remove("name") {
                    f.insert("name".to_string(), n);
                }
                if let Some(d) = obj.remove("description") {
                    f.insert("description".to_string(), d);
                }
                if let Some(s) = obj.remove("input_schema") {
                    f.insert("parameters".to_string(), s);
                }
                serde_json::json!({
                    "type": "function",
                    "function": f
                })
            } else {
                t
            }
        })
        .collect()
}

fn translate_anthropic_tool_choice_to_openai(tc: serde_json::Value) -> serde_json::Value {
    if let Some(obj) = tc.as_object()
        && obj.get("type").and_then(|v| v.as_str()) == Some("tool")
        && let Some(name) = obj.get("name")
    {
        return serde_json::json!({
            "type": "function",
            "function": { "name": name }
        });
    }
    tc
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_translate_anthropic_tool_choice_to_openai() {
        let anthropic_tc = json!({
            "type": "tool",
            "name": "get_weather"
        });

        let openai_tc = translate_anthropic_tool_choice_to_openai(anthropic_tc);

        assert_eq!(
            openai_tc,
            json!({
                "type": "function",
                "function": { "name": "get_weather" }
            })
        );

        let fallback_tc = json!({"type": "any"});
        assert_eq!(
            translate_anthropic_tool_choice_to_openai(fallback_tc.clone()),
            fallback_tc
        );
    }

    #[test]
    fn test_normalize_claude_client_identity() {
        assert_eq!(
            normalize_claude_client_identity(CLAUDE_AGENT_SDK_IDENTITY),
            CLAUDE_CODE_CLI_IDENTITY
        );
        let custom_prompt = "You are a specialized coding agent.";
        assert_eq!(
            normalize_claude_client_identity(custom_prompt),
            custom_prompt
        );
    }
}
