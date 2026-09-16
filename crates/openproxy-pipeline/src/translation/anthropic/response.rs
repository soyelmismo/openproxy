use crate::translation::types::{AnthropicResponse, OpenAIChoice, OpenAIResponse, OpenAIUsage};
use openproxy_types::OpenAIMessage;
use serde_json::Value;

/// Convert Anthropic response to OpenAI response.
///
/// - `choices[0].message.content` = concatenation of all text content blocks.
/// - `usage`: `prompt_tokens=input_tokens`, `completion_tokens=output_tokens`,
///   `total_tokens=sum`.
/// - `finish_reason` mapped from `stop_reason` using Anthropic -> OpenAI semantics.
pub fn anthropic_to_openai(resp: &AnthropicResponse) -> OpenAIResponse {
    let combined: String = resp
        .content
        .iter()
        .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join("");

    let cache_read = resp.usage.cache_read_input_tokens.unwrap_or(0);
    let cache_creation = resp.usage.cache_creation_input_tokens.unwrap_or(0);
    let prompt_tokens = resp
        .usage
        .input_tokens
        .saturating_add(cache_read)
        .saturating_add(cache_creation);
    let completion_tokens = resp.usage.output_tokens;
    let total_tokens = prompt_tokens.saturating_add(completion_tokens);

    let message = OpenAIMessage {
        role: "assistant".to_string(),
        content: Some(Value::String(combined)),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        extra: serde_json::Map::new(),
    };

    let choice = OpenAIChoice {
        index: 0,
        message,
        finish_reason: resp.stop_reason.as_deref().map(map_finish_reason),
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

/// Map an Anthropic stop_reason value to an OpenAI finish_reason value.
pub fn map_finish_reason(stop_reason: &str) -> String {
    match stop_reason {
        "end_turn" => "stop".to_string(),
        "max_tokens" => "length".to_string(),
        "tool_use" => "tool_calls".to_string(),
        // stop_sequence and unknown values fall back to "stop".
        other => {
            // Treat anything unknown as "stop" to stay close to OpenAI's vocabulary.
            let _ = other;
            "stop".to_string()
        }
    }
}
