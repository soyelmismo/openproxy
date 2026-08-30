//! fx.sh WebAssembly Gateway Provider Adapter
//!
//! Exposes free GLM-5.2 inference from the fx.sh demo gateway with spoofed
//! browser identity and Vercel AI SDK Spec v4 request/response translation.

use super::{
    AdapterAuthType, AdapterFormat, DiscoveredModel, ModelId, ProviderAdapter,
    ProviderAdapterConfig, ProviderId, Result, TargetFormat, UpstreamClient,
};
use crate::spoofer::{ClientSpoofer, FxSpoofer};
use openproxy_types::{CoreError, OpenAIMessage, OpenAIRequest, OpenAIResponse, ProviderMetadata};
use serde_json::Value;
use std::sync::Arc;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct FxAdapter {
    config: ProviderAdapterConfig,
}

impl FxAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("fx"),
                name: "fx (Free GLM-5.2)".into(),
                anonymous_fallback: true,
                rate_limit_scope: "account".into(),
                base_url: "https://fx.sh/fx-wasm/gateway/v3/ai/language-model".into(),
                auth_type: AdapterAuthType::Bearer,
                format: AdapterFormat::Fx,
                extra_headers: Vec::new(),
            },
        }
    }
}

crate::adapters::derive_default_from_new!(FxAdapter);

fn convert_function_tool(func: &Value) -> Option<Value> {
    let name = func.get("name")?.as_str()?;
    let description = func.get("description").and_then(|d| d.as_str());
    let input_schema = func
        .get("parameters")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({"type": "object", "properties": {}}));
    let mut obj = serde_json::json!({
        "type": "function",
        "name": name,
        "inputSchema": input_schema,
    });
    if let Some(desc) = description {
        obj["description"] = Value::String(desc.to_string());
    }
    Some(obj)
}

fn convert_generic_tool(t: &Value) -> Option<Value> {
    if t.get("name").is_some() && t.get("inputSchema").is_some() {
        return Some(t.clone());
    }
    if t.get("name").and_then(|n| n.as_str()).is_some() {
        let mut obj = t.clone();
        if obj.get("inputSchema").is_none() {
            obj["inputSchema"] = serde_json::json!({"type": "object", "properties": {}});
        }
        if obj.get("type").is_none() {
            obj["type"] = Value::String("function".to_string());
        }
        return Some(obj);
    }
    None
}

fn convert_single_tool(t: &Value) -> Option<Value> {
    if let Some(func) = t.get("function") {
        convert_function_tool(func)
    } else {
        convert_generic_tool(t)
    }
}

fn convert_tools(req_tools: Option<&[Value]>) -> Vec<Value> {
    let Some(tools) = req_tools else {
        return Vec::new();
    };
    tools.iter().filter_map(convert_single_tool).collect()
}

fn parse_obj_tool_choice(obj: &serde_json::Map<String, Value>) -> Value {
    if let Some(func) = obj
        .get("function")
        .and_then(|f| f.get("name"))
        .and_then(|n| n.as_str())
    {
        serde_json::json!({
            "type": "tool",
            "toolName": func
        })
    } else {
        serde_json::json!({"type": "auto"})
    }
}

fn convert_tool_choice(choice: Option<&Value>) -> Value {
    match choice {
        Some(Value::String(s)) => match s.as_str() {
            "none" => serde_json::json!({"type": "none"}),
            "required" => serde_json::json!({"type": "required"}),
            _ => serde_json::json!({"type": "auto"}),
        },
        Some(Value::Object(obj)) => parse_obj_tool_choice(obj),
        _ => serde_json::json!({"type": "auto"}),
    }
}

fn convert_assistant_tool_call(tc: &Value) -> Value {
    let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let name = tc
        .get("function")
        .and_then(|f| f.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let raw_args = tc.get("function").and_then(|f| f.get("arguments"));
    let input_val = match raw_args {
        Some(Value::String(s)) => {
            serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.clone()))
        }
        Some(other) => other.clone(),
        None => serde_json::json!({}),
    };
    serde_json::json!({
        "type": "tool-call",
        "toolCallId": id,
        "toolName": name,
        "input": input_val,
    })
}

