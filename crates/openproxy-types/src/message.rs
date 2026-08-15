use crate::error::{CoreError, Result};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

/// Output wire format the upstream model natively speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "lowercase")]
pub enum TargetFormat {
    Openai,
    Anthropic,
    Gemini,
    Responses,
    Atomesus,
}

impl TargetFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            TargetFormat::Openai => "openai",
            TargetFormat::Anthropic => "anthropic",
            TargetFormat::Gemini => "gemini",
            TargetFormat::Responses => "responses",
            TargetFormat::Atomesus => "atomesus",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "openai" => Ok(TargetFormat::Openai),
            "anthropic" => Ok(TargetFormat::Anthropic),
            "gemini" => Ok(TargetFormat::Gemini),
            "responses" => Ok(TargetFormat::Responses),
            "atomesus" => Ok(TargetFormat::Atomesus),
            other => Err(CoreError::Validation(format!(
                "invalid target_format: {}",
                other
            ))),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIMessage {
    /// "system" | "user" | "assistant" | "tool" | "function" | "developer"
    pub role: String,
    #[serde(default, deserialize_with = "deserialize_optional_content")]
    pub content: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<serde_json::Value>>,
    #[serde(default, flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Extracts text from an optional JSON content value, handling string, multipart array, null, etc.
pub fn extract_content_text(content: &Option<serde_json::Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => {
            let mut out = String::new();
            for part in parts {
                out.push_str(&extract_content_part_text(part));
            }
            out
        }
        Some(Value::Null) | None => String::new(),
        Some(value) => value.to_string(),
    }
}

/// Extracts text from a single content part JSON value.
pub fn extract_content_part_text(part: &serde_json::Value) -> String {
    if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
        return text.to_string();
    }
    if let Some(content) = part.get("content").and_then(|v| v.as_str()) {
        return content.to_string();
    }
    match part {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

impl OpenAIMessage {
    /// Extracts a direct string slice if the message content is a plain string.
    /// Returns `None` if content is `None`, `Null`, or structured (e.g. array of parts).
    pub fn extract_text_lossless(&self) -> Option<&str> {
        self.content.as_ref().and_then(|c| c.as_str())
    }

    /// Extracts all text content from the message, handling both plain strings
    /// and multipart arrays (e.g. `[{"type": "text", "text": "..."}]` or `[{"content": "..."}]`).
    /// Returns an empty string if content is `None` or `Null`.
    pub fn extract_text(&self) -> String {
        extract_content_text(&self.content)
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OpenAIRequest {
    pub model: String,
    pub messages: Vec<OpenAIMessage>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
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
    pub tools: &'a Option<Vec<serde_json::Value>>,
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
            tools: &req.tools,
            tool_choice: &req.tool_choice,
            top_k: req.top_k,
            user: &req.user,
            extra: std::borrow::Cow::Borrowed(&req.extra),
            stream,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_extract_text_plain_string() {
        let msg = OpenAIMessage {
            role: "user".to_string(),
            content: Some(json!("hello world")),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
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
            extra: Default::default(),
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
            extra: Default::default(),
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
            extra: Default::default(),
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
            extra: Default::default(),
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
            extra: Default::default(),
        };
        assert_eq!(msg_none.extract_text(), "");
        assert_eq!(msg_none.extract_text_lossless(), None);

        let msg_null = OpenAIMessage {
            role: "assistant".to_string(),
            content: Some(json!(null)),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        };
        assert_eq!(msg_null.extract_text(), "");
        assert_eq!(msg_null.extract_text_lossless(), None);
    }

    #[test]
    fn test_extract_content_part_text() {
        assert_eq!(extract_content_part_text(&json!({"text": "abc"})), "abc");
        assert_eq!(extract_content_part_text(&json!({"content": "xyz"})), "xyz");
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
        ] {
            assert_eq!(TargetFormat::parse(fmt.as_str()).unwrap(), fmt);
        }
        assert!(TargetFormat::parse("unknown").is_err());
    }
}
