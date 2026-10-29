use crate::PipelineRequest;
use openproxy_types::ResultExt;
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
                }
                crate::schema_sanitizer::sanitize_tool_parameters(&mut clean);
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
        // If the client omitted max_tokens/max_completion_tokens, raise it to the
        // model's declared output ceiling (or combo's effective ceiling) so backends
        // don't truncate on small 4096 defaults.
        let client_has_max = req.openai_request.max_tokens.is_some()
            || view.extra.contains_key("max_completion_tokens")
            || view.extra.contains_key("max_tokens");
        if !client_has_max
            && let Some(ceiling) = model
                .max_output_tokens
                .filter(|&v| v > 0)
                .map(|v| v.min(i64::from(u32::MAX)) as u32)
        {
            view.max_tokens = Some(ceiling);
        }
        adapter.normalize_openai_request(&mut view);
        serde_json::to_vec(&view)
            .map(bytes::Bytes::from)
            .ctx_parse("serialize openai request")
    }
}

/// Strips custom parameters specified per-provider in `extra_headers` (e.g. `X-OpenProxy-Strip-Params`)
/// from a fully-formatted upstream JSON body.
///
/// This is the single, format-agnostic choke point for parameter stripping: it runs once in the
/// target stage, AFTER the body has been formatted into its target dialect (OpenAI, Anthropic,
/// Gemini, Responses, …) and AFTER the provider adapter's `wrap_request_body`. Because it operates
/// on the final serialized body rather than on a typed intermediate, it works for every schema
/// without any per-provider or per-format hardcoding: each provider configures the keys to drop
/// via the `X-OpenProxy-Strip-Params` header and this function honors them uniformly.
pub fn strip_provider_configured_params_from_body(
    body: bytes::Bytes,
    extra_headers: &[(String, String)],
) -> Result<bytes::Bytes, CoreError> {
    let has_strip_config = extra_headers.iter().any(|(k, _)| {
        k.eq_ignore_ascii_case("x-openproxy-strip-params")
            || k.eq_ignore_ascii_case("x-strip-params")
    });
    if !has_strip_config {
        return Ok(body);
    }
    openproxy_adapters::adapters::traits::patch_json_request_body(body, |obj| {
        for (k, v) in extra_headers {
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
    })
}

fn message_needs_openai_normalization(m: &OpenAIMessage) -> bool {
    if m.extra.contains_key("cache_control") {
        return true;
    }
    if message_has_conflicting_reasoning(m) {
        return true;
    }
    if m.role == "assistant"
        && let Some(Value::String(s)) = &m.content
        && s.trim().is_empty()
    {
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
                    if matches!(
                        t,
                        "image_url"
                            | "input_image"
                            | "input_audio"
                            | "input_video"
                            | "video_url"
                            | "image"
                            | "video"
                            | "audio"
                            | "file"
                            | "input_file"
                            | "document"
                    ) {
                        has_media = true;
                    }
                }
            }
        }
        if !has_media {
            return true;
        }
    }
    if let Some(ref tcs) = m.tool_calls
        && tool_calls_need_normalization(tcs)
    {
        return true;
    }
    false
}

fn tool_calls_need_normalization(tool_calls: &[Value]) -> bool {
    tool_calls.iter().any(|tc| {
        let Some(func) = tc.get("function") else {
            return false;
        };
        match func.get("arguments") {
            Some(Value::String(s)) => {
                let trimmed = s.trim();
                trimmed.is_empty() || serde_json::from_str::<Value>(trimmed).is_err()
            }
            Some(Value::Object(_) | Value::Array(_)) => true,
            Some(_) => true,
            None => false,
        }
    })
}

pub fn sanitize_tool_call_arguments(args: &str) -> String {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return "{}".to_string();
    }
    if serde_json::from_str::<Value>(trimmed).is_ok() {
        return trimmed.to_string();
    }
    let mut de = serde_json::Deserializer::from_str(trimmed).into_iter::<Value>();
    if let Some(Ok(val)) = de.next() {
        return val.to_string();
    }
    let mut in_quote = false;
    let mut escaped = false;
    let mut stack = Vec::new();
    for ch in trimmed.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == '"' {
            in_quote = !in_quote;
        } else if !in_quote {
            match ch {
                '{' => stack.push('}'),
                '[' => stack.push(']'),
                '}' | ']' if stack.last() == Some(&ch) => {
                    stack.pop();
                }
                _ => {}
            }
        }
    }
    let mut repaired = trimmed.to_string();
    if in_quote {
        repaired.push('"');
    }
    while let Some(closing) = stack.pop() {
        repaired.push(closing);
    }
    if let Ok(val) = serde_json::from_str::<Value>(&repaired) {
        return val.to_string();
    }
    "{}".to_string()
}

