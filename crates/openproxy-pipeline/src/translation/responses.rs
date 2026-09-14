//! Translation from OpenAI Responses API envelope to standard OpenAI chat completion response.

use openproxy_types::{CoreError, OpenAIChoice, OpenAIMessage, OpenAIResponse, OpenAIUsage, Result};
use serde_json::Value;

/// Translate an upstream OpenAI Responses API JSON response into standard [`OpenAIResponse`].
pub fn responses_to_openai(val: &Value, fallback_model: &str) -> Result<OpenAIResponse> {
    if let Some(error) = val.get("error")
        && !error.is_null()
    {
        let msg = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_else(|| error.as_str().unwrap_or("unknown responses error"));
        return Err(CoreError::upstream_error(
            500,
            "responses",
            fallback_model,
            msg,
            false,
        ));
    }

    let id = val
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("resp_chatcmpl")
        .to_string();

    let created = val
        .get("created_at")
        .or_else(|| val.get("created"))
        .and_then(Value::as_u64)
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs())
        });

    let model = val
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(fallback_model)
        .to_string();

    let mut full_text = String::new();
    let mut tool_calls = Vec::new();
    let mut role = "assistant".to_string();

    if let Some(output) = val.get("output").and_then(Value::as_array) {
        for item in output {
            let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
            if item_type == "message" {
                if let Some(r) = item.get("role").and_then(Value::as_str) {
                    role = r.to_string();
                }
                if let Some(content) = item.get("content").and_then(Value::as_array) {
                    for part in content {
                        let part_type = part.get("type").and_then(Value::as_str).unwrap_or("");
                        if (part_type == "output_text" || part_type == "text")
                            && let Some(text) = part.get("text").and_then(Value::as_str)
                        {
                            full_text.push_str(text);
                        }
                    }
                } else if let Some(text) = item.get("content").and_then(Value::as_str) {
                    full_text.push_str(text);
                }
            } else if item_type == "function_call" {
                let call_id = item
                    .get("call_id")
                    .or_else(|| item.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or("call_unknown")
                    .to_string();
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let arguments = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("{}")
                    .to_string();

                tool_calls.push(serde_json::json!({
                    "id": call_id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": arguments
                    }
                }));
            }
        }
    }

    let finish_reason = if !tool_calls.is_empty() {
        Some("tool_calls".to_string())
    } else {
        Some("stop".to_string())
    };

    let usage = val.get("usage").map(|u| {
        let prompt_tokens = u
            .get("input_tokens")
            .or_else(|| u.get("prompt_tokens"))
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0);
        let completion_tokens = u
            .get("output_tokens")
            .or_else(|| u.get("completion_tokens"))
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0);
        let total_tokens = u
            .get("total_tokens")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(prompt_tokens + completion_tokens);

        let cached_tokens = u
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .or_else(|| {
                u.get("prompt_tokens_details")
                    .and_then(|d| d.get("cached_tokens"))
            })
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok());

        let prompt_tokens_details =
            cached_tokens.map(|cached| openproxy_types::PromptTokensDetails {
                cached_tokens: Some(cached),
            });

        OpenAIUsage {
            prompt_tokens,
            completion_tokens,
            total_tokens,
            prompt_tokens_details,
        }
    });

    let tool_calls_opt = if tool_calls.is_empty() {
        None
    } else {
        Some(tool_calls)
    };

    let content_opt = if full_text.is_empty() && tool_calls_opt.is_some() {
        None
    } else {
        Some(Value::String(full_text))
    };

    Ok(OpenAIResponse {
        id,
        object: "chat.completion".to_string(),
        created,
        model,
        choices: vec![OpenAIChoice {
            index: 0,
            message: OpenAIMessage {
                role,
                content: content_opt,
                name: None,
                tool_call_id: None,
                tool_calls: tool_calls_opt,
                extra: Default::default(),
            },
            finish_reason,
        }],
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_responses_to_openai_translation() {
        let raw = serde_json::json!({
            "id": "resp_123",
            "object": "response",
            "created_at": 1789385067,
            "status": "completed",
            "model": "muse-spark-1.3-contributor-free",
            "output": [
                {
                    "id": "rs_1",
                    "type": "reasoning",
                    "status": "completed"
                },
                {
                    "id": "msg_1",
                    "type": "message",
                    "status": "completed",
                    "role": "assistant",
                    "content": [
                        {
                            "type": "output_text",
                            "text": "Hello world!"
                        }
                    ]
                }
            ],
            "usage": {
                "input_tokens": 10,
                "output_tokens": 20,
                "total_tokens": 30
            }
        });

        let resp = responses_to_openai(&raw, "fallback-model").expect("translation succeeded");
        assert_eq!(resp.id, "resp_123");
        assert_eq!(resp.model, "muse-spark-1.3-contributor-free");
        assert_eq!(resp.created, 1789385067);
        assert_eq!(resp.choices.len(), 1);
        assert_eq!(
            resp.choices[0].message.content,
            Some(Value::String("Hello world!".to_string()))
        );
        assert_eq!(resp.choices[0].finish_reason, Some("stop".to_string()));

        let usage = resp.usage.expect("usage exists");
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 20);
        assert_eq!(usage.total_tokens, 30);
    }

    #[test]
    fn test_responses_to_openai_tool_calls() {
        let raw = serde_json::json!({
            "id": "resp_tc",
            "created": 1789385000,
            "model": "gpt-5-test",
            "output": [
                {
                    "id": "fc_1",
                    "type": "function_call",
                    "name": "get_weather",
                    "arguments": "{\"city\":\"Paris\"}",
                    "call_id": "call_123"
                }
            ]
        });

        let resp = responses_to_openai(&raw, "fallback").expect("translated tool calls");
        assert_eq!(resp.choices[0].finish_reason, Some("tool_calls".to_string()));
        let tool_calls = resp.choices[0].message.tool_calls.as_ref().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0]["id"], "call_123");
        assert_eq!(tool_calls[0]["function"]["name"], "get_weather");
    }
}