fn convert_assistant_message(text: &str, tool_calls: Option<&Vec<Value>>) -> Value {
    let mut parts: Vec<Value> = Vec::new();
    if !text.is_empty() {
        parts.push(serde_json::json!({
            "type": "text",
            "text": text,
        }));
    }
    if let Some(calls) = tool_calls {
        for tc in calls {
            parts.push(convert_assistant_tool_call(tc));
        }
    }
    serde_json::json!({
        "role": "assistant",
        "content": parts,
    })
}

fn convert_tool_message(m: &OpenAIMessage, text: &str) -> Value {
    let tool_call_id = m.tool_call_id.as_deref().unwrap_or("");
    let tool_name = m.name.as_deref().unwrap_or("unknown");
    serde_json::json!({
        "role": "tool",
        "content": [{
            "type": "tool-result",
            "toolCallId": tool_call_id,
            "toolName": tool_name,
            "output": {
                "type": "text",
                "value": text,
            }
        }]
    })
}

fn convert_single_message(m: &OpenAIMessage) -> Value {
    let text = openproxy_types::message::extract_content_text(&m.content);
    match m.role.as_str() {
        "system" => serde_json::json!({
            "role": "system",
            "content": text,
        }),
        "assistant" => convert_assistant_message(&text, m.tool_calls.as_ref()),
        "tool" => convert_tool_message(m, &text),
        role => serde_json::json!({
            "role": role,
            "content": [{
                "type": "text",
                "text": text,
            }],
        }),
    }
}

fn convert_messages(messages: &[OpenAIMessage]) -> Vec<Value> {
    messages.iter().map(convert_single_message).collect()
}

impl ProviderAdapter for FxAdapter {
    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn metadata(&self) -> ProviderMetadata {
        let mut meta = ProviderMetadata::custom_default();
        meta.built_in = true;
        meta.deletable = false;
        meta.supports_quota = false;
        meta
    }

    fn is_anonymous_fallback(&self) -> bool {
        true
    }

    fn build_chat_url(&self, _target_format: TargetFormat, _model: &ModelId) -> String {
        self.config.base_url.clone()
    }

    fn build_headers(
        &self,
        api_key: &str,
        _target_format: TargetFormat,
        model: &ModelId,
    ) -> Vec<(String, String)> {
        let mut headers = FxSpoofer.headers();
        let key = if api_key.is_empty() {
            "fx-demo-proxy"
        } else {
            api_key
        };
        let mut auth = String::with_capacity(7 + key.len());
        auth.push_str("Bearer ");
        auth.push_str(key);
        headers.push(("authorization".into(), auth));
        headers.push(("content-type".into(), "application/json".into()));
        headers.push(("ai-language-model-id".into(), model.as_str().to_string()));
        headers
    }

    fn format_request(
        &self,
        _target_format: TargetFormat,
        req: &OpenAIRequest,
        _model: &ModelId,
        messages: &[OpenAIMessage],
        _stream: bool,
    ) -> std::result::Result<bytes::Bytes, CoreError> {
        let prompt = convert_messages(messages);
        let tools = convert_tools(req.tools.as_deref());
        let tool_choice = convert_tool_choice(req.tool_choice.as_ref());

        let body = serde_json::json!({
            "prompt": prompt,
            "tools": tools,
            "toolChoice": tool_choice,
            "headers": {
                "user-agent": "fx/0.0.4"
            }
        });

        serde_json::to_vec(&body)
            .map(bytes::Bytes::from)
            .map_err(|e| CoreError::Parse(format!("serialize fx request: {e}")))
    }

    fn translate_non_streaming_response(
        &self,
        _target_format: TargetFormat,
        response_body: Value,
    ) -> std::result::Result<OpenAIResponse, CoreError> {
        let text = response_body
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|s| s.as_str())
            .or_else(|| response_body.get("content").and_then(|s| s.as_str()))
            .unwrap_or("");

