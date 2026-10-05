use crate::PipelineRequest;
use openproxy_types::OpenAIMessage;
use openproxy_types::error::CoreError;
use openproxy_types::models::Model;
use serde_json::{Value, json};

use super::TargetFormatter;

pub struct ResponsesFormatter;

impl TargetFormatter for ResponsesFormatter {
    fn format_request(
        &self,
        req: &PipelineRequest,
        model: &Model,
        messages_ref: &[OpenAIMessage],
        stream: bool,
        _adapter: &openproxy_adapters::adapters::ProviderAdapterEnum,
    ) -> Result<bytes::Bytes, CoreError> {
        let (resolved_model, effort_from_model) =
            normalize_model_and_effort(model.model_id.as_str());
        let mut obj = req.openai_request.extra.clone();
        obj.insert("model".to_string(), Value::String(resolved_model));

        let (system_instructions, _messages_without_system) =
            extract_system_and_messages(messages_ref);

        obj.insert(
            "input".to_string(),
            messages_to_responses_input(messages_ref),
        );
        obj.insert("stream".to_string(), Value::Bool(stream));
        obj.insert("store".to_string(), Value::Bool(false));

        let default_instructions =
            "Follow the developer instructions in the conversation.".to_string();
        obj.entry("instructions".to_string()).or_insert_with(|| {
            Value::String(system_instructions.unwrap_or(default_instructions.clone()))
        });

        let custom_instructions = if !messages_ref
            .iter()
            .any(|m| m.role == "system" || m.role == "developer")
        {
            obj.get("instructions")
                .and_then(Value::as_str)
                .filter(|inst| !inst.is_empty() && *inst != default_instructions)
                .map(str::to_string)
        } else {
            None
        };

        if let Some(inst) = custom_instructions
            && let Some(arr) = obj.get_mut("input").and_then(Value::as_array_mut)
            && !arr
                .iter()
                .any(|item| item.get("role").and_then(Value::as_str) == Some("developer"))
        {
            arr.insert(
                0,
                json!({
                    "role": "developer",
                    "content": [{
                        "type": "input_text",
                        "text": inst
                    }]
                }),
            );
        }

        if let Some(temperature) = req.openai_request.temperature {
            obj.insert("temperature".to_string(), json!(temperature));
        }
        if let Some(top_p) = req.openai_request.top_p {
            obj.insert("top_p".to_string(), json!(top_p));
        }
        if let Some(tools) = format_responses_tools(req.openai_request.tools.as_deref()) {
            obj.insert("tools".to_string(), tools);
        }
        if let Some(choice) = format_responses_tool_choice(req.openai_request.tool_choice.as_ref())
        {
            obj.insert("tool_choice".to_string(), choice);
        }

        strip_responses_disallowed_keys(&mut obj);
        for (k, v) in &_adapter.config().extra_headers {
            if k.eq_ignore_ascii_case("x-openproxy-strip-params")
                || k.eq_ignore_ascii_case("x-strip-params")
            {
                for param in v.split(',') {
                    let clean = param.trim();
                    if !clean.is_empty() {
                        obj.remove(clean);
                    }
                }
            }
        }
        apply_responses_reasoning_and_tier(&mut obj, effort_from_model);

        if obj
            .get("prompt_cache_key")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            let instructions_str = obj
                .get("instructions")
                .and_then(|v| v.as_str())
                .unwrap_or("Follow the developer instructions in the conversation.");
            let pck = compute_responses_prompt_cache_key(
                instructions_str,
                req.openai_request.tools.as_deref(),
            );
            obj.insert("prompt_cache_key".to_string(), Value::String(pck));
        }

        if let Some(arr) = obj.get_mut("input").and_then(Value::as_array_mut) {
            for item in arr.iter_mut() {
                sanitize_responses_reasoning_item(item);
            }
        }

        serde_json::to_vec(&Value::Object(obj))
            .map(bytes::Bytes::from)
            .map_err(|e| CoreError::Parse(format!("serialize responses request: {e}")))
    }
}

pub(crate) fn extract_system_and_messages(
    messages_ref: &[OpenAIMessage],
) -> (Option<String>, Vec<&OpenAIMessage>) {
    let mut instructions_parts = Vec::new();
    let mut messages_without_system = Vec::new();
    let mut leading_system = true;

    for msg in messages_ref {
        if (msg.role == "system" || msg.role == "developer") && leading_system {
            let text = content_to_text(msg.content.as_ref());
            if !text.is_empty() {
                instructions_parts.push(text);
            }
        } else {
            leading_system = false;
            messages_without_system.push(msg);
        }
    }

    let instructions = if instructions_parts.is_empty() {
        None
    } else {
        Some(instructions_parts.join("\n\n"))
    };

    (instructions, messages_without_system)
}

