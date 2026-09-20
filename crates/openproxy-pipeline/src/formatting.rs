use crate::PipelineRequest;
use openproxy_types::error::CoreError;
use openproxy_types::models::Model;
use openproxy_types::{OpenAIMessage, OpenAIRequestView, TargetFormat};
use serde_json::{Value, json};

pub trait TargetFormatter: Send + Sync {
    fn format_request(
        &self,
        req: &PipelineRequest,
        model: &Model,
        messages_ref: &[OpenAIMessage],
        stream: bool,
        adapter: &openproxy_adapters::adapters::ProviderAdapterEnum,
    ) -> Result<bytes::Bytes, CoreError>;
}

pub struct OpenaiFormatter;
impl TargetFormatter for OpenaiFormatter {
    fn format_request(
        &self,
        req: &PipelineRequest,
        model: &Model,
        messages_ref: &[OpenAIMessage],
        stream: bool,
        adapter: &openproxy_adapters::adapters::ProviderAdapterEnum,
    ) -> Result<bytes::Bytes, CoreError> {
        let mut view = OpenAIRequestView::new(
            &req.openai_request,
            model.model_id.as_str(),
            messages_ref,
            stream,
        );
        let needs_normalization = view.messages.iter().any(message_needs_openai_normalization);
        if needs_normalization {
            view.messages = std::borrow::Cow::Owned(
                view.messages.iter().map(normalize_openai_message).collect(),
            );
        }
        const OPENAI_CHAT_DISALLOWED_EXTRA: &[&str] = &[
            "disabled",
            "prompt_cache_key",
            "prompt_cache_retention",
            "instructions",
            "input",
            "previous_response_id",
            "store",
            "background",
            "truncation",
        ];
        for key in OPENAI_CHAT_DISALLOWED_EXTRA {
            if view.extra.contains_key(*key) {
                view.extra.to_mut().remove(*key);
            }
        }
        adapter.normalize_openai_request(&mut view);
        match serde_json::to_vec(&view) {
            Ok(v) => Ok(bytes::Bytes::from(v)),
            Err(e) => Err(CoreError::Parse(format!("serialize openai request: {e}"))),
        }
    }
}

fn message_needs_openai_normalization(m: &OpenAIMessage) -> bool {
    if m.role == "developer" {
        return true;
    }
    if m.role == "tool" && m.name.is_some() {
        return true;
    }
    if m.name.as_deref().is_some_and(|n| {
        n.is_empty()
            || n.len() > 64
            || !n
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    }) {
        return true;
    }
    if matches!(m.role.as_str(), "assistant" | "system" | "tool")
        && matches!(&m.content, Some(Value::Array(_) | Value::Object(_)))
    {
        return true;
    }
    if let Some(Value::Array(parts)) = &m.content {
        let mut has_media = false;
        for p in parts {
            if let Some(obj) = p.as_object() {
                if obj.contains_key("annotations") {
                    return true;
                }
                if let Some(t) = obj.get("type").and_then(|v| v.as_str()) {
                    if t == "output_text" || t == "input_text" {
                        return true;
                    }
                    if t == "image_url" || t == "input_audio" || t == "image" {
                        has_media = true;
                    }
                }
            }
        }
        if !has_media {
            return true;
        }
    }
    false
}

fn normalize_openai_message(m: &OpenAIMessage) -> OpenAIMessage {
    let mut patched = m.clone();
    if patched.role == "developer" {
        patched.role = "system".to_string();
    }
    patched.sanitize_name();

    if matches!(patched.role.as_str(), "assistant" | "system" | "tool")
        && matches!(&patched.content, Some(Value::Array(_) | Value::Object(_)))
    {
        patched.content = Some(Value::String(patched.extract_text()));
    } else if let Some(Value::Array(parts)) = &patched.content {
        let has_media = parts.iter().any(|p| {
            p.get("type")
                .and_then(|v| v.as_str())
                .is_some_and(|t| t == "image_url" || t == "input_audio" || t == "image")
        });
        if !has_media {
            patched.content = Some(Value::String(patched.extract_text()));
        } else {
            let sanitized_parts = parts
                .iter()
                .map(|p| {
                    if let Some(obj) = p.as_object() {
                        let mut new_obj = obj.clone();
                        new_obj.remove("annotations");
                        if let Some(t) = new_obj.get("type").and_then(|v| v.as_str())
                            && (t == "output_text" || t == "input_text")
                        {
                            new_obj.insert("type".to_string(), Value::String("text".to_string()));
                        }
                        if !new_obj.contains_key("text")
                            && let Some(cnt) = new_obj.remove("content")
                        {
                            new_obj.insert("text".to_string(), cnt);
                        }
                        Value::Object(new_obj)
                    } else {
                        p.clone()
                    }
                })
                .collect();
            patched.content = Some(Value::Array(sanitized_parts));
        }
    }
    patched
}

