use crate::PipelineRequest;
use openproxy_types::error::CoreError;
use openproxy_types::models::Model;
use openproxy_types::{OpenAIMessage, OpenAIRequestView, TargetFormat};
use serde_json::Value;

pub mod responses;
pub use responses::ResponsesFormatter;

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
        if let Some(ref tools) = view.tools {
            let mut sanitized_tools: Vec<Value> = Vec::with_capacity(tools.len());
            for t in tools.iter() {
                let mut clean = t.clone();
                if let Some(obj) = clean.as_object_mut() {
                    obj.remove("cache_control");
                    if let Some(func) = obj.get_mut("function").and_then(|f| f.as_object_mut())
                        && let Some(params) = func.get_mut("parameters")
                    {
                        crate::schema_sanitizer::sanitize_tool_parameters_schema(params);
                    }
                }
                sanitized_tools.push(clean);
            }
            view.tools = Some(std::borrow::Cow::Owned(sanitized_tools));
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
            "cache_control",
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
    if m.extra.contains_key("cache_control") {
        return true;
    }
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
                    if t == "image_url" || t == "input_image" || t == "input_audio" || t == "image" {
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
    patched.extra.remove("cache_control");
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
                .is_some_and(|t| t == "image_url" || t == "input_image" || t == "input_audio" || t == "image")
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
        | TargetFormat::SystemOne => &OpenaiFormatter,
        TargetFormat::Anthropic => &AnthropicFormatter,
        TargetFormat::Gemini => &GEMINI_FORMATTER,
        TargetFormat::Responses => &responses::ResponsesFormatter,
    }
}

#[cfg(test)]
pub(crate) use responses::*;

#[cfg(test)]
pub(crate) use serde_json::json;

#[cfg(test)]
#[path = "../formatting_tests.rs"]
mod tests;