pub(crate) fn format_responses_tools(tools: Option<&[Value]>) -> Option<Value> {
    let tools = tools?;
    let mut flat_tools = Vec::with_capacity(tools.len());
    for tool in tools {
        let mut flat_tool = tool.clone();
        if let Some(obj) = flat_tool.as_object_mut()
            && obj.get("type").and_then(|v| v.as_str()) == Some("function")
            && let Some(mut func) = obj.remove("function")
            && let Some(func_obj) = func.as_object_mut()
        {
            if let Some(name) = func_obj.remove("name") {
                obj.insert("name".to_string(), name);
            }
            if let Some(desc) = func_obj.remove("description") {
                obj.insert("description".to_string(), desc);
            }
            if let Some(mut params) = func_obj.remove("parameters") {
                crate::schema_sanitizer::sanitize_tool_parameters_schema(&mut params);
                obj.insert("parameters".to_string(), params);
            }
        }
        flat_tools.push(flat_tool);
    }
    Some(Value::Array(flat_tools))
}

pub(crate) fn format_responses_tool_choice(tool_choice: Option<&Value>) -> Option<Value> {
    let tool_choice = tool_choice?;
    let mut flat_choice = tool_choice.clone();
    if let Some(obj) = flat_choice.as_object_mut()
        && obj.get("type").and_then(|v| v.as_str()) == Some("function")
        && let Some(mut func) = obj.remove("function")
        && let Some(func_obj) = func.as_object_mut()
        && let Some(name) = func_obj.remove("name")
    {
        obj.insert("name".to_string(), name);
    }
    Some(flat_choice)
}

pub(crate) fn strip_responses_disallowed_keys(obj: &mut serde_json::Map<String, Value>) {
    const DISALLOWED: &[&str] = &[
        "max_tokens",
        "max_completion_tokens",
        "max_output_tokens",
        "truncation",
        "background",
        "prompt_cache_retention",
        "safety_identifier",
        "user",
        "stream_options",
    ];
    for key in DISALLOWED {
        obj.remove(*key);
    }
}

pub(crate) fn apply_responses_reasoning_and_tier(
    obj: &mut serde_json::Map<String, Value>,
    effort_from_model: Option<&'static str>,
) {
    let effort_val = obj.remove("reasoning_effort");
    let effort = effort_val
        .as_ref()
        .and_then(|v| v.as_str())
        .map(normalize_effort)
        .or(effort_from_model);
    if let Some(effort) = effort.filter(|v| *v != "none") {
        obj.insert(
            "reasoning".to_string(),
            json!({
                "effort": effort,
                "summary": "auto"
            }),
        );
    }
    if matches!(
        obj.get("service_tier").and_then(|v| v.as_str()),
        Some("fast")
    ) {
        obj.insert(
            "service_tier".to_string(),
            Value::String("priority".to_string()),
        );
    }
}

pub(crate) fn compute_responses_prompt_cache_key(
    instructions_str: &str,
    tools: Option<&[Value]>,
) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;
    let mut hasher = Sha256::new();
    hasher.update(instructions_str.as_bytes());
    if let Some(tools) = tools
        && let Ok(tools_str) = serde_json::to_string(tools)
    {
        hasher.update(tools_str.as_bytes());
    }
    let hash = hasher.finalize();
    let mut pck = String::with_capacity(28);
    pck.push_str("pck_");
    for b in &hash[..12] {
        let _ = write!(pck, "{b:02x}");
    }
    pck
}

