use super::parser::InlineToolParser;
use super::types::{ParsedToolCall, generate_tool_call_id};
use serde_json::Value;

/// Parser for Hermes, Qwen, and DeepSeek style JSON tool calls:
///
/// ```text
/// <tool_call>
/// {"name": "fetch_web_page", "arguments": {"url": "https://..."}}
/// </tool_call>
/// ```
///
/// Or:
///
/// ```text
/// [TOOL_CALLS] [{"name": "fetch_web_page", "arguments": {...}}]
/// ```
#[derive(Debug, Default, Clone, Copy)]
pub struct HermesJsonParser;

impl HermesJsonParser {
    pub fn new() -> Self {
        Self
    }
}

impl InlineToolParser for HermesJsonParser {
    fn parse_block(&self, block: &str) -> Option<Vec<ParsedToolCall>> {
        let trimmed = block.trim();
        let cleaned = strip_markdown_codeblock(trimmed);

        if cleaned.starts_with('[') && cleaned.ends_with(']') {
            let v = serde_json::from_str::<Value>(cleaned).ok()?;
            let arr = v.as_array()?;
            let mut calls = Vec::new();
            for item in arr {
                if let Some(call) = parse_single_json_tool_call(item) {
                    calls.push(call);
                }
            }
            if calls.is_empty() { None } else { Some(calls) }
        } else if cleaned.starts_with('{') && cleaned.ends_with('}') {
            let v = serde_json::from_str::<Value>(cleaned).ok()?;
            parse_single_json_tool_call(&v).map(|c| vec![c])
        } else {
            None
        }
    }
}

/// Parse a single JSON object into a [`ParsedToolCall`].
fn parse_single_json_tool_call(val: &Value) -> Option<ParsedToolCall> {
    let obj = val.as_object()?;

    // Standard format: {"name": "...", "arguments": {...}}
    let mut name = obj.get("name").and_then(Value::as_str);
    let mut args_val = obj.get("arguments").or_else(|| obj.get("parameters")).or_else(|| obj.get("input"));

    // Nested function format: {"function": {"name": "...", "arguments": ...}}
    if name.is_none()
        && let Some(func_obj) = obj.get("function").and_then(Value::as_object)
    {
        name = func_obj.get("name").and_then(Value::as_str);
        if args_val.is_none() {
            args_val = func_obj.get("arguments").or_else(|| func_obj.get("parameters"));
        }
    }

    let name = name?.trim();
    if name.is_empty() {
        return None;
    }

    let id = obj
        .get("id")
        .or_else(|| obj.get("call_id"))
        .and_then(Value::as_str)
        .map_or_else(generate_tool_call_id, ToString::to_string);

    let arguments = match args_val {
        Some(Value::String(s)) => s.clone(),
        Some(val @ (Value::Object(_) | Value::Array(_))) => {
            serde_json::to_string(val).unwrap_or_else(|_| "{}".to_string())
        }
        Some(other) => other.to_string(),
        None => "{}".to_string(),
    };

    Some(ParsedToolCall {
        id,
        name: name.to_string(),
        arguments,
    })
}

/// Strips markdown code block wrappers (e.g. ```json ... ```) if present.
fn strip_markdown_codeblock(s: &str) -> &str {
    let trimmed = s.trim();
    if let Some(rest) = trimmed.strip_prefix("```") {
        let after_lang = if let Some(newline_idx) = rest.find('\n') {
            &rest[newline_idx + 1..]
        } else {
            rest
        };
        if let Some(content) = after_lang.strip_suffix("```") {
            return content.trim();
        }
    }
    trimmed
}