fn sanitize_tool_calls(tool_calls: &[Value]) -> Vec<Value> {
    tool_calls
        .iter()
        .map(|tc| {
            let Some(obj) = tc.as_object() else {
                return tc.clone();
            };
            let mut new_obj = obj.clone();
            if let Some(func_val) = new_obj.get_mut("function")
                && let Some(func_obj) = func_val.as_object_mut()
                && let Some(args) = func_obj.get("arguments")
            {
                let sanitized = match args {
                    Value::String(s) => sanitize_tool_call_arguments(s),
                    Value::Object(_) | Value::Array(_) => args.to_string(),
                    _ => "{}".to_string(),
                };
                func_obj.insert("arguments".to_string(), Value::String(sanitized));
            }
            Value::Object(new_obj)
        })
        .collect()
}

/// Deduplicates reasoning fields in `extra`.
///
/// Algunos clientes reenvían el historial con `reasoning` y
/// `reasoning_content` a la vez. Upstreams como NVIDIA NIM rechazan el
/// cuerpo entero con un 400 de campo duplicado, así que nos quedamos con
/// `reasoning_content`, que es el nombre estándar, y descartamos `reasoning`.
///
/// Solo actúa cuando coexisten los dos. Un `reasoning` suelto se deja pasar
/// porque hay proveedores que únicamente entienden ese nombre.
pub(crate) fn dedupe_reasoning_fields(msg: &mut OpenAIMessage) {
    if msg.extra.contains_key("reasoning_content") {
        msg.extra.remove("reasoning");
    }
}

/// Checks whether an OpenAIMessage has conflicting reasoning fields that need deduplication.
pub(crate) fn message_has_conflicting_reasoning(msg: &OpenAIMessage) -> bool {
    msg.extra.contains_key("reasoning") && msg.extra.contains_key("reasoning_content")
}

/// Extracts reasoning content from an OpenAIMessage, looking in `extra` fields
/// (`reasoning_content`, `reasoning`, `thinking`) or within multipart `content`
/// array blocks of type `thinking` or `reasoning`.
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

fn normalize_openai_message(m: &OpenAIMessage) -> OpenAIMessage {
    let mut patched = m.clone();
    patched.extra.remove("cache_control");
    dedupe_reasoning_fields(&mut patched);
    if patched.role == "developer" {
        patched.role = "system".to_string();
    }
    patched.sanitize_name();

    if let Some(ref tcs) = patched.tool_calls {
        patched.tool_calls = Some(sanitize_tool_calls(tcs));
    }

    if patched.role == "assistant"
        && let Some(Value::String(ref s)) = patched.content
        && s.trim().is_empty()
    {
        patched.content = None;
    }

    if matches!(patched.role.as_str(), "assistant" | "system" | "tool")
        && matches!(&patched.content, Some(Value::Array(_) | Value::Object(_)))
    {
        patched.content = Some(Value::String(patched.extract_text()));
    } else if let Some(Value::Array(parts)) = &patched.content {
        let has_media = parts.iter().any(|p| {
            p.get("type").and_then(|v| v.as_str()).is_some_and(|t| {
                matches!(
                    t,
                    "image_url"
                        | "input_image"
                        | "input_audio"
                        | "input_video"
                        | "video_url"
                        | "image"
                        | "video"
                        | "audio"
                        | "file"
                        | "input_file"
                        | "document"
                )
            })
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
        let client_supplied_max_tokens = req.openai_request.max_tokens.is_some()
            || req
                .openai_request
                .extra
                .get("max_completion_tokens")
                .and_then(|v| v.as_u64())
                .is_some();
        let mut val = serde_json::to_value(&anthro).ctx_parse("serialize anthropic request")?;

        // Anthropic makes max_tokens mandatory, so the translator falls back to
        // DEFAULT_MAX_TOKENS (4096) when the client omits it. That floor is far
        // below every real model's output ceiling and burns out on always-thinking
        // models (thinking tokens count toward max_tokens). When the model row
        // declares its true ceiling and the client didn't pin one, raise the
        // emitted max_tokens to it. Anthropic rejects values above the model's
        // cap, so never exceed the declared ceiling.
        if !client_supplied_max_tokens
            && let Some(ceiling) = model
                .max_output_tokens
                .filter(|&v| v > 0)
                .map(|v| v.min(i64::from(u32::MAX)) as u32)
            && anthro.max_tokens < ceiling
            && let Some(obj) = val.as_object_mut()
        {
            obj.insert("max_tokens".to_string(), serde_json::json!(ceiling));
        }

        openproxy_adapters::anthropic_cache::ensure_anthropic_cache_control(&mut val);
        openproxy_adapters::anthropic_cache::enforce_cache_control_limit(&mut val, 4);

        let body_bytes = serde_json::to_vec(&val).ctx_parse("serialize anthropic request")?;
        Ok(bytes::Bytes::from(body_bytes))
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
mod test_helpers;

#[cfg(test)]
mod general_tests;

#[cfg(test)]
mod responses_tests;

#[cfg(test)]
mod tool_calls_tests;

#[cfg(test)]
mod strip_params_tests;
