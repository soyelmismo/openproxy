use super::diagnostics::{
    translate_anthropic_tool_choice_to_openai, translate_anthropic_tools_to_openai,
};
use super::identity::normalize_claude_client_identity;
use crate::translation::types::{
    AnthropicMessage, AnthropicRequest, AnthropicResponse, AnthropicUsage, OpenAIResponse,
    OpenAIUsage,
};
use openproxy_types::{OpenAIMessage, OpenAIRequest};

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

fn extract_anthropic_json_schema_response_format(
    extra_req: &serde_json::Map<String, serde_json::Value>,
) -> Option<serde_json::Value> {
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

fn build_anthropic_content_from_openai_choice(
    choice: &crate::translation::Choice,
) -> Vec<serde_json::Value> {
    let mut content = Vec::new();
    if let Some(s) = choice
        .message
        .content
        .as_ref()
        .and_then(|c| c.as_str())
        .filter(|s| !s.is_empty())
    {
        content.push(serde_json::json!({
            "type": "text",
            "text": s.to_string()
        }));
    }
    if let Some(tool_calls) = &choice.message.tool_calls {
        for tc in tool_calls {
            if let (Some(id), Some(function)) = (tc.get("id"), tc.get("function")) {
                let name = function
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or_default();
                let arguments_str = function
                    .get("arguments")
                    .and_then(|a| a.as_str())
                    .unwrap_or("{}");
                let input = serde_json::from_str::<serde_json::Value>(arguments_str)
                    .unwrap_or_else(|_| serde_json::json!({}));
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
