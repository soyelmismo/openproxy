use crate::PipelineRequest;
use openproxy_types::OpenAIMessage;
use openproxy_types::ResultExt;
use openproxy_types::error::CoreError;
use openproxy_types::models::Model;
use serde_json::{Value, json};

use super::TargetFormatter;

mod input;
pub(crate) use input::*;

pub struct ResponsesFormatter;

pub(crate) const DEFAULT_RESPONSES_INSTRUCTIONS: &str = "You are an autonomous agent. Fulfill the user's intent completely without stopping to ask for confirmation or permission for routine, intermediate, or subsequent steps unless strictly blocked. Follow the developer instructions in the conversation.";

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
            messages_to_responses_input(messages_ref)?,
        );
        obj.insert("stream".to_string(), Value::Bool(stream));
        obj.insert("store".to_string(), Value::Bool(false));

        let default_instructions = DEFAULT_RESPONSES_INSTRUCTIONS.to_string();
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
        apply_responses_reasoning_and_tier(&mut obj, effort_from_model);

        if obj
            .get("prompt_cache_key")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            let instructions_str = obj
                .get("instructions")
                .and_then(|v| v.as_str())
                .unwrap_or(DEFAULT_RESPONSES_INSTRUCTIONS);
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
            .ctx_parse("serialize responses request")
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
        crate::schema_sanitizer::sanitize_tool_parameters(&mut flat_tool);
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
                if let Some(params) = func_obj.remove("parameters") {
                    obj.insert("parameters".to_string(), params);
                }
                if let Some(st) = func_obj.remove("strict") {
                    strict_val = Some(st);
                }
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
mod tests;
