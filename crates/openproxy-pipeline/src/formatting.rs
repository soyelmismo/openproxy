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
        if view.model.to_lowercase().contains("deepseek") {
            let needs_patch = view
                .messages
                .iter()
                .any(|m| m.role == "assistant" && !m.extra.contains_key("reasoning_content"));
            if needs_patch {
                let mut msgs = view.messages.into_owned();
                for msg in &mut msgs {
                    if msg.role == "assistant" && !msg.extra.contains_key("reasoning_content") {
                        msg.extra.insert(
                            "reasoning_content".to_string(),
                            serde_json::Value::String("".to_string()),
                        );
                    }
                }
                view.messages = std::borrow::Cow::Owned(msgs);
            }
        }
        if view.extra.contains_key("reasoning_effort") {
            let model_lower = view.model.to_lowercase();
            let is_reasoning = model_lower.starts_with("o1")
                || model_lower.starts_with("o3")
                || model_lower.starts_with("o4")
                || model_lower.contains("/o1")
                || model_lower.contains("/o3")
                || model_lower.contains("/o4");
            if !is_reasoning {
                let mut extra = view.extra.into_owned();
                extra.remove("reasoning_effort");
                view.extra = std::borrow::Cow::Owned(extra);
            }
        }
        adapter.normalize_openai_request(&mut view);
        match serde_json::to_vec(&view) {
            Ok(v) => Ok(bytes::Bytes::from(v)),
            Err(e) => Err(CoreError::Parse(format!("serialize openai request: {}", e))),
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
                "serialize anthropic request: {}",
                e
            ))),
        }
    }
}

pub struct GeminiFormatter;
impl TargetFormatter for GeminiFormatter {
    fn format_request(
        &self,
        req: &PipelineRequest,
        model: &Model,
        messages_ref: &[OpenAIMessage],
        stream: bool,
        adapter: &openproxy_adapters::adapters::ProviderAdapterEnum,
    ) -> Result<bytes::Bytes, CoreError> {
        adapter.format_request(
            TargetFormat::Gemini,
            &req.openai_request,
            &model.model_id,
            messages_ref,
            stream,
        )
    }
}

pub fn get_formatter(target_format: TargetFormat) -> Box<dyn TargetFormatter> {
    macro_rules! match_formatter {
        ($($variant:ident => $formatter:ident),* $(,)?) => {
            match target_format {
                $(TargetFormat::$variant => Box::new($formatter),)*
            }
        };
    }
    match_formatter! {
        Openai => OpenaiFormatter,
        Anthropic => AnthropicFormatter,
        Gemini => GeminiFormatter,
        Responses => ResponsesFormatter,
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

        let mut system_instructions = None;
        let mut messages_without_system = Vec::new();
        for msg in messages_ref {
            if msg.role == "system" && system_instructions.is_none() {
                system_instructions = Some(content_to_text(msg.content.as_ref()));
            } else {
                messages_without_system.push(msg);
            }
        }

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
        if let Some(tools) = &req.openai_request.tools {
            let mut flat_tools = Vec::new();
            for tool in tools {
                let mut flat_tool = tool.clone();
                if let Some(obj) = flat_tool.as_object_mut() {
                    let is_function = obj.get("type").and_then(|v| v.as_str()) == Some("function");
                    if is_function
                        && let Some(func) = obj.remove("function")
                        && let Some(func_obj) = func.as_object()
                    {
                        if let Some(name) = func_obj.get("name") {
                            obj.insert("name".to_string(), name.clone());
                        }
                        if let Some(desc) = func_obj.get("description") {
                            obj.insert("description".to_string(), desc.clone());
                        }
                        if let Some(params) = func_obj.get("parameters") {
                            obj.insert("parameters".to_string(), params.clone());
                        }
                    }
                }
                flat_tools.push(flat_tool);
            }
            obj.insert("tools".to_string(), Value::Array(flat_tools));
        }
        if let Some(tool_choice) = &req.openai_request.tool_choice {
            let mut flat_choice = tool_choice.clone();
            if let Some(obj) = flat_choice.as_object_mut()
                && obj.get("type").and_then(|v| v.as_str()) == Some("function")
                && let Some(func) = obj.remove("function")
                && let Some(func_obj) = func.as_object()
                && let Some(name) = func_obj.get("name")
            {
                obj.insert("name".to_string(), name.clone());
            }
            obj.insert("tool_choice".to_string(), flat_choice);
        }

        // Codex Responses API strict schema: strip Chat Completions parameters that cause 400s
        obj.remove("max_tokens");
        obj.remove("max_output_tokens");
        obj.remove("truncation");
        obj.remove("background");
        obj.remove("prompt_cache_retention");
        obj.remove("safety_identifier");
        obj.remove("user");
        obj.remove("stream_options");

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

        let instructions_str = obj
            .get("instructions")
            .and_then(|v| v.as_str())
            .unwrap_or("Follow the developer instructions in the conversation.");

        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(instructions_str.as_bytes());
        if let Some(tools) = &req.openai_request.tools
            && let Ok(tools_str) = serde_json::to_string(tools)
        {
            hasher.update(tools_str.as_bytes());
        }
        let hash_hex = hex::encode(hasher.finalize());
        obj.insert(
            "prompt_cache_key".to_string(),
            Value::String(format!("pck_{}", &hash_hex[..24])),
        );

        match serde_json::to_vec(&Value::Object(obj)) {
            Ok(v) => Ok(bytes::Bytes::from(v)),
            Err(e) => Err(CoreError::Parse(format!(
                "serialize responses request: {}",
                e
            ))),
        }
    }
}

