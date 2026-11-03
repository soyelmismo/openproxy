//! Sanitizer and normalization for CodeBuddy / WorkBuddy requests.
//!
//! Handles Tencent-specific restrictions:
//! 1. Content filter on agent system prompts (Claude Code, cc_entrypoint, etc.)
//!    triggering error 11128 ("Illegal API invocation from an unapproved channel").
//! 2. Go struct unmarshal failure on structured `tool_choice` objects:
//!    Tencent backend only supports string values ("auto", "none", "required").
//! 3. Oversized tool descriptions (>64KB) triggering sensitive content rejections.

use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

pub const NEUTRAL_CODEBUDDY_SYSTEM_PROMPT: &str =
    "You are a helpful AI assistant that helps with software engineering tasks.";

static AGENT_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    openproxy_types::static_regex!(
        r"(?i)you are claude code|claude.?code.+official.+cli|anthropic.+official.+cli|anxthxropic.+official.+cli|x-anthropic-billing-header|cc_entrypoint\s*=\s*(?:cli|vscode|jetbrains|gui)|you are (?:cursor|windsurf|cline|aider|continue|copilot|cody)|you are an? (?:ai )?(?:coding |code )?agent|claude.?code.+issues|give feedback.+claude.?code|you are .{0,30}(?:powerful )?ai agent|orchestration capabilities|OhMyOpenCode|<agent-identity>|<Role>|<Behavior_Instructions>"
    )
});

/// Flattens message content (string, array of text parts) to a single string for pattern matching.
fn extract_text_content(val: &Value) -> Option<String> {
    match val {
        Value::String(s) => Some(s.clone()),
        Value::Array(arr) => {
            let mut buf = String::new();
            for item in arr {
                if let Some(t) = item.get("text").and_then(Value::as_str) {
                    if !buf.is_empty() {
                        buf.push('\n');
                    }
                    buf.push_str(t);
                }
            }
            if buf.is_empty() { None } else { Some(buf) }
        }
        _ => None,
    }
}

/// Checks whether a system prompt triggers Tencent channel/content filter
/// (either by length > 2000 or by containing agent/billing header markers).
pub fn is_agent_system_prompt(content: &Value) -> bool {
    let Some(text) = extract_text_content(content) else {
        return false;
    };
    text.len() > 2000 || AGENT_PATTERN.is_match(&text)
}

/// Sanitizes system messages to avoid Tencent error 11128.
pub fn sanitize_system_messages(messages: &mut [Value]) {
    for msg in messages.iter_mut() {
        if msg.get("role").and_then(Value::as_str) == Some("system") {
            let should_sanitize = msg.get("content").is_some_and(is_agent_system_prompt);
            if should_sanitize && let Some(obj) = msg.as_object_mut() {
                let is_array = obj.get("content").is_some_and(Value::is_array);
                if is_array {
                    obj.insert(
                        "content".to_string(),
                        serde_json::json!([{ "type": "text", "text": NEUTRAL_CODEBUDDY_SYSTEM_PROMPT }]),
                    );
                } else {
                    obj.insert(
                        "content".to_string(),
                        Value::String(NEUTRAL_CODEBUDDY_SYSTEM_PROMPT.to_string()),
                    );
                }
            }
        }
    }
}

/// Tencent Go backend expects `tool_choice` to be a string ("auto", "none", "required").
/// When clients pass structured object `{"type": "function", "function": {"name": "..."}}`,
/// Tencent fails with `11101 Unmarshal chat params failed: cannot unmarshal object into Go struct field Request.tool_choice of type string`.
pub fn normalize_tool_choice(obj: &mut serde_json::Map<String, Value>) {
    if let Some(tc) = obj.get("tool_choice") {
        match tc {
            Value::Object(_) => {
                // Flatten structured tool_choice object to "auto"
                obj.insert("tool_choice".to_string(), Value::String("auto".to_string()));
            }
            Value::String(_) => {}
            _ => {
                obj.remove("tool_choice");
            }
        }
    }
}

/// Strips oversized tool descriptions (>64KB total tools) to prevent sensitive content filter trips.
pub fn compact_oversized_tools(obj: &mut serde_json::Map<String, Value>) {
    let Some(tools_arr) = obj.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };
    if tools_arr.is_empty() {
        return;
    }

    let Ok(serialized) = serde_json::to_string(&tools_arr) else {
        return;
    };
    if serialized.len() < 65536 {
        return;
    }

    for tool in tools_arr.iter_mut() {
        if let Some(tool_obj) = tool.as_object_mut()
            && tool_obj.get("type").and_then(Value::as_str) == Some("function")
            && let Some(func) = tool_obj.get_mut("function").and_then(Value::as_object_mut)
        {
            func.remove("description");
        }
    }
}
