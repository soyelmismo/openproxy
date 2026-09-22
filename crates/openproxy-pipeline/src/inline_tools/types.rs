use serde_json::Value;

/// Structured representation of a single extracted tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedToolCall {
    /// Unique identifier for the tool call (e.g. "call_1234567890abcdef").
    pub id: String,
    /// Function/tool name (e.g. "fetch_web_page").
    pub name: String,
    /// Arguments encoded as a valid JSON string (e.g. `{"url":"https://..."}`).
    pub arguments: String,
}

impl ParsedToolCall {
    /// Convert to OpenAI-compatible `tool_call` JSON object.
    pub fn to_openai_value(&self) -> Value {
        serde_json::json!({
            "id": self.id,
            "type": "function",
            "function": {
                "name": self.name,
                "arguments": self.arguments,
            }
        })
    }
}

/// Result of extracting inline tool calls from a text content string.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtractedInlineTools {
    /// Content with inline tool call blocks cleanly removed.
    pub clean_content: String,
    /// Parsed tool calls.
    pub tool_calls: Vec<ParsedToolCall>,
}

impl ExtractedInlineTools {
    #[inline]
    pub fn has_tools(&self) -> bool {
        !self.tool_calls.is_empty()
    }
}

/// Generates a standardized OpenAI-compatible tool call ID.
pub fn generate_tool_call_id() -> String {
    let raw = uuid::Uuid::new_v4().simple().to_string();
    let suffix = if raw.len() >= 16 {
        &raw[..16]
    } else {
        &raw
    };
    format!("call_{suffix}")
}