        Ok(OpenAIResponse {
            id: format!("chatcmpl_{}", uuid::Uuid::new_v4()),
            object: "chat.completion".to_string(),
            created: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
            model: "zai/glm-5.2".to_string(),
            choices: vec![openproxy_types::OpenAIChoice {
                index: 0,
                message: OpenAIMessage {
                    role: "assistant".to_string(),
                    content: Some(Value::String(text.to_string())),
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                    extra: serde_json::Map::new(),
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: None,
        })
    }

    fn fetch_models(
        &self,
        _upstream_client: &Arc<UpstreamClient>,
        _api_key: &str,
    ) -> impl std::future::Future<Output = Result<Vec<DiscoveredModel>>> + Send {
        let make_model = |id: &'static str, name: &'static str, ctx: i64| DiscoveredModel {
            model_id: ModelId::new(id),
            display_name: Some(name.into()),
            target_format: TargetFormat::Fx,
            context_length: Some(ctx),
            max_output_tokens: Some(8_192),
            input_modalities: Some(vec!["text".into()].into()),
            output_modalities: Some(vec!["text".into()].into()),
            model_type: Some("chat".into()),
            family: Some("glm".into()),
            capabilities: None,
        };

        std::future::ready(Ok(vec![
            make_model("zai/glm-5.2", "GLM 5.2 (Free)", 1_000_000),
            make_model("zai/glm-5.2-fast", "GLM 5.2 Fast (Free)", 1_000_000),
            make_model("zai/glm-5.3", "GLM 5.3", 1_000_000),
            make_model("zai/glm-5-turbo", "GLM 5 Turbo", 202_800),
            make_model("zai/glm-5.1", "GLM 5.1", 202_800),
            make_model("zai/glm-5v-turbo", "GLM 5V Turbo (Vision)", 200_000),
            make_model("zai/glm-4.7-flashx", "GLM 4.7 FlashX", 200_000),
        ]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fx_adapter_config_and_headers() {
        let adapter = FxAdapter::new();
        assert_eq!(adapter.id().as_str(), "fx");
        assert!(adapter.is_anonymous_fallback());

        let headers = adapter.build_headers("", TargetFormat::Fx, &ModelId::new("zai/glm-5.2"));
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "authorization" && v == "Bearer fx-demo-proxy")
        );
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "ai-language-model-id" && v == "zai/glm-5.2")
        );
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "origin" && v == "https://fx.sh")
        );
    }

    #[test]
    fn test_fx_format_request_with_tools() {
        let adapter = FxAdapter::new();
        let req = OpenAIRequest {
            model: "zai/glm-5.2".into(),
            messages: vec![
                OpenAIMessage {
                    role: "system".into(),
                    content: Some(Value::String("sys prompt".into())),
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                    extra: serde_json::Map::new(),
                },
                OpenAIMessage {
                    role: "user".into(),
                    content: Some(Value::String("user question".into())),
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                    extra: serde_json::Map::new(),
                },
            ],
            stream: true,
            temperature: None,
            top_p: None,
            max_tokens: None,
            tools: Some(vec![serde_json::json!({
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": "Reads a file",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string"}
                        },
                        "required": ["path"]
                    }
                }
            })]),
            tool_choice: Some(serde_json::json!("auto")),
            stop: None,
            extra: serde_json::Map::new(),
            top_k: None,
            user: None,
        };

        let formatted = adapter
            .format_request(
                TargetFormat::Fx,
                &req,
                &ModelId::new("zai/glm-5.2"),
                &req.messages,
                true,
            )
            .expect("format ok");

        let val: Value = serde_json::from_slice(&formatted).unwrap();
        assert!(val.get("prompt").is_some());
        assert_eq!(val["prompt"].as_array().unwrap().len(), 2);

        let tools = val["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"].as_str(), Some("read_file"));
        assert_eq!(tools[0]["description"].as_str(), Some("Reads a file"));
        assert!(tools[0]["inputSchema"].is_object());
        assert_eq!(val["toolChoice"]["type"].as_str(), Some("auto"));
    }
}