fn messages_to_responses_input(messages: &[&OpenAIMessage]) -> Value {
    let mut input_items = Vec::new();

    for msg in messages {
        if msg.role == "tool" {
            let call_id = msg
                .tool_call_id
                .clone()
                .unwrap_or_else(|| "call_xyz".to_string());
            let content_str = content_to_text(msg.content.as_ref());
            input_items.push(json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": content_str
            }));
            continue;
        }

        let text_type = if msg.role == "assistant" {
            "output_text"
        } else {
            "input_text"
        };

        let mut parts = Vec::new();
        match &msg.content {
            Some(Value::String(text)) => {
                parts.push(json!({ "type": text_type, "text": text }));
            }
            Some(Value::Array(arr)) => {
                for item in arr {
                    let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("text");
                    if item_type == "text"
                        || item_type == "input_text"
                        || item_type == "output_text"
                    {
                        let text = item.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        parts.push(json!({ "type": text_type, "text": text }));
                    } else if item_type == "image_url" {
                        if let Some(url_obj) = item.get("image_url").and_then(|v| v.as_object()) {
                            let url = url_obj.get("url").and_then(|v| v.as_str()).unwrap_or("");
                            if url.starts_with("data:image/") {
                                let parts_url: Vec<&str> = url.splitn(2, ',').collect();
                                if parts_url.len() == 2 {
                                    let mime = parts_url[0]
                                        .strip_prefix("data:")
                                        .and_then(|s| s.strip_suffix(";base64"))
                                        .unwrap_or("image/jpeg");
                                    parts.push(json!({
                                        "type": "input_image",
                                        "image": parts_url[1],
                                        "mime_type": mime
                                    }));
                                }
                            } else {
                                parts.push(json!({
                                    "type": "input_image",
                                    "image_url": url
                                }));
                            }
                        }
                    } else if item_type == "image"
                        && let Some(source) = item.get("source").and_then(|v| v.as_object())
                    {
                        let data = source.get("data").and_then(|v| v.as_str()).unwrap_or("");
                        let media_type = source
                            .get("media_type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("image/jpeg");
                        parts.push(json!({
                            "type": "input_image",
                            "image": data,
                            "mime_type": media_type
                        }));
                    }
                }
            }
            Some(value) => {
                parts.push(json!({ "type": text_type, "text": value.to_string() }));
            }
            None => {
                parts.push(json!({ "type": text_type, "text": "" }));
            }
        }

        input_items.push(json!({
            "role": msg.role,
            "content": parts
        }));

        if let Some(tool_calls) = &msg.tool_calls {
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
        "medium" => "medium",
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

    fn make_test_model(provider: &str, model: &str) -> Model {
        Model {
            row_id: openproxy_types::ids::ModelRowId(1),
            provider_id: openproxy_types::ids::ProviderId::new(provider),
            model_id: openproxy_types::ids::ModelId::new(model),
            display_name: None,
            target_format: TargetFormat::Openai,
            discovered_at: String::new(),
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
        }
    }

    #[test]
    fn test_openai_formatter_deepseek_reasoning_content() {
        let formatter = OpenaiFormatter;
        let (mut req, _rx) = crate::test_utils::make_request(openproxy_types::ids::ComboId(1));
        let mut openai_req = (*req.openai_request).clone();
        openai_req.model = "deepseek/deepseek-v4-flash-free".to_string();
        openai_req.messages = vec![
            OpenAIMessage {
                role: "user".to_string(),
                content: Some(Value::String("hello".to_string())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: serde_json::Map::new(),
            },
            OpenAIMessage {
                role: "assistant".to_string(),
                content: Some(Value::String("hi".to_string())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: serde_json::Map::new(),
            },
        ];
        req.openai_request = std::sync::Arc::new(openai_req);
        let model = make_test_model("zenmux", "deepseek/deepseek-v4-flash-free");
        let mock = crate::test_utils::MockAdapter::new(
            "zenmux",
            "http://127.0.0.1:8080".to_string(),
            openproxy_adapters::adapters::AdapterFormat::Openai,
        );
        let adapter = openproxy_adapters::adapters::ProviderAdapterEnum::Mock(mock);
        let bytes = formatter
            .format_request(&req, &model, &req.openai_request.messages, false, &adapter)
            .expect("format request");
        let parsed: Value = serde_json::from_slice(&bytes).expect("parse json");
        let msgs = parsed["messages"].as_array().expect("messages array");
        assert_eq!(msgs[1]["reasoning_content"], "");
    }

    #[test]
    fn test_openai_formatter_strips_unsupported_reasoning_effort() {
        let formatter = OpenaiFormatter;
        let (mut req, _rx) = crate::test_utils::make_request(openproxy_types::ids::ComboId(1));
        let mut openai_req = (*req.openai_request).clone();
        openai_req.model = "moonshotai/kimi-k3-free".to_string();
        openai_req.extra.insert(
            "reasoning_effort".to_string(),
            Value::String("high".to_string()),
        );
        req.openai_request = std::sync::Arc::new(openai_req);
        let model = make_test_model("tokenrouter", "moonshotai/kimi-k3-free");
        let mock = crate::test_utils::MockAdapter::new(
            "tokenrouter",
            "http://127.0.0.1:8080".to_string(),
            openproxy_adapters::adapters::AdapterFormat::Openai,
        );
        let adapter = openproxy_adapters::adapters::ProviderAdapterEnum::Mock(mock);
        let bytes = formatter
            .format_request(&req, &model, &req.openai_request.messages, false, &adapter)
            .expect("format request");
        let parsed: Value = serde_json::from_slice(&bytes).expect("parse json");
        assert!(parsed.get("reasoning_effort").is_none());
    }
}
