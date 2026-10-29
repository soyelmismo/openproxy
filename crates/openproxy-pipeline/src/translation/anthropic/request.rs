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
    let reasoning_obj = req.extra.get("reasoning").and_then(|v| v.as_object());
    let reasoning_enabled = reasoning_obj
        .and_then(|o| o.get("enabled"))
        .and_then(|v| v.as_bool());
    let effort_from_obj = reasoning_obj
        .and_then(|o| o.get("effort"))
        .and_then(|v| v.as_str());

    let effort_opt = req
        .extra
        .get("reasoning_effort")
        .and_then(|v| v.as_str())
        .or(effort_from_obj)
        .or_else(|| req.extra.get("reasoningEffort").and_then(|v| v.as_str()))
        .or_else(|| req.extra.get("thinking_effort").and_then(|v| v.as_str()))
        .or_else(|| {
            if reasoning_enabled == Some(false) {
                Some("none")
            } else if reasoning_enabled == Some(true) {
                Some("medium")
            } else {
                None
            }
        });

    let (max_tokens_override, thinking_val) = if let Some(effort) = effort_opt {
        let (val, budget) = match effort {
            "none" => (None, 0u32),
            "low" => (
                Some(json!({"type": "enabled", "budget_tokens": 2048})),
                2048,
            ),
            "medium" => (
                Some(json!({"type": "enabled", "budget_tokens": 8192})),
                8192,
            ),
            "high" => (
                Some(json!({"type": "enabled", "budget_tokens": 16384})),
                16384,
            ),
            "max" | "xhigh" => (
                Some(json!({"type": "enabled", "budget_tokens": 32768})),
                32768,
            ),
            s => {
                let b = s.parse::<u32>().unwrap_or(8192);
                (Some(json!({"type": "enabled", "budget_tokens": b})), b)
            }
        };
        (budget, val)
    } else if let Some(client_thinking) = req.extra.get("thinking") {
        let is_disabled = client_thinking
            .get("type")
            .and_then(|v| v.as_str())
            .is_some_and(|t| t == "disabled");
        if is_disabled {
            (0, None)
        } else {
            let budget = client_thinking
                .get("budget_tokens")
                .and_then(|v| v.as_u64())
                .map_or(0, |b| b as u32);
            (budget, Some(client_thinking.clone()))
        }
    } else {
        (0, None)
    };

    if let Some(tv) = thinking_val {
        extra.insert("thinking".to_string(), tv);
    }

    let base_max_tokens = req
        .max_tokens
        .or_else(|| {
            req.extra
                .get("max_completion_tokens")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32)
        })
        .unwrap_or(DEFAULT_MAX_TOKENS);
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

fn extract_signed_thinking_blocks(m: &OpenAIMessage) -> Vec<serde_json::Value> {
    let mut thinking_blocks = Vec::new();
    if let Some(arr) = m.content.as_ref().and_then(|c| c.as_array()) {
        for b in arr {
            if b.get("type").and_then(|t| t.as_str()) == Some("thinking")
                && b.get("signature")
                    .and_then(|s| s.as_str())
                    .is_some_and(|s| !s.is_empty())
            {
                thinking_blocks.push(b.clone());
            }
        }
    }
    thinking_blocks
}

fn extract_assistant_text_without_thinking(m: &OpenAIMessage) -> String {
    if let Some(arr) = m.content.as_ref().and_then(|c| c.as_array()) {
        let mut out = String::new();
        for b in arr {
            let b_type = b.get("type").and_then(|t| t.as_str());
            if b_type != Some("thinking") && b_type != Some("reasoning") {
                if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                    out.push_str(t);
                } else if let Some(t) = b.get("content").and_then(|v| v.as_str()) {
                    out.push_str(t);
                }
            }
        }
        out
    } else {
        m.extract_text()
    }
}

