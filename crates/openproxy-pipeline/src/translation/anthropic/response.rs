use crate::translation::types::{AnthropicResponse, OpenAIChoice, OpenAIResponse, OpenAIUsage};
use openproxy_types::OpenAIMessage;
use serde_json::Value;

/// `content` concatenates the text blocks, `usage` maps input/output/sum
/// onto prompt/completion/total, and `finish_reason` comes from
pub fn anthropic_to_openai(resp: &AnthropicResponse) -> OpenAIResponse {
    let mut combined = String::new();
    for b in &resp.content {
        if b.get("type").and_then(|t| t.as_str()) == Some("text")
            && let Some(t) = b.get("text").and_then(|t| t.as_str())
        {
            combined.push_str(t);
        }
    }

    let mut tool_calls = Vec::new();
    for b in &resp.content {
        if b.get("type").and_then(|t| t.as_str()) == Some("tool_use")
            && let (Some(id), Some(name), Some(input)) = (
                b.get("id").and_then(|v| v.as_str()),
                b.get("name").and_then(|v| v.as_str()),
                b.get("input"),
            )
        {
            tool_calls.push(serde_json::json!({
                "id": id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string())
                }
            }));
        }
    }

    let cache_read = resp.usage.cache_read_input_tokens.unwrap_or(0);
    let cache_creation = resp.usage.cache_creation_input_tokens.unwrap_or(0);
    let prompt_tokens = resp
        .usage
        .input_tokens
        .saturating_add(cache_read)
        .saturating_add(cache_creation);
    let completion_tokens = resp.usage.output_tokens;
    let total_tokens = prompt_tokens.saturating_add(completion_tokens);

    let has_tools = !tool_calls.is_empty();
    let content = if combined.is_empty() && has_tools {
        None
    } else {
        Some(Value::String(combined))
    };

    let message = OpenAIMessage {
        role: "assistant".to_string(),
        content,
        name: None,
        tool_call_id: None,
        tool_calls: if has_tools { Some(tool_calls) } else { None },
        extra: serde_json::Map::new(),
    };

    let finish_reason = if has_tools {
        Some("tool_calls".to_string())
    } else {
        resp.stop_reason.as_deref().map(map_finish_reason)
    };

    let choice = OpenAIChoice {
        index: 0,
        message,
        finish_reason,
    };

    OpenAIResponse {
        id: resp.id.clone(),
        object: "chat.completion".to_string(),
        created: 0,
        model: resp.model.clone(),
        choices: vec![choice],
        usage: Some(OpenAIUsage {
            prompt_tokens,
            completion_tokens,
            total_tokens,
            prompt_tokens_details: resp.usage.cache_read_input_tokens.map(|c| {
                openproxy_types::message::PromptTokensDetails {
                    cached_tokens: Some(c),
                }
            }),
        }),
    }
}

pub fn map_finish_reason(stop_reason: &str) -> String {
    openproxy_types::map_stop_reason_to_finish_reason(stop_reason).to_string()
}
