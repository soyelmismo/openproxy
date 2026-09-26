//! Stateless payload translation from Anthropic SSE wire format into OpenAI SSE chunks.

use super::super::UpstreamSseChunk;
use openproxy_types::error::{CoreError, Result};
use openproxy_types::message::{OpenAIUsage, PromptTokensDetails};
use serde_json::Value;

/// Translate a single Anthropic SSE payload (event_type + data JSON) into
/// an OpenAI-compatible SSE chunk string.
///
/// The payload format is "event_type\njson_data".
pub(crate) fn build_anthropic_message_start_chunk(
    chunk_id: &str,
    created: u64,
    model: &str,
    data: &Value,
) -> UpstreamSseChunk {
    let usage = data.get("message").and_then(|m| m.get("usage")).map(|u| {
        let input_tokens = u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
        let output_tokens = u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
        let cache_read = u
            .get("cache_read_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let cache_creation = u
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let prompt_total = input_tokens
            .saturating_add(cache_read)
            .saturating_add(cache_creation);
        let total = prompt_total.saturating_add(output_tokens);
        OpenAIUsage {
            prompt_tokens: u32::try_from(prompt_total).unwrap_or(u32::MAX),
            completion_tokens: u32::try_from(output_tokens).unwrap_or(u32::MAX),
            total_tokens: u32::try_from(total).unwrap_or(u32::MAX),
            prompt_tokens_details: Some(PromptTokensDetails {
                cached_tokens: u32::try_from(cache_read).ok(),
            }),
        }
    });

    let chunk = serde_json::json!({
        "id": chunk_id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": {"role": "assistant", "content": ""},
            "finish_reason": null
        }]
    });
    UpstreamSseChunk {
        raw_payload: None,
        payload: chunk,
        done: false,
        usage,
        stop_reason: None,
        delta_reasoning: None,
        delta_tool_calls: Vec::new(),
        has_content: false,
    }
}

pub(crate) fn translate_anthropic_content_delta(
    data: &Value,
    chunk_id: &str,
    created: u64,
    model: &str,
) -> Option<UpstreamSseChunk> {
    let delta_type = data
        .get("delta")
        .and_then(|d| d.get("type"))
        .and_then(|t| t.as_str())
        .unwrap_or("text_delta");

    if delta_type == "thinking_delta" {
        let thinking = data
            .get("delta")
            .and_then(|d| d.get("thinking"))
            .and_then(|t| t.as_str())
            .filter(|s| !s.is_empty())?;

        let chunk = serde_json::json!({
            "id": chunk_id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [{
                "index": 0,
                "delta": {"content": ""},
                "finish_reason": null
            }]
        });
        return Some(UpstreamSseChunk {
            raw_payload: None,
            payload: chunk,
            done: false,
            usage: None,
            stop_reason: None,
            delta_reasoning: Some(thinking.to_string()),
            delta_tool_calls: Vec::new(),
            has_content: true,
        });
    }

    let text = data
        .get("delta")
        .and_then(|d| d.get("text"))
        .and_then(|t| t.as_str())
        .filter(|s| !s.is_empty())?;

    let chunk = serde_json::json!({
        "id": chunk_id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": {"content": text},
            "finish_reason": null
        }]
    });
    Some(UpstreamSseChunk {
        raw_payload: None,
        payload: chunk,
        done: false,
        usage: None,
        stop_reason: None,
        delta_reasoning: None,
        delta_tool_calls: Vec::new(),
        has_content: true,
    })
}

pub(crate) fn translate_anthropic_message_delta(
    data: &Value,
    chunk_id: &str,
    created: u64,
    model: &str,
) -> UpstreamSseChunk {
    let stop_reason = data
        .get("delta")
        .and_then(|d| d.get("stop_reason"))
        .or_else(|| data.get("stop_reason"))
        .and_then(|r| r.as_str());

    let finish_reason = match stop_reason {
        Some("end_turn" | "stop_sequence") => Some("stop".to_string()),
        Some("max_tokens") => Some("length".to_string()),
        Some("tool_use" | "tool_call" | "tool_calls" | "toolUse" | "toolCall" | "toolCalls") => {
            Some("tool_calls".to_string())
        }
        _ => None,
    };

    let usage = {
        let usage_block = data.get("usage");
        let input_present = usage_block.and_then(|u| u.get("input_tokens")).is_some();
        let output_tokens = usage_block
            .and_then(|u| u.get("output_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);

        if input_present {
            let input_tokens = usage_block
                .and_then(|u| u.get("input_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let cache_read = usage_block
                .and_then(|u| u.get("cache_read_input_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let cache_creation = usage_block
                .and_then(|u| u.get("cache_creation_input_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let prompt_total = input_tokens
                .saturating_add(cache_read)
                .saturating_add(cache_creation);
            let total = prompt_total.saturating_add(output_tokens);
            Some(OpenAIUsage {
                prompt_tokens: u32::try_from(prompt_total).unwrap_or(u32::MAX),
                completion_tokens: u32::try_from(output_tokens).unwrap_or(u32::MAX),
                total_tokens: u32::try_from(total).unwrap_or(u32::MAX),
                prompt_tokens_details: Some(PromptTokensDetails {
                    cached_tokens: u32::try_from(cache_read).ok(),
                }),
            })
        } else if output_tokens > 0 {
            Some(OpenAIUsage {
                prompt_tokens: 0,
                completion_tokens: u32::try_from(output_tokens).unwrap_or(u32::MAX),
                total_tokens: 0,
                prompt_tokens_details: None,
            })
        } else {
            None
        }
    };

    let chunk = serde_json::json!({
        "id": chunk_id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": finish_reason
        }]
    });
    UpstreamSseChunk {
        raw_payload: None,
        payload: chunk,
        done: true,
        usage,
        stop_reason: stop_reason.map(std::string::ToString::to_string),
        delta_reasoning: None,
        delta_tool_calls: Vec::new(),
        has_content: false,
    }
}

pub fn translate_anthropic_sse_payload(
    payload: &str,
    chunk_id: &str,
    created: u64,
    model: &str,
) -> Result<Option<UpstreamSseChunk>> {
    let Some((event_type, data_json)) = payload.split_once('\n') else {
        return Ok(None);
    };

    if event_type == "ping" || event_type == "message_stop" {
        return Ok(None);
    }

    let data: Value = serde_json::from_str(data_json)
        .map_err(|e| CoreError::Parse(format!("anthropic sse json: {e}")))?;

    match event_type {
        "message_start" => Ok(Some(build_anthropic_message_start_chunk(
            chunk_id, created, model, &data,
        ))),
        "content_block_delta" => Ok(translate_anthropic_content_delta(
            &data, chunk_id, created, model,
        )),
        "message_delta" => Ok(Some(translate_anthropic_message_delta(
            &data, chunk_id, created, model,
        ))),
        _ => Ok(None),
    }
}