fn append_assistant_message(
    m: &OpenAIMessage,
    conversation: &mut Vec<AnthropicMessage>,
    pending_assistant_text: &mut Vec<String>,
) {
    let signed_thinking = extract_signed_thinking_blocks(m);

    if let Some(tool_calls) = m.tool_calls.as_ref() {
        let text = if signed_thinking.is_empty() {
            m.extract_text()
        } else {
            extract_assistant_text_without_thinking(m)
        };
        let mut blocks = convert_assistant_tool_calls(tool_calls, pending_assistant_text, &text);
        if !signed_thinking.is_empty() {
            blocks.splice(0..0, signed_thinking);
        }
        conversation.push(AnthropicMessage {
            role: "assistant".to_string(),
            content: serde_json::Value::Array(blocks),
        });
    } else if !signed_thinking.is_empty() {
        flush_assistant_text(conversation, pending_assistant_text);
        let text = extract_assistant_text_without_thinking(m);
        let mut blocks = signed_thinking;
        if !text.is_empty() {
            blocks.push(json!({"type": "text", "text": text}));
        }
        conversation.push(AnthropicMessage {
            role: "assistant".to_string(),
            content: serde_json::Value::Array(blocks),
        });
    } else {
        let text = extract_assistant_text_without_thinking(m);
        if !text.is_empty()
            && !text.starts_with("Operation interrupted")
            && !text.starts_with("[System:")
        {
            pending_assistant_text.push(text);
        }
    }
}

