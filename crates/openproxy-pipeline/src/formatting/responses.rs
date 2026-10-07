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
        if let Some(obj) = flat_tool.as_object_mut() {
            let mut strict_val = obj.remove("strict");
            if obj.get("type").and_then(|v| v.as_str()) == Some("function")
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
                if let Some(st) = func_obj.remove("strict") {
                    strict_val = Some(st);
                }
            } else if let Some(params) = obj.get_mut("parameters") {
                crate::schema_sanitizer::sanitize_tool_parameters_schema(params);
            }
            // Codex / OpenAI Responses parity: function tools specify strict: false
            // so dynamic and MCP schemas do not fail OpenAI structured-outputs validation.
            let strict = strict_val.unwrap_or(Value::Bool(false));
            obj.insert("strict".to_string(), strict);
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
    let is_meta_muse = obj
        .get("model")
        .and_then(Value::as_str)
        .is_some_and(|m| m.contains("muse"));
    if is_meta_muse {
        if !obj.contains_key("include") {
            obj.insert(
                "include".to_string(),
                json!(["reasoning.encrypted_content"]),
            );
        }
        if !obj.contains_key("reasoning") {
            let eff = effort.unwrap_or("medium");
            obj.insert(
                "reasoning".to_string(),
                json!({
                    "effort": eff,
                    "summary": "auto"
                }),
            );
        }
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
        let has_encrypted = msg
            .extra
            .get("reasoning_encrypted_content")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty());
        let reasoning_text = extract_reasoning_content(msg);

        if has_encrypted || reasoning_text.is_some() || msg.extra.contains_key("reasoning_summary")
        {
            let mut r_item = serde_json::Map::new();
            r_item.insert("type".to_string(), Value::String("reasoning".to_string()));
            if let Some(id) = msg.extra.get("reasoning_id").and_then(Value::as_str) {
                r_item.insert("id".to_string(), Value::String(id.to_string()));
            }
            if let Some(enc) = msg
                .extra
                .get("reasoning_encrypted_content")
                .and_then(Value::as_str)
            {
                r_item.insert(
                    "encrypted_content".to_string(),
                    Value::String(enc.to_string()),
                );
            }
            if let Some(sum) = msg.extra.get("reasoning_summary") {
                r_item.insert("summary".to_string(), sum.clone());
            } else if let Some(text) = reasoning_text {
                r_item.insert(
                    "summary".to_string(),
                    json!([{
                        "type": "summary_text",
                        "text": text
                    }]),
                );
            } else if !r_item.contains_key("encrypted_content") {
                r_item.insert("summary".to_string(), Value::Array(Vec::new()));
            }
            input_items.push(Value::Object(r_item));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_responses_tools_preserves_and_defaults_strict_false() {
        let tools = vec![
            json!({
                "type": "function",
                "function": {
                    "name": "bash",
                    "description": "Execute shell command",
                    "parameters": { "type": "object" }
                }
            }),
            json!({
                "type": "function",
                "name": "read",
                "description": "Read file",
                "parameters": { "type": "object" },
                "strict": true
            }),
            json!({
                "type": "function",
                "name": "write",
                "description": "Write file",
                "parameters": { "type": "object" }
            }),
        ];

        let formatted = format_responses_tools(Some(&tools)).expect("formatted tools");
        let arr = formatted.as_array().expect("array");
        assert_eq!(arr.len(), 3);

        // Nested function tool defaults strict to false
        assert_eq!(arr[0]["name"], "bash");
        assert_eq!(arr[0]["strict"], false);

        // Flat function tool with strict: true preserves it
        assert_eq!(arr[1]["name"], "read");
        assert_eq!(arr[1]["strict"], true);

        // Flat function tool without strict defaults to false
        assert_eq!(arr[2]["name"], "write");
        assert_eq!(arr[2]["strict"], false);
    }

    #[test]
    fn test_messages_to_responses_input_preserves_encrypted_reasoning() {
        let mut extra = serde_json::Map::new();
        extra.insert(
            "reasoning_encrypted_content".to_string(),
            Value::String("gAAAAABsecret_token".to_string()),
        );
        extra.insert(
            "reasoning_id".to_string(),
            Value::String("rs_abc123".to_string()),
        );
        extra.insert(
            "reasoning_summary".to_string(),
            json!([{ "type": "summary_text", "text": "Plan: read then edit" }]),
        );

        let messages = vec![
            OpenAIMessage {
                role: "assistant".to_string(),
                content: Some(Value::String("Calling read tool...".to_string())),
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![json!({
                    "id": "call_1",
                    "type": "function",
                    "function": { "name": "read", "arguments": "{\"path\":\"foo.txt\"}" }
                })]),
                extra,
            },
            OpenAIMessage {
                role: "tool".to_string(),
                content: Some(Value::String("file content".to_string())),
                name: None,
                tool_call_id: Some("call_1".to_string()),
                tool_calls: None,
                extra: serde_json::Map::new(),
            },
        ];

        let input_val = messages_to_responses_input(&messages);
        let items = input_val.as_array().expect("items array");

        // First item should be the reconstructed reasoning block
        let r_item = &items[0];
        assert_eq!(r_item["type"], "reasoning");
        assert_eq!(r_item["id"], "rs_abc123");
        assert_eq!(r_item["encrypted_content"], "gAAAAABsecret_token");
        assert_eq!(r_item["summary"][0]["text"], "Plan: read then edit");

        // Next item is assistant message
        assert_eq!(items[1]["role"], "assistant");

        // Next item is function_call
        assert_eq!(items[2]["type"], "function_call");
        assert_eq!(items[2]["call_id"], "call_1");

        // Next item is function_call_output
        assert_eq!(items[3]["type"], "function_call_output");
        assert_eq!(items[3]["call_id"], "call_1");
    }

    #[test]
    fn test_apply_responses_reasoning_and_tier_sets_include_for_muse() {
        let mut obj = serde_json::Map::new();
        obj.insert(
            "model".to_string(),
            Value::String("muse-spark-1.3-contributor-free".to_string()),
        );

        apply_responses_reasoning_and_tier(&mut obj, None);

        assert_eq!(obj["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(obj["reasoning"]["summary"], "auto");
        assert_eq!(obj["reasoning"]["effort"], "medium");
    }
}
