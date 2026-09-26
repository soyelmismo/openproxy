//! Wire types for the OpenAI Responses protocol surface that
//! openproxy exposes at `POST /v1/responses`.
//!
//! See `docs/specs/antigravity-gaps-p2.md` §3 (GAP-2) for the spec.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// `POST /v1/responses` request body.
///
/// Mirrors the public OpenAI Responses API shape: a list of `input` items
/// (each tagged with its `type`), an optional `instructions` string
/// prepended as a system message, and pass-through `tools` / `tool_choice`
/// fields that share the OpenAI function-calling shape verbatim.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesRequest {
    /// Model name to route through the pipeline.
    pub model: String,
    /// Optional system instructions (prepended as a `system` message).
    #[serde(default)]
    pub instructions: Option<String>,
    /// Ordered list of input items.
    #[serde(default, deserialize_with = "deserialize_responses_input")]
    pub input: Vec<ResponsesInputItem>,
    /// Tools (pass-through; Responses and OpenAI share the
    /// `{type:"function", function:{...}}` shape).
    #[serde(default)]
    pub tools: Option<Vec<Value>>,
    #[serde(default)]
    pub tool_choice: Option<Value>,
    #[serde(default)]
    pub stream: bool,
    /// Responses max output tokens, maps to OpenAI max_tokens.
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub top_p: Option<f32>,
    /// Stored-response chain. Not implemented in MVP — we log a warning
    /// and proceed without it.
    #[serde(default)]
    pub previous_response_id: Option<String>,
    /// Unknown fields are preserved verbatim so the proxy can pass them
    /// to upstreams that understand them.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// One entry in the `input[]` array.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponsesInputItem {
    /// A plain chat-style message (`role` + `content`).
    Message {
        role: String,
        content: ResponsesContent,
    },
    /// Assistant-side function call emission (re-injected into the
    /// conversation history to restore assistant tool-call state).
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    /// Tool-side function result (re-injected to restore tool results).
    FunctionCallOutput { call_id: String, output: String },
    /// Forward-compatible: new item types the proxy doesn't know
    /// about are dropped (with a debug log).
    Unknown,
}

impl<'de> Deserialize<'de> for ResponsesInputItem {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        Ok(match value {
            Value::String(s) => ResponsesInputItem::Message {
                role: "user".to_string(),
                content: ResponsesContent::Plain(s),
            },
            Value::Object(mut map) => {
                let type_tag = map
                    .get("type")
                    .and_then(|v| v.as_str())
                    .map(str::to_lowercase);
                match type_tag.as_deref() {
                    Some("message") => {
                        let role = map
                            .get("role")
                            .and_then(|v| v.as_str())
                            .unwrap_or("user")
                            .to_string();
                        let content = map
                            .remove("content")
                            .and_then(|v| serde_json::from_value(v).ok())
                            .unwrap_or_else(|| ResponsesContent::Plain(String::new()));
                        ResponsesInputItem::Message { role, content }
                    }
                    Some("function_call") => {
                        let call_id = map
                            .get("call_id")
                            .or_else(|| map.get("id"))
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string();
                        let name = map
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string();
                        let arguments = map
                            .get("arguments")
                            .map(|v| {
                                if let Some(s) = v.as_str() {
                                    s.to_string()
                                } else {
                                    v.to_string()
                                }
                            })
                            .unwrap_or_default();
                        ResponsesInputItem::FunctionCall {
                            call_id,
                            name,
                            arguments,
                        }
                    }
                    Some("function_call_output") => {
                        let call_id = map
                            .get("call_id")
                            .or_else(|| map.get("id"))
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string();
                        let output = map
                            .get("output")
                            .map(|v| {
                                if let Some(s) = v.as_str() {
                                    s.to_string()
                                } else {
                                    v.to_string()
                                }
                            })
                            .unwrap_or_default();
                        ResponsesInputItem::FunctionCallOutput { call_id, output }
                    }
                    Some(_) => ResponsesInputItem::Unknown,
                    None => {
                        // Inferred variants without explicit "type":
                        if let Some(role_val) = map.get("role").and_then(|v| v.as_str()) {
                            let role = role_val.to_string();
                            let content = map
                                .remove("content")
                                .and_then(|v| serde_json::from_value(v).ok())
                                .unwrap_or_else(|| ResponsesContent::Plain(String::new()));
                            ResponsesInputItem::Message { role, content }
                        } else if map.contains_key("call_id") && map.contains_key("output") {
                            let call_id = map
                                .get("call_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string();
                            let output = map
                                .get("output")
                                .map(|v| {
                                    if let Some(s) = v.as_str() {
                                        s.to_string()
                                    } else {
                                        v.to_string()
                                    }
                                })
                                .unwrap_or_default();
                            ResponsesInputItem::FunctionCallOutput { call_id, output }
                        } else if (map.contains_key("call_id") || map.contains_key("id"))
                            && (map.contains_key("name") || map.contains_key("arguments"))
                        {
                            let call_id = map
                                .get("call_id")
                                .or_else(|| map.get("id"))
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string();
                            let name = map
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string();
                            let arguments = map
                                .get("arguments")
                                .map(|v| {
                                    if let Some(s) = v.as_str() {
                                        s.to_string()
                                    } else {
                                        v.to_string()
                                    }
                                })
                                .unwrap_or_default();
                            ResponsesInputItem::FunctionCall {
                                call_id,
                                name,
                                arguments,
                            }
                        } else {
                            ResponsesInputItem::Unknown
                        }
                    }
                }
            }
            _ => ResponsesInputItem::Unknown,
        })
    }
}