/// Splits an OpenAI media part into the URL it carries, whatever field the
/// client used: `image_url`/`video_url` as an object or a bare string, or a
/// top-level `url`.
fn openai_part_media_url(
    obj: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<String> {
    obj.get(key)
        .and_then(|u| {
            if let Some(s) = u.as_str() {
                Some(s)
            } else {
                u.get("url").and_then(|v| v.as_str())
            }
        })
        .or_else(|| obj.get("url").and_then(|v| v.as_str()))
        .map(|s| s.to_string())
}

/// Builds an Anthropic `source` object from a URL-shaped string: data URIs
/// become `base64`, plain http(s) URLs become `url`, and anything else (a
/// provider file id such as `mm_file://...`) becomes a `file` source so the
/// upstream can resolve it.
fn anthropic_source_from_url(url: &str, media_type: Option<&str>) -> serde_json::Value {
    if let Some(data_part) = url.strip_prefix("data:")
        && let Some((mt, base64_data)) = data_part.split_once(";base64,")
    {
        return json!({
            "type": "base64",
            "media_type": mt,
            "data": base64_data,
        });
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        return json!({ "type": "url", "url": url });
    }
    json!({
        "type": "file",
        "file_id": url.strip_prefix("mm_file://").unwrap_or(url),
        "media_type": media_type.unwrap_or("application/octet-stream"),
    })
}

/// Maps one OpenAI content part onto its Anthropic block.
///
/// Covers every modality the Messages API accepts from MiniMax M3.1-Flash and
/// M3 (`text`, `image`, `video`, `document`) plus audio, which reaches this
/// layer as an OpenAI `input_audio` part.
fn convert_openai_part_to_anthropic(part: &serde_json::Value) -> serde_json::Value {
    let Some(obj) = part.as_object() else {
        return part.clone();
    };
    let typ = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");

    let (anthropic_type, url) = match typ {
        "image_url" | "input_image" => ("image", openai_part_media_url(obj, "image_url")),
        "input_video" | "video_url" => ("video", openai_part_media_url(obj, "video_url")),
        "file" | "input_file" => {
            let url = obj
                .get("file")
                .and_then(|f| {
                    f.get("file_data")
                        .and_then(|v| v.as_str())
                        .or_else(|| f.get("file_id").and_then(|v| v.as_str()))
                })
                .or_else(|| obj.get("file_data").and_then(|v| v.as_str()))
                .or_else(|| obj.get("file_id").and_then(|v| v.as_str()))
                .map(|s| s.to_string());
            ("document", url)
        }
        "input_audio" => {
            // OpenAI carries audio inline as `{data, format}`; Anthropic takes a
            // base64 source, so rebuild the data URI the source builder wants.
            let url = obj
                .get("input_audio")
                .and_then(|a| {
                    let data = a.get("data").and_then(|v| v.as_str())?;
                    let format = a.get("format").and_then(|v| v.as_str()).unwrap_or("wav");
                    Some(format!("data:audio/{format};base64,{data}"))
                })
                .or_else(|| openai_part_media_url(obj, "audio_url"));
            ("audio", url)
        }
        _ => return part.clone(),
    };

    let Some(url) = url else {
        return part.clone();
    };
    let media_type = obj.get("media_type").and_then(|v| v.as_str());
    json!({
        "type": anthropic_type,
        "source": anthropic_source_from_url(&url, media_type),
    })
}

/// Content of an Anthropic `tool_result` built from an OpenAI `tool` message.
///
/// Clients such as Hermes return screenshots and `vision_analyze` pixels as a
/// tool message whose content is `[text, image_url]`. Flattening that to text
/// drops the image inside the proxy: the model answers from the text stub and
/// never sees the pixels. `tool_result.content` accepts `text` and `image`
/// blocks, so media parts are carried as blocks; plain text stays a string.
fn tool_result_content(m: &OpenAIMessage) -> serde_json::Value {
    let Some(serde_json::Value::Array(parts)) = &m.content else {
        return serde_json::Value::String(m.extract_text_cow().into_owned());
    };
    let has_media = parts.iter().any(|p| {
        p.get("type")
            .and_then(|t| t.as_str())
            .is_some_and(|t| t != "text")
    });
    if !has_media {
        return serde_json::Value::String(m.extract_text_cow().into_owned());
    }
    let blocks: Vec<serde_json::Value> = parts
        .iter()
        .map(convert_openai_part_to_anthropic)
        .filter(|b| match b.get("type").and_then(|t| t.as_str()) {
            Some("text") => b
                .get("text")
                .and_then(|t| t.as_str())
                .is_some_and(|t| !t.is_empty()),
            _ => true,
        })
        .collect();
    serde_json::Value::Array(blocks)
}

fn append_user_message(
    m: &OpenAIMessage,
    conversation: &mut Vec<AnthropicMessage>,
    pending_tool_results: &mut Vec<serde_json::Value>,
) {
    if pending_tool_results.is_empty() {
        if let Some(serde_json::Value::Array(parts)) = &m.content {
            let converted_parts: Vec<serde_json::Value> =
                parts.iter().map(convert_openai_part_to_anthropic).collect();
            conversation.push(AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::Value::Array(converted_parts),
            });
            return;
        }

        let text = m.extract_text_cow();
        conversation.push(AnthropicMessage {
            role: "user".to_string(),
            content: serde_json::Value::String(text.into_owned()),
        });
    } else {
        if let Some(serde_json::Value::Array(parts)) = &m.content {
            for part in parts {
                pending_tool_results.push(convert_openai_part_to_anthropic(part));
            }
        } else {
            let text = m.extract_text_cow();
            pending_tool_results.push(json!({"type": "text", "text": text}));
        }
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
                pending_tool_results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "content": tool_result_content(m),
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

/// OpenAI `{"type":"function","function":{...,"parameters":{...}}}` becomes
/// Anthropic `{"name":...,"description":...,"input_schema":{...}}`.
///
/// The `function` and `name` fields are required: MiniMax rejects a tool with an
/// empty name with `(2013)`.
fn translate_openai_tool_to_anthropic(tool: &serde_json::Value) -> Option<serde_json::Value> {
    let function = tool.get("function")?;
    let name = function.get("name").and_then(|v| v.as_str())?;
    if name.is_empty() {
        return None;
    }
    let description = function.get("description").and_then(|v| v.as_str());
    // Anthropic requires input_schema to be a present, valid schema object.
    let input_schema = function.get("parameters").cloned().unwrap_or(json!({}));
    Some(json!({
        "name": name,
        "description": description,
        "input_schema": input_schema,
    }))
}

/// Anthropic `tool_choice`: `auto` (model decides), `none`, `any` (OpenAI's
/// `required`), or `{"type":"tool","name":"X"}` to force one tool. The
/// OpenAI source is a string (`"auto"` / `"none"` / `"required"`), a
/// `{"type":"function","function":{"name":"X"}}` object, or the object form
/// of those strings.
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

/// `None` for unrecognized shapes, which omits the field so upstream
/// defaults to `auto`.
fn translate_openai_tool_choice_to_anthropic(tc: &serde_json::Value) -> Option<serde_json::Value> {
    match tc {
        serde_json::Value::String(s) => translate_string_tool_choice(s),
        serde_json::Value::Object(obj) => translate_object_tool_choice(obj),
        _ => None,
    }
}