pub(crate) fn convert_image_url_part(item: &Value) -> Option<Value> {
    let (url, detail) = if let Some(url_obj) = item.get("image_url").and_then(Value::as_object) {
        let u = url_obj.get("url").and_then(Value::as_str)?;
        let d = url_obj
            .get("detail")
            .and_then(Value::as_str)
            .or_else(|| item.get("detail").and_then(Value::as_str));
        (u.to_string(), d)
    } else if let Some(u) = item
        .get("image_url")
        .and_then(Value::as_str)
        .or_else(|| item.get("url").and_then(Value::as_str))
    {
        (u.to_string(), item.get("detail").and_then(Value::as_str))
    } else if let Some(fid) = item.get("file_id").and_then(Value::as_str) {
        return Some(json!({ "type": "input_image", "file_id": fid }));
    } else {
        let data = item.get("image").and_then(Value::as_str)?;
        let mime = item
            .get("mime_type")
            .and_then(Value::as_str)
            .unwrap_or("image/jpeg");
        let u = if data.starts_with("data:") {
            data.to_string()
        } else {
            format!("data:{mime};base64,{data}")
        };
        (u, item.get("detail").and_then(Value::as_str))
    };
    let mut out = json!({ "type": "input_image", "image_url": url });
    if let Some(d) = detail {
        out["detail"] = Value::String(d.to_string());
    }
    Some(out)
}

pub(crate) fn convert_image_source_part(item: &Value) -> Option<Value> {
    if let Some(source) = item.get("source").and_then(Value::as_object)
        && let Some(data) = source
            .get("data")
            .and_then(Value::as_str)
            .filter(|d| !d.is_empty())
    {
        let mime = source
            .get("media_type")
            .and_then(Value::as_str)
            .unwrap_or("image/jpeg");
        let u = if data.starts_with("data:") {
            data.to_string()
        } else {
            format!("data:{mime};base64,{data}")
        };
        return Some(json!({ "type": "input_image", "image_url": u }));
    }
    convert_image_url_part(item)
}

pub(crate) fn convert_content_item_to_part(item: &Value, text_type: &str) -> Option<Value> {
    let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("text");
    match item_type {
        "text" | "input_text" | "output_text" => {
            let text = item.get("text").and_then(|v| v.as_str()).unwrap_or("");
            Some(json!({ "type": text_type, "text": text }))
        }
        "image_url" | "input_image" => convert_image_url_part(item),
        "image" => convert_image_source_part(item),
        _ => None,
    }
}