/// Helper deserializer for `ResponsesRequest.input` supporting both a bare string or an array of items.
pub fn deserialize_responses_input<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<ResponsesInputItem>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct InputVisitor;

    impl<'de> serde::de::Visitor<'de> for InputVisitor {
        type Value = Vec<ResponsesInputItem>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a string or an array of input items")
        }

        fn visit_str<E>(self, v: &str) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(vec![ResponsesInputItem::Message {
                role: "user".to_string(),
                content: ResponsesContent::Plain(v.to_string()),
            }])
        }

        fn visit_seq<A>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            let mut items = Vec::with_capacity(seq.size_hint().unwrap_or(0));
            while let Some(item) = seq.next_element::<ResponsesInputItem>()? {
                items.push(item);
            }
            Ok(items)
        }
    }

    deserializer.deserialize_any(InputVisitor)
}

/// Message content can be a plain string OR an array of parts
/// (e.g. `[{type:"input_text", text:"hi"}, {type:"input_image", ...}]`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponsesContent {
    Plain(String),
    Parts(Vec<Value>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deepseek_harness_payload() {
        let raw = r#"{
            "model":"nerd",
            "input":[
                {
                    "role":"system",
                    "content":"Create a concise title for an AI coding-assistant session."
                },
                {
                    "role":"user",
                    "content":[{"type":"input_text","text":"Generate the session title"}]
                }
            ],
            "stream":true,
            "prompt_cache_key":"session-123",
            "store":false,
            "max_output_tokens":64
        }"#;

        let req: ResponsesRequest = serde_json::from_str(raw).expect("parse deepseek payload");
        assert_eq!(req.model, "nerd");
        assert_eq!(req.max_output_tokens, Some(64));
        assert!(req.stream);
        assert_eq!(req.input.len(), 2);

        match &req.input[0] {
            ResponsesInputItem::Message { role, content } => {
                assert_eq!(role, "system");
                match content {
                    ResponsesContent::Plain(text) => {
                        assert_eq!(
                            text,
                            "Create a concise title for an AI coding-assistant session."
                        );
                    }
                    _ => panic!("expected Plain content"),
                }
            }
            _ => panic!("expected Message item"),
        }

        match &req.input[1] {
            ResponsesInputItem::Message { role, content } => {
                assert_eq!(role, "user");
                match content {
                    ResponsesContent::Parts(parts) => {
                        assert_eq!(parts.len(), 1);
                        assert_eq!(parts[0]["type"], "input_text");
                        assert_eq!(parts[0]["text"], "Generate the session title");
                    }
                    _ => panic!("expected Parts content"),
                }
            }
            _ => panic!("expected Message item"),
        }
    }

    #[test]
    fn test_bare_string_input() {
        let raw = r#"{"model":"nerd","input":"tell me a joke"}"#;
        let req: ResponsesRequest = serde_json::from_str(raw).expect("parse bare string input");
        assert_eq!(req.input.len(), 1);
        match &req.input[0] {
            ResponsesInputItem::Message { role, content } => {
                assert_eq!(role, "user");
                match content {
                    ResponsesContent::Plain(t) => assert_eq!(t, "tell me a joke"),
                    _ => panic!("expected Plain"),
                }
            }
            _ => panic!("expected Message"),
        }
    }
}