pub struct AnthropicFormatter;
impl TargetFormatter for AnthropicFormatter {
    fn format_request(
        &self,
        req: &PipelineRequest,
        model: &Model,
        messages_ref: &[OpenAIMessage],
        stream: bool,
        _adapter: &openproxy_adapters::adapters::ProviderAdapterEnum,
    ) -> Result<bytes::Bytes, CoreError> {
        let anthro = crate::translation::openai_to_anthropic(
            &req.openai_request,
            model.model_id.as_str(),
            messages_ref,
            stream,
        );
        match serde_json::to_vec(&anthro) {
            Ok(v) => Ok(bytes::Bytes::from(v)),
            Err(e) => Err(CoreError::Parse(format!(
                "serialize anthropic request: {e}"
            ))),
        }
    }
}

pub struct GenericFormatter {
    pub format_spec: TargetFormat,
}

impl TargetFormatter for GenericFormatter {
    fn format_request(
        &self,
        req: &PipelineRequest,
        model: &Model,
        messages_ref: &[OpenAIMessage],
        stream: bool,
        adapter: &openproxy_adapters::adapters::ProviderAdapterEnum,
    ) -> Result<bytes::Bytes, CoreError> {
        adapter.format_request(
            self.format_spec,
            &req.openai_request,
            &model.model_id,
            messages_ref,
            stream,
        )
    }
}

static GEMINI_FORMATTER: GenericFormatter = GenericFormatter {
    format_spec: TargetFormat::Gemini,
};

pub fn get_formatter(target_format: TargetFormat) -> &'static dyn TargetFormatter {
    match target_format {
        TargetFormat::Openai
        | TargetFormat::Atomesus
        | TargetFormat::CommandCodeGo
        | TargetFormat::SystemOne => {
            &OpenaiFormatter
        }
        TargetFormat::Anthropic => &AnthropicFormatter,
        TargetFormat::Gemini => &GEMINI_FORMATTER,
        TargetFormat::Responses => &ResponsesFormatter,
    }
}

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

        let (system_instructions, messages_without_system) =
            extract_system_and_messages(messages_ref);

        obj.insert(
            "input".to_string(),
            messages_to_responses_input(&messages_without_system),
        );
        obj.insert("stream".to_string(), Value::Bool(stream));
        obj.insert("store".to_string(), Value::Bool(false));

        let default_instructions =
            "Follow the developer instructions in the conversation.".to_string();
        obj.entry("instructions".to_string())
            .or_insert_with(|| Value::String(system_instructions.unwrap_or(default_instructions)));

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
        apply_responses_reasoning_and_tier(&mut obj, effort_from_model);

        let instructions_str = obj
            .get("instructions")
            .and_then(|v| v.as_str())
            .unwrap_or("Follow the developer instructions in the conversation.");
        let pck = compute_responses_prompt_cache_key(
            instructions_str,
            req.openai_request.tools.as_deref(),
        );
        obj.insert("prompt_cache_key".to_string(), Value::String(pck));

        serde_json::to_vec(&Value::Object(obj))
            .map(bytes::Bytes::from)
            .map_err(|e| CoreError::Parse(format!("serialize responses request: {e}")))
    }
}

