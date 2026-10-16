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
    /// Progress estimate the vendor attaches to a `thinking_delta` whose
    /// `thinking` string is empty — i.e. reasoning generated server-side
    /// and withheld from the stream. Not billed usage; see
    /// [`super::super::HIDDEN_THINKING_EVENTS`].
    #[serde(default, rename = "estimated_tokens")]
    estimated_tokens: Option<i64>,
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
        // An empty `thinking` with a positive `estimated_tokens` estimate is
        // the vendor reporting reasoning it withholds from the stream. The
        // event used to be dropped here, so the pipeline never learned the
        // upstream was generating and the TTFT gate kept counting to its
        // deadline. Return liveness instead: the pipeline forwards nothing
        // but keeps the body stream alive. Zero, negative and non-integer
        // estimates are not progress claims and stay dropped.
        if delta.estimated_tokens.is_some_and(|n| n > 0) {
            return Some(UpstreamSseChunk::liveness());
        }
        return None;
    }
    Some(crate::sse::make_text_delta(
        chunk_id, created, model, thinking, true,
    ))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn translate(event: &str, data: &str) -> Result<Option<UpstreamSseChunk>> {
        let payload = format!("{event}\n{data}");
        translate_anthropic_sse_event(
            &payload,
            "chatcmpl-1",
            1_700_000_000,
            "claude-opus-5-5",
            &mut None,
            &mut 0,
        )
    }

    /// Translate an event that must parse, and unwrap its payload.
    fn translate_ok(event: &str, data: &str) -> Option<UpstreamSseChunk> {
        translate(event, data).expect("translation must not fail on a well-formed event")
    }

    /// The vendor reports reasoning it withholds from the stream: empty
    /// `thinking` plus `estimated_tokens`. Must produce liveness, not
    /// `None`, or the TTFT gate keeps counting and the request dies before
    /// the first visible token.
    #[test]
    fn empty_thinking_with_estimated_tokens_is_liveness() {
        let chunk = translate_ok(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"","estimated_tokens":120}}"#,
        )
        .expect("hidden progress must yield a liveness chunk");
        assert!(chunk.is_liveness());
        assert!(!chunk.has_content);
        assert!(!chunk.done);
        assert!(chunk.usage.is_none());
        assert!(chunk.delta_reasoning.is_none());
    }

    /// Empty `thinking` with no estimate carries no progress information, so
    /// there is nothing to report and the event stays dropped.
    #[test]
    fn empty_thinking_without_estimated_tokens_is_dropped() {
        assert!(
            translate_ok(
                "content_block_delta",
                r#"{"index":0,"delta":{"type":"thinking_delta","thinking":""}}"#,
            )
            .is_none()
        );
        assert!(
            translate_ok(
                "content_block_delta",
                r#"{"index":0,"delta":{"type":"thinking_delta"}}"#
            )
            .is_none()
        );
    }

    /// A zero estimate claims no tokens were generated, so it is not a
    /// progress signal and the event stays dropped.
    #[test]
    fn zero_estimated_tokens_is_not_progress() {
        assert!(
            translate_ok(
                "content_block_delta",
                r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"","estimated_tokens":0}}"#,
            )
            .is_none()
        );
    }

    /// A non-numeric estimate makes the event unparseable, which the caller
    /// already treats as non-progress (a logged parse warning).
    #[test]
    fn negative_estimates_are_not_progress() {
        assert!(
            translate_ok(
                "content_block_delta",
                r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"","estimated_tokens":-5}}"#,
            )
            .is_none()
        );
        let parsed = translate(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"","estimated_tokens":"not-a-number"}}"#,
        );
        assert!(parsed.is_err());
    }

    /// Real reasoning text keeps its existing behaviour: forwarded as a
    /// reasoning delta with content.
    #[test]
    fn thinking_with_text_is_still_forwarded() {
        let chunk = translate_ok(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"weighing options","estimated_tokens":120}}"#,
        )
        .expect("visible reasoning must still be forwarded");
        assert!(!chunk.is_liveness());
        assert!(chunk.has_content);
        assert_eq!(chunk.delta_reasoning.as_deref(), Some("weighing options"));
    }

    /// `estimated_tokens` on a *text* delta is not a progress signal: only
    /// the thinking branch interprets it.
    #[test]
    fn estimated_tokens_on_text_delta_is_not_progress() {
        assert!(
            translate_ok(
                "content_block_delta",
                r#"{"index":0,"delta":{"type":"text_delta","text":"","estimated_tokens":120}}"#,
            )
            .is_none()
        );
    }

    /// `input_json_delta` with an empty fragment keeps dropping, exactly as
    /// before: tool arguments are content when they arrive, never progress.
    #[test]
    fn empty_tool_arguments_are_not_progress() {
        assert!(
            translate_ok(
                "content_block_delta",
                r#"{"index":0,"delta":{"type":"input_json_delta","partial_json":""}}"#,
            )
            .is_none()
        );
    }

    /// The liveness chunk is recognised only by the documented shape, so a
    /// final chunk with a `Null` payload is never mistaken for progress.
    #[test]
    fn done_chunk_is_not_liveness() {
        let done = UpstreamSseChunk::done();
        assert!(done.done);
        assert!(!done.is_liveness());
    }
}
