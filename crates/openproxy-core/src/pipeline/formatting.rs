use crate::adapters::ProviderAdapter;
use crate::error::CoreError;
use crate::models::{Model, TargetFormat};
use crate::pipeline::PipelineRequest;
use crate::translation::OpenAIMessage;
use serde_json::{Value, json};

pub trait TargetFormatter: Send + Sync {
    fn format_request(
        &self,
        req: &PipelineRequest,
        model: &Model,
        messages_ref: &[OpenAIMessage],
        stream: bool,
        adapter: &dyn ProviderAdapter,
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
        adapter: &dyn ProviderAdapter,
    ) -> Result<bytes::Bytes, CoreError> {
        let mut view = crate::translation::OpenAIRequestView::new(
            &req.openai_request,
            model.model_id.as_str(),
            messages_ref,
            stream,
        );
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
        _adapter: &dyn ProviderAdapter,
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
        _model: &Model,
        messages_ref: &[OpenAIMessage],
        _stream: bool,
        _adapter: &dyn ProviderAdapter,
    ) -> Result<bytes::Bytes, CoreError> {
        let gemini = crate::translation::openai_to_gemini(&req.openai_request, messages_ref);
        match serde_json::to_vec(&gemini) {
            Ok(v) => Ok(bytes::Bytes::from(v)),
            Err(e) => Err(CoreError::Parse(format!("serialize gemini request: {}", e))),
        }
    }
}

pub fn get_formatter(target_format: TargetFormat) -> Box<dyn TargetFormatter> {
    match target_format {
        TargetFormat::Openai => Box::new(OpenaiFormatter),
        TargetFormat::Anthropic => Box::new(AnthropicFormatter),
        TargetFormat::Gemini => Box::new(GeminiFormatter),
        TargetFormat::Responses => Box::new(ResponsesFormatter),
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
        _adapter: &dyn ProviderAdapter,
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

        if let Some(max_tokens) = req.openai_request.max_tokens {
            obj.insert("max_output_tokens".to_string(), json!(max_tokens));
        }
        if let Some(temperature) = req.openai_request.temperature {
            obj.insert("temperature".to_string(), json!(temperature));
        }
        if let Some(top_p) = req.openai_request.top_p {
            obj.insert("top_p".to_string(), json!(top_p));
        }
        if let Some(tools) = &req.openai_request.tools {
            obj.insert("tools".to_string(), Value::Array(tools.clone()));
        }
        if let Some(tool_choice) = &req.openai_request.tool_choice {
            obj.insert("tool_choice".to_string(), tool_choice.clone());
        }

        let effort = req
            .openai_request
            .extra
            .get("reasoning_effort")
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

        let mut parts = Vec::new();
        match &msg.content {
            Some(Value::String(text)) => {
                parts.push(json!({ "type": "input_text", "text": text }));
            }
            Some(Value::Array(arr)) => {
                for item in arr {
                    let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("text");
                    if item_type == "text" {
                        let text = item.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        parts.push(json!({ "type": "input_text", "text": text }));
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
                parts.push(json!({ "type": "input_text", "text": value.to_string() }));
            }
            None => {
                parts.push(json!({ "type": "input_text", "text": "" }));
            }
        }

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

                parts.push(json!({
                    "type": "function_call",
                    "id": call_id,
                    "name": func_name,
                    "arguments": func_args
                }));
            }
        }

        input_items.push(json!({
            "role": msg.role,
            "content": parts
        }));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::translation::OpenAIMessage;

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