pub(crate) fn convert_msg_content_to_parts(content: Option<&Value>, text_type: &str) -> Vec<Value> {
    match content {
        Some(Value::String(text)) if !text.is_empty() => {
            vec![json!({ "type": text_type, "text": text })]
        }
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|item| convert_content_item_to_part(item, text_type))
            .filter(|part| {
                part.get("text")
                    .and_then(Value::as_str)
                    .is_none_or(|t| !t.is_empty())
            })
            .collect(),
        Some(value) if !value.is_null() => {
            let s = if let Some(s) = value.as_str() {
                s.to_string()
            } else {
                value.to_string()
            };
            if !s.is_empty() {
                vec![json!({ "type": text_type, "text": s })]
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    }
}

pub(crate) fn convert_msg_tool_calls(tool_calls: &[Value], input_items: &mut Vec<Value>) {
    for call in tool_calls {
        let call_id = call
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("call_xyz")
            .to_string();
        let func_name = call
            .get("function")
            .and_then(|v| v.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let func_args = call
            .get("function")
            .and_then(|v| v.get("arguments"))
            .and_then(|v| v.as_str())
            .unwrap_or("{}")
            .to_string();

        input_items.push(json!({
            "type": "function_call",
            "call_id": call_id,
            "name": func_name,
            "arguments": func_args
        }));
    }
}

pub(crate) fn extract_reasoning_content(msg: &OpenAIMessage) -> Option<String> {
    for key in &["reasoning_content", "reasoning", "thinking"] {
        if let Some(val) = msg.extra.get(*key) {
            match val {
                Value::String(s) if !s.is_empty() => return Some(s.clone()),
                Value::Array(arr) => {
                    let mut combined = String::new();
                    for item in arr {
                        if let Some(s) = item.as_str() {
                            combined.push_str(s);
                        } else if let Some(text) = item.get("text").and_then(Value::as_str) {
                            combined.push_str(text);
                        } else if let Some(thinking) = item.get("thinking").and_then(Value::as_str)
                        {
                            combined.push_str(thinking);
                        }
                    }
                    if !combined.is_empty() {
                        return Some(combined);
                    }
                }
                _ => {}
            }
        }
    }
    if let Some(Value::Array(parts)) = &msg.content {
        let mut combined = String::new();
        for p in parts {
            let p_type = p.get("type").and_then(Value::as_str);
            if matches!(p_type, Some("thinking" | "reasoning")) {
                if let Some(t) = p.get("thinking").and_then(Value::as_str) {
                    combined.push_str(t);
                } else if let Some(t) = p.get("text").and_then(Value::as_str) {
                    combined.push_str(t);
                }
            }
        }
        if !combined.is_empty() {
            return Some(combined);
        }
    }
    None
}

pub(crate) fn sanitize_responses_reasoning_item(item: &mut Value) {
    let Some(map) = item.as_object_mut() else {
        return;
    };
    if map.get("type").and_then(Value::as_str) != Some("reasoning") {
        return;
    }
    let Some(content) = map.remove("content") else {
        return;
    };
    let summary_empty = map
        .get("summary")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty);
    if summary_empty {
        let mut text = String::new();
        if let Some(c_arr) = content.as_array() {
            for part in c_arr {
                if let Some(t) = part
                    .get("text")
                    .and_then(Value::as_str)
                    .or_else(|| part.as_str())
                {
                    text.push_str(t);
                }
            }
        } else if let Some(t) = content.as_str() {
            text.push_str(t);
        }
        if !text.is_empty() {
            map.insert(
                "summary".to_string(),
                json!([{ "type": "summary_text", "text": text }]),
            );
        }
    }
}

pub(crate) fn convert_single_message_to_responses_input(
    msg: &OpenAIMessage,
    input_items: &mut Vec<Value>,
) {
    if msg.role == "tool" {
        let call_id = msg.tool_call_id.as_deref().unwrap_or("call_xyz");
        let content_str = content_to_text(msg.content.as_ref());
        input_items.push(json!({
            "type": "function_call_output",
            "call_id": call_id,
            "output": content_str
        }));
        return;
    }

    if msg.role == "system" || msg.role == "developer" {
        let mut parts = convert_msg_content_to_parts(msg.content.as_ref(), "input_text");
        if parts.is_empty() {
            let text = content_to_text(msg.content.as_ref());
            if !text.is_empty() {
                parts.push(json!({ "type": "input_text", "text": text }));
            }
        }
        if !parts.is_empty() {
            input_items.push(json!({
                "role": "developer",
                "content": parts
            }));
        }
        return;
    }

    let has_tool_calls = msg.tool_calls.as_ref().is_some_and(|tc| !tc.is_empty());

    if msg.role == "assistant" {
        if let Some(reasoning_text) = extract_reasoning_content(msg) {
            input_items.push(json!({
                "type": "reasoning",
                "summary": [{
                    "type": "summary_text",
                    "text": reasoning_text
                }]
            }));
        }

        let parts = convert_msg_content_to_parts(msg.content.as_ref(), "output_text");
        if !parts.is_empty() {
            input_items.push(json!({
                "role": "assistant",
                "content": parts
            }));
        } else if !has_tool_calls {
            input_items.push(json!({
                "role": "assistant",
                "content": [json!({ "type": "output_text", "text": "" })]
            }));
        }
    } else {
        let mut parts = convert_msg_content_to_parts(msg.content.as_ref(), "input_text");
        if parts.is_empty() {
            parts.push(json!({ "type": "input_text", "text": "" }));
        }
        input_items.push(json!({
            "role": msg.role,
            "content": parts
        }));
    }

    if let Some(tool_calls) = &msg.tool_calls {
        convert_msg_tool_calls(tool_calls, input_items);
    }
}

pub(crate) fn messages_to_responses_input<M: crate::context::AsOpenAIMessage>(
    messages: &[M],
) -> Value {
    let mut input_items = Vec::new();
    for msg in messages {
        convert_single_message_to_responses_input(msg.as_message(), &mut input_items);
    }
    Value::Array(input_items)
}

pub(crate) fn content_to_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(value) => value.to_string(),
        None => String::new(),
    }
}

pub(crate) fn normalize_model_and_effort(model: &str) -> (String, Option<&'static str>) {
    for (suffix, effort) in [
        ("-xhigh", "xhigh"),
        ("-high", "high"),
        ("-medium", "medium"),
        ("-low", "low"),
        ("-none", "none"),
    ] {
        if let Some(base) = model.strip_suffix(suffix) {
            return (base.to_string(), Some(effort));
        }
    }
    (model.to_string(), None)
}

pub(crate) fn normalize_effort(value: &str) -> &'static str {
    match value {
        "max" | "xhigh" => "xhigh",
        "high" => "high",
        "low" => "low",
        "none" => "none",
        _ => "medium",
    }
}