fn extract_system_and_messages(
    messages_ref: &[OpenAIMessage],
) -> (Option<String>, Vec<&OpenAIMessage>) {
    let mut instructions_parts = Vec::new();
    let mut messages_without_system = Vec::new();
    let mut leading_system = true;

    for msg in messages_ref {
        if msg.role == "system" && leading_system {
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

fn format_responses_tools(tools: Option<&[Value]>) -> Option<Value> {
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
            if let Some(params) = func_obj.remove("parameters") {
                obj.insert("parameters".to_string(), params);
            }
        }
        flat_tools.push(flat_tool);
    }
    Some(Value::Array(flat_tools))
}

fn format_responses_tool_choice(tool_choice: Option<&Value>) -> Option<Value> {
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

fn strip_responses_disallowed_keys(obj: &mut serde_json::Map<String, Value>) {
    const DISALLOWED: &[&str] = &[
        "max_tokens",
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

fn apply_responses_reasoning_and_tier(
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

fn compute_responses_prompt_cache_key(instructions_str: &str, tools: Option<&[Value]>) -> String {
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

fn parse_data_image_url(url: &str) -> Value {
    if let Some((mime_part, data_part)) = url.split_once(',') {
        let mime = mime_part
            .strip_prefix("data:")
            .and_then(|s| s.strip_suffix(";base64"))
            .unwrap_or("image/jpeg");
        json!({
            "type": "input_image",
            "image": data_part,
            "mime_type": mime
        })
    } else {
        json!({
            "type": "input_image",
            "image_url": url
        })
    }
}

fn convert_image_url_part(item: &Value) -> Option<Value> {
    let url_obj = item.get("image_url")?.as_object()?;
    let url = url_obj.get("url")?.as_str().unwrap_or("");
    if url.starts_with("data:image/") {
        Some(parse_data_image_url(url))
    } else {
        Some(json!({
            "type": "input_image",
            "image_url": url
        }))
    }
}

fn convert_image_source_part(item: &Value) -> Option<Value> {
    let source = item.get("source")?.as_object()?;
    let data = source.get("data").and_then(|v| v.as_str()).unwrap_or("");
    let media_type = source
        .get("media_type")
        .and_then(|v| v.as_str())
        .unwrap_or("image/jpeg");
    Some(json!({
        "type": "input_image",
        "image": data,
        "mime_type": media_type
    }))
}

fn convert_content_item_to_part(item: &Value, text_type: &str) -> Option<Value> {
    let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("text");
    match item_type {
        "text" | "input_text" | "output_text" => {
            let text = item.get("text").and_then(|v| v.as_str()).unwrap_or("");
            Some(json!({ "type": text_type, "text": text }))
        }
        "image_url" => convert_image_url_part(item),
        "image" => convert_image_source_part(item),
        _ => None,
    }
}

fn convert_msg_content_to_parts(content: Option<&Value>, text_type: &str) -> Vec<Value> {
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

fn convert_msg_tool_calls(tool_calls: &[Value], input_items: &mut Vec<Value>) {
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

fn convert_single_message_to_responses_input(msg: &OpenAIMessage, input_items: &mut Vec<Value>) {
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

    if msg.role == "system" {
        input_items.push(json!({
            "role": "system",
            "content": content_to_text(msg.content.as_ref())
        }));
        return;
    }

    let has_tool_calls = msg.tool_calls.as_ref().is_some_and(|tc| !tc.is_empty());

    if msg.role == "assistant" {
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

fn messages_to_responses_input(messages: &[&OpenAIMessage]) -> Value {
    let mut input_items = Vec::new();
    for msg in messages {
        convert_single_message_to_responses_input(msg, &mut input_items);
    }
    Value::Array(input_items)
}

fn content_to_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(value) => value.to_string(),
        None => String::new(),
    }
}

fn normalize_model_and_effort(model: &str) -> (String, Option<&'static str>) {
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

fn normalize_effort(value: &str) -> &'static str {
    match value {
        "max" | "xhigh" => "xhigh",
        "high" => "high",
        "low" => "low",
        "none" => "none",
        _ => "medium",
    }
}

#[cfg(test)]
#[path = "formatting_tests.rs"]
mod tests;
