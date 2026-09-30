use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

impl_string_enum! {
    /// Output wire format the upstream model natively speaks.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
    #[serde(rename_all = "lowercase")]
    pub enum TargetFormat {
        Openai => "openai",
        Anthropic => "anthropic",
        Gemini => "gemini",
        Responses => "responses",
        Atomesus => "atomesus",
        CommandCodeGo => "commandcodego",
        SystemOne => "systemone",
    }
    core_error: "target_format"
}

use std::borrow::Cow;

/// Detailed descriptor of a target wire format for API introspection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetFormatDescriptor {
    pub id: TargetFormat,
    pub name: Cow<'static, str>,
    pub label: Cow<'static, str>,
    pub endpoint: Option<Cow<'static, str>>,
    pub description: Cow<'static, str>,
}

impl TargetFormat {
    pub const ALL: &'static [Self] = &[
        Self::Openai,
        Self::Responses,
        Self::Anthropic,
        Self::Gemini,
        Self::SystemOne,
        Self::Atomesus,
        Self::CommandCodeGo,
    ];

    #[inline]
    pub const fn descriptor(&self) -> TargetFormatDescriptor {
        match self {
            Self::Openai => TargetFormatDescriptor {
                id: Self::Openai,
                name: Cow::Borrowed("OpenAI Chat Completions"),
                label: Cow::Borrowed("OpenAI Chat Completions (/v1/chat/completions)"),
                endpoint: Some(Cow::Borrowed("/v1/chat/completions")),
                description: Cow::Borrowed("Standard OpenAI chat completions API wire format"),
            },
            Self::Responses => TargetFormatDescriptor {
                id: Self::Responses,
                name: Cow::Borrowed("OpenAI Responses"),
                label: Cow::Borrowed("OpenAI Responses (/v1/responses)"),
                endpoint: Some(Cow::Borrowed("/v1/responses")),
                description: Cow::Borrowed("OpenAI Responses API wire format"),
            },
            Self::Anthropic => TargetFormatDescriptor {
                id: Self::Anthropic,
                name: Cow::Borrowed("Anthropic Messages"),
                label: Cow::Borrowed("Anthropic Messages (/v1/messages)"),
                endpoint: Some(Cow::Borrowed("/v1/messages")),
                description: Cow::Borrowed("Anthropic Claude Messages API wire format"),
            },
            Self::Gemini => TargetFormatDescriptor {
                id: Self::Gemini,
                name: Cow::Borrowed("Google Gemini"),
                label: Cow::Borrowed("Google Gemini (generateContent)"),
                endpoint: Some(Cow::Borrowed("v1beta/models/...:generateContent")),
                description: Cow::Borrowed("Google Gemini REST API generateContent wire format"),
            },
            Self::SystemOne => TargetFormatDescriptor {
                id: Self::SystemOne,
                name: Cow::Borrowed("SystemOne Decision"),
                label: Cow::Borrowed("SystemOne (Decision / Fast Engine)"),
                endpoint: Some(Cow::Borrowed("/v1/decisions")),
                description: Cow::Borrowed("SystemOne fast decision engine wire format"),
            },
            Self::Atomesus => TargetFormatDescriptor {
                id: Self::Atomesus,
                name: Cow::Borrowed("Atomesus"),
                label: Cow::Borrowed("Atomesus"),
                endpoint: None,
                description: Cow::Borrowed("Atomesus upstream wire format"),
            },
            Self::CommandCodeGo => TargetFormatDescriptor {
                id: Self::CommandCodeGo,
                name: Cow::Borrowed("CommandCodeGo"),
                label: Cow::Borrowed("CommandCodeGo"),
                endpoint: None,
                description: Cow::Borrowed("CommandCode Go bridge wire format"),
            },
        }
    }

    pub fn all_descriptors() -> Vec<TargetFormatDescriptor> {
        Self::ALL.iter().map(|fmt| fmt.descriptor()).collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIMessage {
    /// "system" | "user" | "assistant" | "tool" | "function" | "developer"
    pub role: String,
    #[serde(default, deserialize_with = "deserialize_optional_content")]
    pub content: Option<serde_json::Value>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_name",
        skip_serializing_if = "Option::is_none"
    )]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<serde_json::Value>>,
    #[serde(default, flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Extracts text from an optional JSON content value without allocating when content is a plain string.
