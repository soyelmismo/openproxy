//! Zero-cost extension trait that re-shapes an OpenAI chat-completion
//! response into the OpenAI Responses envelope expected by
//! `POST /v1/responses` clients.
//!
//! Lives here (and not in `openproxy-types`) because the conversion is
//! wire-format policy specific to the pipeline layer; the types crate
//! stays pure data per AGENTS.md §2.
//!
//! See `docs/specs/antigravity-gaps-p2.md` §3.3.4 (GAP-2 N1 fix).

use openproxy_types::OpenAIResponse;
use serde_json::{Value, json};

/// Extension trait for [`OpenAIResponse`] that produces the wire shape
/// expected by the `/v1/responses` endpoint.
pub trait ResponseExt {
    /// Build the Responses-shaped JSON envelope from the internal
    /// chat-completion response. Pure function; allocates a fresh
    /// `serde_json::Value` (one `Vec` per choice + one `Value::Object`).
    fn to_responses_envelope(&self) -> Value;
}

impl ResponseExt for OpenAIResponse {
    fn to_responses_envelope(&self) -> Value {
        let output: Vec<Value> = self
            .choices
            .iter()
            .map(|c| {
                let content_val = match &c.message.content {
                    Some(Value::String(s)) => json!([{ "type": "output_text", "text": s }]),
                    Some(v) => json!([v.clone()]),
                    None => json!([]),
                };
                json!({
                    "type": "message",
                    "role": c.message.role,
                    "content": content_val,
                })
            })
            .collect();

        json!({
            "object": "response",
            "id": self.id,
            "created": self.created,
            "model": self.model,
            "output": output,
            "usage": self.usage,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openproxy_types::{OpenAIChoice, OpenAIMessage, OpenAIUsage};

    fn sample_response() -> OpenAIResponse {
        OpenAIResponse {
            id: "chatcmpl-xyz".to_string(),
            object: "chat.completion".to_string(),
            created: 1_700_000_000,
            model: "gpt-x".to_string(),
            choices: vec![OpenAIChoice {
                index: 0,
                message: OpenAIMessage {
                    role: "assistant".to_string(),
                    content: Some(json!("hello world")),
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                    extra: serde_json::Map::new(),
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: Some(OpenAIUsage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                prompt_tokens_details: None,
            }),
        }
    }

    #[test]
    fn envelope_carries_object_response_id_and_output() {
        let v = sample_response().to_responses_envelope();
        assert_eq!(v["object"], "response");
        assert_eq!(v["id"], "chatcmpl-xyz");
        assert_eq!(v["model"], "gpt-x");
        assert_eq!(v["created"], 1_700_000_000u64);
        let output = v["output"].as_array().expect("output[]");
        assert_eq!(output.len(), 1);
        assert_eq!(output[0]["type"], "message");
        assert_eq!(output[0]["role"], "assistant");
        let content = output[0]["content"].as_array().expect("content[]");
        assert_eq!(content[0]["type"], "output_text");
        assert_eq!(content[0]["text"], "hello world");
    }

    #[test]
    fn envelope_handles_no_choices() {
        let mut resp = sample_response();
        resp.choices.clear();
        let v = resp.to_responses_envelope();
        assert_eq!(v["object"], "response");
        assert!(v["output"].as_array().unwrap().is_empty());
    }

    #[test]
    fn envelope_handles_null_content() {
        let mut resp = sample_response();
        resp.choices[0].message.content = None;
        let v = resp.to_responses_envelope();
        let content = v["output"][0]["content"].as_array().expect("content[]");
        assert!(content.is_empty());
    }
}
