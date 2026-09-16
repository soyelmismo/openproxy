//! Stateful event translation with tool use accumulator.

use super::super::UpstreamSseChunk;
use super::accumulator::{AnthropicToolUseAccumulator, MAX_TOOL_ARGUMENTS_BYTES};
use super::payload::translate_anthropic_sse_payload;
use openproxy_types::error::{CoreError, Result};

#[derive(serde::Deserialize, Default)]
struct AnthropicContentBlockDeltaProbe {
    #[serde(default)]
    delta: Option<AnthropicDeltaProbe>,
}

#[derive(serde::Deserialize, Default)]
struct AnthropicDeltaProbe {
    #[serde(default, rename = "type")]
    delta_type: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default, rename = "partial_json")]
    partial_json: Option<String>,
}

#[derive(serde::Deserialize, Default)]
struct AnthropicContentBlockStartProbe {
    #[serde(default)]
    content_block: Option<AnthropicContentBlockProbe>,
}

#[derive(serde::Deserialize, Default)]
struct AnthropicContentBlockProbe {
    #[serde(default, rename = "type")]
    block_type: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

fn handle_anthropic_input_json_delta(
    delta: &AnthropicDeltaProbe,
    chunk_id: &str,
    created: u64,
    model: &str,
    acc: &mut AnthropicToolUseAccumulator,
) -> UpstreamSseChunk {
    let prev_len = acc.arguments.len();
    if let Some(partial) = delta.partial_json.as_deref()
        && prev_len + partial.len() <= MAX_TOOL_ARGUMENTS_BYTES
    {
        acc.arguments.push_str(partial);
    }
    let new_fragment = &acc.arguments[prev_len..];
    let chunk = serde_json::json!({
        "id": chunk_id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": acc.index,
                    "function": {
                        "arguments": new_fragment
                    }
                }]
            },
            "finish_reason": null
        }]
    });
    let tool_call_obj = serde_json::json!({
        "index": acc.index,
        "function": {
            "arguments": new_fragment,
        }
    });
    UpstreamSseChunk {
        raw_payload: None,
        payload: chunk,
        done: false,
        usage: None,
        stop_reason: None,
        delta_reasoning: None,
        delta_tool_calls: vec![tool_call_obj],
        has_content: true,
    }
}

fn handle_anthropic_thinking_delta(
    delta: &AnthropicDeltaProbe,
    chunk_id: &str,
    created: u64,
    model: &str,
) -> Option<UpstreamSseChunk> {
    let thinking = delta.thinking.as_deref().unwrap_or("");
    if thinking.is_empty() {
        return None;
    }
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
    Some(UpstreamSseChunk {
        raw_payload: None,
        payload: chunk,
        done: false,
        usage: None,
        stop_reason: None,
        delta_reasoning: Some(thinking.to_string()),
        delta_tool_calls: Vec::new(),
        has_content: true,
    })
}

fn handle_anthropic_text_delta(
    delta: &AnthropicDeltaProbe,
    chunk_id: &str,
    created: u64,
    model: &str,
) -> Option<UpstreamSseChunk> {
    let text = delta.text.as_deref().unwrap_or("");
    if text.is_empty() {
        return None;
    }
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

fn translate_anthropic_content_block_delta(
    data_json: &str,
    chunk_id: &str,
    created: u64,
    model: &str,
    tool_use_acc: &mut Option<AnthropicToolUseAccumulator>,
) -> Result<Option<UpstreamSseChunk>> {
    let probe: AnthropicContentBlockDeltaProbe = serde_json::from_str(data_json)
        .map_err(|e| CoreError::Parse(format!("anthropic sse json: {e}")))?;
    let delta = probe.delta.unwrap_or_default();
    let delta_type = delta.delta_type.as_deref().unwrap_or("");

    let chunk = match delta_type {
        "input_json_delta" => tool_use_acc
            .as_mut()
            .map(|acc| handle_anthropic_input_json_delta(&delta, chunk_id, created, model, acc)),
        "thinking_delta" => handle_anthropic_thinking_delta(&delta, chunk_id, created, model),
        _ => handle_anthropic_text_delta(&delta, chunk_id, created, model),
    };

    Ok(chunk)
}

fn translate_anthropic_content_block_start(
    data_json: &str,
    chunk_id: &str,
    created: u64,
    model: &str,
    tool_use_acc: &mut Option<AnthropicToolUseAccumulator>,
    tool_call_index_counter: &mut u32,
) -> Result<Option<UpstreamSseChunk>> {
    let probe: AnthropicContentBlockStartProbe = serde_json::from_str(data_json)
        .map_err(|e| CoreError::Parse(format!("anthropic sse json: {e}")))?;
    let block = probe.content_block.unwrap_or_default();
    let block_type = block.block_type.as_deref().unwrap_or("");

    if block_type != "tool_use" {
        return Ok(None);
    }

    let id = block.id.unwrap_or_default();
    let name = block.name.unwrap_or_default();
    let index = *tool_call_index_counter;
    *tool_call_index_counter += 1;

    *tool_use_acc = Some(AnthropicToolUseAccumulator::new_with_bounds(
        index,
        id.clone(),
        name.clone(),
    )?);

    let chunk = serde_json::json!({
        "id": chunk_id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": index,
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": ""
                    }
                }]
            },
            "finish_reason": null
        }]
    });

    let tool_call_obj = serde_json::json!({
        "index": index,
        "id": id,
        "type": "function",
        "function": {
            "name": name,
            "arguments": ""
        }
    });

    Ok(Some(UpstreamSseChunk {
        raw_payload: None,
        payload: chunk,
        done: false,
        usage: None,
        stop_reason: None,
        delta_reasoning: None,
        delta_tool_calls: vec![tool_call_obj],
        has_content: false,
    }))
}

pub fn translate_anthropic_sse_event(
    payload: &str,
    chunk_id: &str,
    created: u64,
    model: &str,
    tool_use_acc: &mut Option<AnthropicToolUseAccumulator>,
    tool_call_index_counter: &mut u32,
) -> Result<Option<UpstreamSseChunk>> {
    let Some((event_type, data_json)) = payload.split_once('\n') else {
        return Ok(None);
    };

    match event_type {
        "ping" => Ok(None),
        "content_block_delta" => translate_anthropic_content_block_delta(
            data_json,
            chunk_id,
            created,
            model,
            tool_use_acc,
        ),
        "content_block_start" => translate_anthropic_content_block_start(
            data_json,
            chunk_id,
            created,
            model,
            tool_use_acc,
            tool_call_index_counter,
        ),
        "content_block_stop" => {
            *tool_use_acc = None;
            Ok(None)
        }
        _ => {
            let rebuilt = format!("{event_type}\n{data_json}");
            translate_anthropic_sse_payload(&rebuilt, chunk_id, created, model)
        }
    }
}
