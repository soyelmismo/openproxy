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
}

impl TargetFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            TargetFormat::Openai => "openai",
            TargetFormat::Anthropic => "anthropic",
            TargetFormat::Gemini => "gemini",
            TargetFormat::Responses => "responses",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "openai" => Ok(TargetFormat::Openai),
            "anthropic" => Ok(TargetFormat::Anthropic),
            "gemini" => Ok(TargetFormat::Gemini),
            "responses" => Ok(TargetFormat::Responses),
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
pub struct OpenAIUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
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
