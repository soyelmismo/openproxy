//! Stateful event translation with tool use accumulator.

use super::super::UpstreamSseChunk;
use super::accumulator::{AnthropicToolUseAccumulator, MAX_TOOL_ARGUMENTS_BYTES};
use super::payload::translate_anthropic_sse_payload;
use openproxy_types::ResultExt;
use openproxy_types::error::Result;

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
    /// Some vendors (MiniMax) ship the whole `tool_use` arguments object
    /// inline in `content_block_start` instead of streaming
    /// `input_json_delta` fragments. Capture it so the accumulated arguments
    /// are not left empty.
    #[serde(default)]
    input: Option<serde_json::Value>,
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
    let probe: AnthropicContentBlockDeltaProbe =
        serde_json::from_str(data_json).ctx_parse("anthropic sse json")?;
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
    let probe: AnthropicContentBlockStartProbe =
        serde_json::from_str(data_json).ctx_parse("anthropic sse json")?;
    let block = probe.content_block.unwrap_or_default();
    let block_type = block.block_type.as_deref().unwrap_or("");

    if block_type != "tool_use" {
        return Ok(None);
    }

    let id = block.id.unwrap_or_default();
    let name = block.name.unwrap_or_default();
    let index = *tool_call_index_counter;
    *tool_call_index_counter += 1;

    let mut acc = AnthropicToolUseAccumulator::new_with_bounds(index, id.clone(), name.clone())?;
    // Seed inline `input` arguments when the vendor ships them upfront
    // (spec sends `{}` here; MiniMax may send the full object). Any later
    // `input_json_delta` fragments simply append — vendors that seed inline
    // do not also stream deltas for the same block.
    if let Some(input) = block.input.as_ref()
        && let Ok(serialized) = serde_json::to_string(input)
        && !serialized.is_empty()
        && serialized != "{}"
        && serialized.len() <= MAX_TOOL_ARGUMENTS_BYTES
    {
        acc.arguments.push_str(&serialized);
    }
    *tool_use_acc = Some(acc);

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
        "message" => translate_anthropic_full_message(data_json, chunk_id, created, model),
        "error" => translate_anthropic_inline_error(data_json),
        _ => {
            let rebuilt = format!("{event_type}\n{data_json}");
            translate_anthropic_sse_payload(&rebuilt, chunk_id, created, model)
        }
    }
}

/// Some vendors deliver the whole Anthropic message as a single SSE `data:`
/// event (`{"type":"message","content":[...]}`) instead of the incremental
/// `message_start`/`content_block_*`/`message_delta` sequence. Translate it
/// through the unary converter and emit one terminal OpenAI chunk carrying
/// the delta (content, reasoning, tool_calls) plus finish_reason.
fn translate_anthropic_full_message(
    data_json: &str,
    chunk_id: &str,
    created: u64,
    model: &str,
) -> Result<Option<UpstreamSseChunk>> {
    let resp: crate::translation::AnthropicResponse =
        serde_json::from_str(data_json).ctx_parse("anthropic sse full message")?;
    let openai = crate::translation::anthropic_to_openai(&resp);

    let mut delta = serde_json::Map::new();
    delta.insert("role".to_string(), serde_json::json!("assistant"));
    let mut reasoning_out = String::new();
    let mut has_content = false;
    if let Some(choice) = openai.choices.first() {
        if let Some(c) = choice
            .message
            .content
            .as_ref()
            .and_then(serde_json::Value::as_str)
            && !c.is_empty()
        {
            delta.insert("content".to_string(), serde_json::json!(c));
            has_content = true;
        }
        for key in ["reasoning", "reasoning_content"] {
            if let Some(r) = choice
                .message
                .extra
                .get(key)
                .and_then(serde_json::Value::as_str)
                && !r.is_empty()
            {
                delta.insert(key.to_string(), serde_json::json!(r));
                reasoning_out = r.to_string();
                has_content = true;
            }
        }
        if let Some(tc) = choice.message.tool_calls.as_ref() {
            delta.insert("tool_calls".to_string(), serde_json::json!(tc));
            has_content = true;
        }
    }
    let finish_reason = openai
        .choices
        .first()
        .and_then(|c| c.finish_reason.clone())
        .unwrap_or_else(|| "stop".to_string());

    let payload = serde_json::json!({
        "id": chunk_id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": serde_json::Value::Object(delta),
            "finish_reason": finish_reason
        }]
    });

    Ok(Some(UpstreamSseChunk {
        raw_payload: None,
        payload,
        done: true,
        usage: openai.usage,
        stop_reason: resp.stop_reason,
        delta_reasoning: (!reasoning_out.is_empty()).then_some(reasoning_out),
        delta_tool_calls: Vec::new(),
        has_content,
    }))
}

