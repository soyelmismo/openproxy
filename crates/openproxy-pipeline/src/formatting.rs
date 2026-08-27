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
        // "developer" role is only valid for native OpenAI; normalize to
        // "system" so every OpenAI-compatible upstream accepts it.
        if view.messages.iter().any(|m| m.role == "developer") {
            view.messages = std::borrow::Cow::Owned(
                view.messages
                    .iter()
                    .map(|m| {
                        if m.role == "developer" {
                            let mut patched = m.clone();
                            patched.role = "system".to_string();
                            patched
                        } else {
                            m.clone()
                        }
                    })
                    .collect(),
            );
        }
        if view.extra.contains_key("disabled") {
            view.extra.to_mut().remove("disabled");
        }
        adapter.normalize_openai_request(&mut view);
        match serde_json::to_vec(&view) {
            Ok(v) => Ok(bytes::Bytes::from(v)),
            Err(e) => Err(CoreError::Parse(format!("serialize openai request: {e}"))),
        }
    }
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
static FX_FORMATTER: GenericFormatter = GenericFormatter {
    format_spec: TargetFormat::Fx,
};

pub fn get_formatter(target_format: TargetFormat) -> &'static dyn TargetFormatter {
    match target_format {
        TargetFormat::Openai | TargetFormat::Atomesus => &OpenaiFormatter,
        TargetFormat::Anthropic => &AnthropicFormatter,
        TargetFormat::Gemini => &GEMINI_FORMATTER,
        TargetFormat::Responses => &ResponsesFormatter,
        TargetFormat::Fx => &FX_FORMATTER,
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
        if let Some(choice) = format_responses_tool_choice(req.openai_request.tool_choice.as_ref()) {
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
    let mut system_instructions = None;
    let mut messages_without_system = Vec::new();
    for msg in messages_ref {
        if msg.role == "system" && system_instructions.is_none() {
            system_instructions = Some(content_to_text(msg.content.as_ref()));
        } else {
            messages_without_system.push(msg);
        }
    }
    (system_instructions, messages_without_system)
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

fn compute_responses_prompt_cache_key(
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
        Some(Value::String(text)) => vec![json!({ "type": text_type, "text": text })],
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|item| convert_content_item_to_part(item, text_type))
            .collect(),
        Some(value) => vec![json!({ "type": text_type, "text": value.to_string() })],
        None => vec![json!({ "type": text_type, "text": "" })],
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

    let text_type = if msg.role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };

    let parts = convert_msg_content_to_parts(msg.content.as_ref(), text_type);
    input_items.push(json!({
        "role": msg.role,
        "content": parts
    }));

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
mod tests {
    use super::*;
    use openproxy_types::OpenAIMessage;

    #[test]
    fn responses_input_does_not_emit_legacy_item_type() {
        let user = OpenAIMessage {
            role: "user".to_string(),
            content: Some(Value::String("ping".to_string())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::new(),
        };
        let tool = OpenAIMessage {
            role: "tool".to_string(),
            content: Some(Value::String("pong".to_string())),
            name: None,
            tool_call_id: Some("call_1".to_string()),
            tool_calls: None,
            extra: serde_json::Map::new(),
        };
        let input = messages_to_responses_input(&[&user, &tool]);
        let items = input.as_array().expect("input array");

        assert_eq!(items[0].get("type"), None);
        assert_eq!(
            items[1].get("type").and_then(Value::as_str),
            Some("function_call_output")
        );
    }

    #[test]
    fn normalize_effort_returns_expected() {
        assert_eq!(super::normalize_effort("max"), "xhigh");
        assert_eq!(super::normalize_effort("xhigh"), "xhigh");
        assert_eq!(super::normalize_effort("high"), "high");
        assert_eq!(super::normalize_effort("medium"), "medium");
        assert_eq!(super::normalize_effort("low"), "low");
        assert_eq!(super::normalize_effort("none"), "none");
        assert_eq!(super::normalize_effort("unknown"), "medium");
        assert_eq!(super::normalize_effort(""), "medium");
    }

    #[test]
    fn test_openai_formatter_strips_disabled() {
        use openproxy_adapters::adapters::nvidia_nim::NvidiaNimAdapter;
        use openproxy_adapters::adapters::ProviderAdapterEnum;
        use openproxy_types::models::Model;
        use openproxy_types::ModelId;
        use openproxy_types::ModelRowId;
        use openproxy_types::OpenAIRequest;
        use openproxy_types::ProviderId;
        use openproxy_types::TargetFormat;
        use std::sync::Arc;

        let adapter = ProviderAdapterEnum::NvidiaNim(NvidiaNimAdapter::new());
        let mut extra = serde_json::Map::new();
        extra.insert("disabled".to_string(), json!(true));
        extra.insert("custom_val".to_string(), json!("ok"));

        let openai_req = OpenAIRequest {
            model: "test-model".to_string(),
            messages: vec![OpenAIMessage {
                role: "user".to_string(),
                content: Some(json!("hello")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: serde_json::Map::new(),
            }],
            extra,
            ..Default::default()
        };

        let req = PipelineRequest {
            request_id: openproxy_types::RequestId::new(),
            trace_id: openproxy_types::TraceId::new(),
            combo_id: openproxy_types::ComboId(1),
            openai_request: Arc::new(openai_req.clone()),
            client_disconnected: tokio::sync::watch::channel(None).1,
            stream_sink: None,
            api_key_id: None,
            combo_override: None,
            targets_override: None,
            request_headers: std::collections::BTreeMap::new(),
            request_body_json: None,
            race_cancelled: false,
            race_cancel: None,
            endpoint_kind: openproxy_types::endpoint::EndpointKind::Chat,
            compressed_messages: Arc::new(std::sync::OnceLock::new()),
            proxy_override: None,
        };

        let model = Model {
            row_id: ModelRowId(1),
            provider_id: ProviderId::new("nvidia-nim"),
            model_id: ModelId::new("deepseek-ai/deepseek-v4-flash-0731"),
            display_name: Some("test".to_string()),
            target_format: TargetFormat::Openai,
            discovered_at: "2026-01-01T00:00:00Z".to_string(),
            expires_at: None,
            timeout_overrides_json: None,
            active: true,
            last_test_status: None,
            last_test_at: None,
            custom: false,
            context_length: None,
            max_output_tokens: None,
            capabilities_json: None,
            family: None,
            model_type: "chat".to_string(),
            input_modalities_json: None,
            output_modalities_json: None,
        };

        let formatter = OpenaiFormatter;
        let formatted = formatter
            .format_request(&req, &model, &openai_req.messages, true, &adapter)
            .expect("formatting must succeed");

        let json_val: Value = serde_json::from_slice(&formatted).unwrap();
        assert!(json_val.get("disabled").is_none());
        assert_eq!(json_val.get("custom_val"), Some(&json!("ok")));
    }
}
