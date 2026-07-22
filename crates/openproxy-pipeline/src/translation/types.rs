use serde::{Deserialize, Serialize};
pub use openproxy_types::{OpenAIChoice, OpenAIResponse, OpenAIUsage};


// =====================
// Anthropic Messages types
// =====================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicRequest {
    pub model: String,
    pub messages: Vec<AnthropicMessage>,
    pub max_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    #[serde(default)]
    pub stream: bool,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicMessage {
    /// "user" | "assistant"
    pub role: String,
    /// Anthropic accepts `content` as either a plain string
    /// (`"content": "text"`) or an array of typed content blocks
    /// (`"content": [{"type":"text","text":"..."},
    /// {"type":"tool_use","id":"...","name":"...","input":{...}},
    /// {"type":"tool_result","tool_use_id":"...","content":"..."}]`).
    /// We use `serde_json::Value` so the translator can emit either
    /// form depending on whether the source OpenAI message carried
    /// only text or also carried `tool_calls` / was a `tool`-role
    /// message.
    pub content: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicResponse {
    pub id: String,
    /// "message"
    #[serde(rename = "type")]
    pub response_type: String,
    /// "assistant"
    pub role: String,
    pub content: Vec<serde_json::Value>,
    pub model: String,
    #[serde(default)]
    pub stop_reason: Option<String>,
    pub usage: AnthropicUsage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicContentBlock {
    /// "text"
    #[serde(rename = "type")]
    pub block_type: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

/// Default max_tokens used when the client doesn't provide one. Anthropic requires it.
pub const DEFAULT_MAX_TOKENS: u32 = 4096;