pub fn extract_content_text_cow(content: &Option<serde_json::Value>) -> std::borrow::Cow<'_, str> {
    match content {
        Some(Value::String(s)) => std::borrow::Cow::Borrowed(s.as_str()),
        Some(Value::Array(parts)) => {
            let mut out = String::new();
            for part in parts {
                out.push_str(&extract_content_part_text(part));
            }
            std::borrow::Cow::Owned(out)
        }
        Some(Value::Null) | None => std::borrow::Cow::Borrowed(""),
        Some(value) => std::borrow::Cow::Owned(value.to_string()),
    }
}

/// Extracts text from an optional JSON content value, handling string, multipart array, null, etc.
pub fn extract_content_text(content: &Option<serde_json::Value>) -> String {
    extract_content_text_cow(content).into_owned()
}

/// Extracts text from a single content part JSON value.
pub fn extract_content_part_text(part: &serde_json::Value) -> String {
    if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
        return text.to_string();
    }
    if let Some(content) = part.get("content").and_then(|v| v.as_str()) {
        return content.to_string();
    }
    if let Some(thinking) = part.get("thinking").and_then(|v| v.as_str()) {
        return thinking.to_string();
    }
    if let Some(reasoning) = part.get("reasoning").and_then(|v| v.as_str()) {
        return reasoning.to_string();
    }
    match part {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Bool(_) | Value::Number(_) | Value::Array(_) | Value::Object(_) => part.to_string(),
    }
}

impl OpenAIMessage {
    /// Extracts a direct string slice if the message content is a plain string.
    /// Returns `None` if content is `None`, `Null`, or structured (e.g. array of parts).
    pub fn extract_text_lossless(&self) -> Option<&str> {
        self.content.as_ref().and_then(|c| c.as_str())
    }

    /// Extracts all text content from the message as a `Cow<str>`, avoiding
    /// heap allocation if the message is a plain string.
    pub fn extract_text_cow(&self) -> std::borrow::Cow<'_, str> {
        extract_content_text_cow(&self.content)
    }

    /// Extracts all text content from the message, handling both plain strings
    /// and multipart arrays (e.g. `[{"type": "text", "text": "..."}]` or `[{"content": "..."}]`).
    /// Returns an empty string if content is `None` or `Null`.
    pub fn extract_text(&self) -> String {
        self.extract_text_cow().into_owned()
    }

    /// Sanitizes the `name` field according to OpenAI API validation:
    /// - Strips `name` from `tool` role messages (OpenAI uses `tool_call_id`).
    /// - For other roles, enforces `^[a-zA-Z0-9_-]{1,64}$`.
    pub fn sanitize_name(&mut self) {
        self.extra.remove("name");
        self.name = sanitize_message_name(&self.role, self.name.as_deref());
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIResponse {
    pub id: String,
    /// "chat.completion"
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<OpenAIChoice>,
    pub usage: Option<OpenAIUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIChoice {
    pub index: u32,
    pub message: OpenAIMessage,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptTokensDetails {
    pub cached_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
}

fn deserialize_optional_content<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Value>, D::Error>
where
    D: Deserializer<'de>,
{
    Value::deserialize(deserializer).map(Some)
}

fn deserialize_optional_name<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    match Option::<Value>::deserialize(deserializer)? {
        Some(Value::String(s)) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed.to_string()))
            }
        }
        Some(Value::Number(n)) => Ok(Some(n.to_string())),
        _ => Ok(None),
    }
}