/// Anthropic-style inline error event (`{"type":"error","error":{...}}`)
/// surfaced as a terminal failure instead of a silent drop.
fn translate_anthropic_inline_error(data_json: &str) -> Result<Option<UpstreamSseChunk>> {
    let probe =
        serde_json::from_str::<serde_json::Value>(data_json).ctx_parse("anthropic sse error")?;
    let message = probe
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("upstream error event");
    let err_type = probe
        .get("error")
        .and_then(|e| e.get("type"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("error");
    Err(openproxy_types::error::CoreError::UpstreamConnection(
        format!("{err_type}: {message}"),
    ))
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

    /// MiniMax ships `tool_use` blocks with the full `input` object inline in
    /// `content_block_start` instead of streaming `input_json_delta`
    /// fragments. The accumulator must be seeded so the final arguments are
    /// not silently empty.
    #[test]
    fn tool_use_with_inline_input_seeds_arguments() {
        let mut acc: Option<AnthropicToolUseAccumulator> = None;
        let mut counter = 0u32;
        let payload = format!(
            "{}\n{}",
            "content_block_start",
            r#"{"index":0,"content_block":{"type":"tool_use","id":"call_x","name":"execute_code","input":{"code":"print(1)"}}}"#
        );
        let chunk = translate_anthropic_sse_event(
            &payload,
            "chatcmpl-1",
            1_700_000_000,
            "m",
            &mut acc,
            &mut counter,
        )
        .expect("content_block_start must translate")
        .expect("tool_use start must emit a chunk");
        assert!(!chunk.done);
        let acc = acc.expect("tool_use accumulator must be created");
        assert_eq!(acc.arguments, r#"{"code":"print(1)"}"#);
    }

    /// Spec-compliant `content_block_start` carries `input: {}` — that must
    /// NOT seed garbage into the accumulator; later `input_json_delta`
    /// fragments own the arguments string.
    #[test]
    fn tool_use_with_empty_inline_input_stays_empty() {
        let mut acc: Option<AnthropicToolUseAccumulator> = None;
        let mut counter = 0u32;
        let payload = format!(
            "{}\n{}",
            "content_block_start",
            r#"{"index":0,"content_block":{"type":"tool_use","id":"call_y","name":"read","input":{}}}"#
        );
        translate_anthropic_sse_event(
            &payload,
            "chatcmpl-1",
            1_700_000_000,
            "m",
            &mut acc,
            &mut counter,
        )
        .expect("content_block_start must translate");
        let acc = acc.expect("tool_use accumulator must be created");
        assert!(acc.arguments.is_empty());
    }

    /// A whole-message `data:` event (MiniMax unary-in-stream shape: `type:
    /// message` plus `base_resp`) must translate into ONE terminal chunk that
    /// carries the assistant delta, tool_calls, finish_reason and usage —
    /// never a silent `Ok(None)` drop.
    #[test]
    fn whole_message_event_produces_done_chunk() {
        let mut acc: Option<AnthropicToolUseAccumulator> = None;
        let mut counter = 0u32;
        let payload = format!(
            "{}\n{}",
            "message",
            r#"{"type":"message","base_resp":{"status_code":0,"status_msg":""},"role":"assistant","model":"MiniMax-M3.1-Flash","stop_reason":"tool_use","content":[{"type":"thinking","thinking":"hmm"},{"type":"tool_use","id":"toolu_1","name":"execute_code","input":{"code":"x=1"}}],"usage":{"input_tokens":10,"output_tokens":5}}"#
        );
        let chunk = translate_anthropic_sse_event(
            &payload,
            "chatcmpl-1",
            1_700_000_000,
            "m",
            &mut acc,
            &mut counter,
        )
        .expect("message event must translate")
        .expect("message event must emit a chunk");
        assert!(chunk.done);
        let choice = &chunk.payload["choices"][0];
        assert_eq!(choice["finish_reason"], "tool_calls");
        let tc = &choice["delta"]["tool_calls"][0];
        assert_eq!(tc["function"]["name"], "execute_code");
        assert_eq!(tc["function"]["arguments"], r#"{"code":"x=1"}"#);
        assert_eq!(chunk.usage.as_ref().map(|u| u.completion_tokens), Some(5));
        assert_eq!(chunk.stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(chunk.delta_reasoning.as_deref(), Some("hmm"));
    }

    /// The MiniMax envelope sometimes omits the `type` tag entirely (only
    /// `base_resp` + `content`). The message branch must still translate it.
    #[test]
    fn whole_message_without_type_tag_produces_done_chunk() {
        let mut acc: Option<AnthropicToolUseAccumulator> = None;
        let mut counter = 0u32;
        let payload = format!(
            "{}\n{}",
            "message",
            r#"{"base_resp":{"status_code":0},"role":"assistant","content":[{"type":"text","text":"hola"}],"stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":2}}"#
        );
        let chunk = translate_anthropic_sse_event(
            &payload,
            "chatcmpl-1",
            1_700_000_000,
            "m",
            &mut acc,
            &mut counter,
        )
        .expect("message event must translate")
        .expect("message event must emit a chunk");
        assert!(chunk.done);
        let choice = &chunk.payload["choices"][0];
        assert_eq!(choice["delta"]["content"], "hola");
    }

    /// An `error`-typed SSE event must surface a failure instead of becoming
    /// a silently dropped `Ok(None)`.
    #[test]
    fn error_event_surfaces_failure() {
        let mut acc: Option<AnthropicToolUseAccumulator> = None;
        let mut counter = 0u32;
        let payload = format!(
            "{}\n{}",
            "error",
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#
        );
        let res = translate_anthropic_sse_event(
            &payload,
            "chatcmpl-1",
            1_700_000_000,
            "m",
            &mut acc,
            &mut counter,
        );
        assert!(res.is_err());
    }
}
