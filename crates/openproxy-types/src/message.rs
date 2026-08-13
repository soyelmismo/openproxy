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

impl OpenAIRequest {
    /// Sanitize all tool definitions in this request.
    pub fn sanitize_tools(&mut self) {
        if let Some(tools) = &mut self.tools {
            for tool in tools {
                sanitize_single_tool(tool);
            }
        }
    }
}

/// Sanitize a single tool definition to conform to standard OpenAI function-calling schema.
///
/// If a client puts `properties`, `required`, or `type: 'object'` directly at the `function` level
/// instead of within `function.parameters`, this normalizes it into `function.parameters`
/// and strips the misplaced fields to prevent strict upstream validators (like Fireworks / Pydantic)
/// from rejecting the request with 400 Bad Request.
pub fn sanitize_single_tool(tool: &mut serde_json::Value) {
    if let Some(tool_obj) = tool.as_object_mut() {
        // Handle Anthropic-style tool objects sent directly to OpenAI endpoint
        if tool_obj.get("type").is_none()
            && tool_obj.contains_key("input_schema")
            && tool_obj.contains_key("name")
        {
            let name = tool_obj.remove("name");
            let desc = tool_obj.remove("description");
            let schema = tool_obj.remove("input_schema");
            let mut func = serde_json::Map::new();
            if let Some(n) = name {
                func.insert("name".to_string(), n);
            }
            if let Some(d) = desc {
                func.insert("description".to_string(), d);
            }
            if let Some(s) = schema {
                func.insert("parameters".to_string(), s);
            }
            tool_obj.insert("type".to_string(), serde_json::json!("function"));
            tool_obj.insert("function".to_string(), serde_json::Value::Object(func));
            return;
        }

        if let Some(func_val) = tool_obj.get_mut("function") {
            if let Some(func_obj) = func_val.as_object_mut() {
                let has_props = func_obj.contains_key("properties");
                let has_req = func_obj.contains_key("required");
                let is_obj_type = func_obj.get("type").and_then(|v| v.as_str()) == Some("object");

                if has_props || has_req || is_obj_type {
                    let mut params = func_obj
                        .get("parameters")
                        .and_then(|p| p.as_object().cloned())
                        .unwrap_or_default();

                    if let Some(props) = func_obj.remove("properties") {
                        params.insert("properties".to_string(), props);
                    }
                    if let Some(req) = func_obj.remove("required") {
                        params.insert("required".to_string(), req);
                    }
                    if let Some(t) = func_obj.remove("type") {
                        if !params.contains_key("type") {
                            params.insert("type".to_string(), t);
                        }
                    }
                    if !params.is_empty() {
                        if !params.contains_key("type") {
                            params.insert("type".to_string(), serde_json::json!("object"));
                        }
                        func_obj
                            .insert("parameters".to_string(), serde_json::Value::Object(params));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_misplaced_function_properties() {
        let mut tool = serde_json::json!({
            "type": "function",
            "function": {
                "name": "generate_image",
                "description": "Generate an image",
                "type": "object",
                "properties": {
                    "prompt": { "type": "string" }
                },
                "required": ["prompt"]
            }
        });

        sanitize_single_tool(&mut tool);

        let func = tool.get("function").unwrap().as_object().unwrap();
        assert!(!func.contains_key("properties"));
        assert!(!func.contains_key("required"));
        assert!(!func.contains_key("type"));

        let params = func.get("parameters").unwrap().as_object().unwrap();
        assert_eq!(params.get("type").unwrap(), "object");
        assert!(params.contains_key("properties"));
        assert_eq!(
            params.get("required").unwrap(),
            &serde_json::json!(["prompt"])
        );
    }
}