/// Sanitizes a message `name` field to conform to OpenAI API validation:
/// - Must match `^[a-zA-Z0-9_-]{1,64}$`.
/// - Role "tool" does not support `name` in OpenAI API (it uses `tool_call_id`).
pub fn sanitize_message_name(role: &str, name: Option<&str>) -> Option<String> {
    if role == "tool" {
        return None;
    }
    let raw = name?.trim();
    if raw.is_empty() {
        return None;
    }
    if !raw
        .chars()
        .any(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    let sanitized: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();

    if sanitized.is_empty() {
        None
    } else {
        Some(sanitized)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OpenAIRequest {
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub messages: Vec<OpenAIMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(default)]
    pub stream: bool,
}

#[derive(Serialize)]
pub struct OpenAIRequestView<'a> {
    pub model: &'a str,
    pub messages: std::borrow::Cow<'a, [OpenAIMessage]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: &'a Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<std::borrow::Cow<'a, [serde_json::Value]>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: &'a Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: &'a Option<String>,
    #[serde(flatten)]
    pub extra: std::borrow::Cow<'a, serde_json::Map<String, serde_json::Value>>,
    pub stream: bool,
}

impl<'a> OpenAIRequestView<'a> {
    pub fn new(
        req: &'a OpenAIRequest,
        model: &'a str,
        messages: &'a [OpenAIMessage],
        stream: bool,
    ) -> Self {
        Self {
            model,
            messages: std::borrow::Cow::Borrowed(messages),
            temperature: req.temperature,
            max_tokens: req.max_tokens,
            top_p: req.top_p,
            stop: &req.stop,
            tools: req.tools.as_deref().map(std::borrow::Cow::Borrowed),
            tool_choice: &req.tool_choice,
            top_k: req.top_k,
            user: &req.user,
            extra: std::borrow::Cow::Borrowed(&req.extra),
            stream,
        }
    }
}

/// Maps upstream stop reasons to canonical OpenAI finish reasons ("stop", "length", "tool_calls", "content_filter").
///
/// Returns canonical `"stop"`, `"length"`, `"tool_calls"`, or `"content_filter"` at zero runtime allocation.
/// Unknown or unmapped stop reasons default to `"stop"`.
#[inline]
pub const fn map_stop_reason_to_finish_reason(reason: &str) -> &'static str {
    match reason.as_bytes() {
        b"end_turn" | b"stop" | b"stop_sequence" | b"end" | b"complete" => "stop",
        b"max_tokens" | b"length" => "length",
        b"tool_use" | b"tool_call" | b"tool_calls" | b"toolUse" | b"toolCall" | b"toolCalls" => {
            "tool_calls"
        }
        b"content_filter" | b"safety" => "content_filter",
        _ => "stop",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_map_stop_reason_to_finish_reason() {
        const CONST_CHECK: &str = map_stop_reason_to_finish_reason("tool_use");
        assert_eq!(CONST_CHECK, "tool_calls");

        for reason in ["end_turn", "stop", "stop_sequence", "end", "complete", ""] {
            assert_eq!(map_stop_reason_to_finish_reason(reason), "stop");
        }
        for reason in ["max_tokens", "length"] {
            assert_eq!(map_stop_reason_to_finish_reason(reason), "length");
        }
        for reason in [
            "tool_use",
            "tool_call",
            "tool_calls",
            "toolUse",
            "toolCall",
            "toolCalls",
        ] {
            assert_eq!(map_stop_reason_to_finish_reason(reason), "tool_calls");
        }
        for reason in ["content_filter", "safety"] {
            assert_eq!(map_stop_reason_to_finish_reason(reason), "content_filter");
        }
        assert_eq!(map_stop_reason_to_finish_reason("unexpected_reason"), "stop");
    }

    #[test]
    fn test_extract_text_plain_string() {
        let msg = OpenAIMessage {
            role: "user".to_string(),
            content: Some(json!("hello world")),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::default(),
        };
        assert_eq!(msg.extract_text(), "hello world");
        assert_eq!(msg.extract_text_lossless(), Some("hello world"));
    }

    #[test]
    fn test_extract_text_array_parts() {
        let msg = OpenAIMessage {
            role: "user".to_string(),
            content: Some(json!([
                {"type": "text", "text": "hello "},
                {"type": "text", "text": "world"}
            ])),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::default(),
        };
        assert_eq!(msg.extract_text(), "hello world");
        assert_eq!(msg.extract_text_lossless(), None);
    }

    #[test]
    fn test_extract_text_array_with_content_field() {
        let msg = OpenAIMessage {
            role: "user".to_string(),
            content: Some(json!([
                {"type": "text", "content": "foo "},
                {"type": "text", "content": "bar"}
            ])),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::default(),
        };
        assert_eq!(msg.extract_text(), "foo bar");
        assert_eq!(msg.extract_text_lossless(), None);
    }

    #[test]
    fn test_extract_text_array_plain_strings() {
        let msg = OpenAIMessage {
            role: "user".to_string(),
            content: Some(json!(["hello ", "world"])),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::default(),
        };
        assert_eq!(msg.extract_text(), "hello world");
        assert_eq!(msg.extract_text_lossless(), None);
    }

    #[test]
    fn test_extract_text_array_mixed() {
        let msg = OpenAIMessage {
            role: "user".to_string(),
            content: Some(json!([
                "first ",
                {"type": "text", "text": "second "},
                {"type": "text", "content": "third"}
            ])),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::default(),
        };
        assert_eq!(msg.extract_text(), "first second third");
        assert_eq!(msg.extract_text_lossless(), None);
    }

    #[test]
    fn test_extract_text_none_and_null() {
        let msg_none = OpenAIMessage {
            role: "assistant".to_string(),
            content: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::default(),
        };
        assert_eq!(msg_none.extract_text(), "");
        assert_eq!(msg_none.extract_text_lossless(), None);

        let msg_null = OpenAIMessage {
            role: "assistant".to_string(),
            content: Some(json!(null)),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::default(),
        };
        assert_eq!(msg_null.extract_text(), "");
        assert_eq!(msg_null.extract_text_lossless(), None);
    }

    #[test]
    fn test_extract_content_part_text() {
        assert_eq!(extract_content_part_text(&json!({"text": "abc"})), "abc");
        assert_eq!(extract_content_part_text(&json!({"content": "xyz"})), "xyz");
        assert_eq!(
            extract_content_part_text(&json!({"type": "thinking", "thinking": "step1"})),
            "step1"
        );
        assert_eq!(
            extract_content_part_text(&json!({"type": "reasoning", "reasoning": "step2"})),
            "step2"
        );
        assert_eq!(extract_content_part_text(&json!("str")), "str");
        assert_eq!(extract_content_part_text(&json!(null)), "");
        assert_eq!(extract_content_part_text(&json!(123)), "123");
    }

    #[test]
    fn test_target_format_as_str_and_parse() {
        for fmt in [
            TargetFormat::Openai,
            TargetFormat::Anthropic,
            TargetFormat::Gemini,
            TargetFormat::Responses,
            TargetFormat::Atomesus,
            TargetFormat::CommandCodeGo,
        ] {
            assert_eq!(TargetFormat::parse(fmt.as_str()).unwrap(), fmt);
        }
        assert!(TargetFormat::parse("unknown").is_err());
    }

    #[test]
    fn test_sanitize_message_name() {
        // Tool messages always drop name
        assert_eq!(sanitize_message_name("tool", Some("get_weather")), None);
        assert_eq!(sanitize_message_name("tool", Some("")), None);

        // Empty / whitespace
        assert_eq!(sanitize_message_name("user", Some("")), None);
        assert_eq!(sanitize_message_name("user", Some("   ")), None);
        assert_eq!(sanitize_message_name("user", None), None);

        // Disallowed chars become underscores
        assert_eq!(
            sanitize_message_name("user", Some("John Doe")),
            Some("John_Doe".into())
        );
        assert_eq!(
            sanitize_message_name("assistant", Some("tool.call:1")),
            Some("tool_call_1".into())
        );
        assert_eq!(
            sanitize_message_name("system", Some("agent@domain/1")),
            Some("agent_domain_1".into())
        );

        // Only invalid characters
        assert_eq!(sanitize_message_name("user", Some("???")), None);

        // Preserves valid characters and length truncation
        assert_eq!(
            sanitize_message_name("user", Some("valid-name_123")),
            Some("valid-name_123".into())
        );
        let long = "a".repeat(100);
        let sanitized = sanitize_message_name("user", Some(&long)).unwrap();
        assert_eq!(sanitized.len(), 64);
    }

    #[test]
    fn test_deserialize_and_sanitize_name() {
        let json_empty = json!({
            "role": "user",
            "content": "hello",
            "name": ""
        });
        let msg: OpenAIMessage = serde_json::from_value(json_empty).unwrap();
        assert_eq!(msg.name, None);

        let json_tool = json!({
            "role": "tool",
            "content": "result",
            "name": "calc",
            "tool_call_id": "call_123"
        });
        let mut msg_tool: OpenAIMessage = serde_json::from_value(json_tool).unwrap();
        msg_tool.sanitize_name();
        assert_eq!(msg_tool.name, None);

        let json_spaces = json!({
            "role": "assistant",
            "content": "ok",
            "name": "My Agent"
        });
        let mut msg_spaces: OpenAIMessage = serde_json::from_value(json_spaces).unwrap();
        msg_spaces.sanitize_name();
        assert_eq!(msg_spaces.name, Some("My_Agent".into()));
    }
}
