//! Anthropic SSE parsing and OpenAI translation.

use super::{MAX_SSE_EVENT_TYPE_BYTES, UpstreamSseChunk};
use openproxy_types::error::{CoreError, Result};
use serde_json::Value;

// ==========
// H5 fix: Anthropic tool_use stateful accumulator
// ==========
//
// Anthropic streams a tool_use block across multiple SSE events:
//   1. content_block_start { type: "tool_use", id: "toolu_X", name: "F", input: {} }
//   2. content_block_delta  { type: "input_json_delta", partial_json: "{frag..." }
//      ... repeated N times until the full arguments string is delivered ...
//   N. content_block_stop   {}
//
// The OpenAI wire format emits ONE chat.completion.chunk with the
// complete `tool_calls[i].function.arguments` JSON string. The SSE
// parser is stateless, so the accumulator lives in the caller
// (pipeline.rs) and we expose the struct here for it to thread
// through each `translate_anthropic_sse_event` call.
/// Maximum allowed length for accumulated tool call arguments string.
/// Prevents unbounded memory growth from malicious input_json_delta fragments.
pub(crate) const MAX_TOOL_ARGUMENTS_BYTES: usize = 1_048_576; // 1 MiB

/// Maximum allowed length for tool call ID string.
pub(crate) const MAX_TOOL_ID_BYTES: usize = 256;

/// Maximum allowed length for tool call name string.
pub(crate) const MAX_TOOL_NAME_BYTES: usize = 256;

#[derive(Debug, Default, Clone)]
pub struct AnthropicToolUseAccumulator {
    /// Index of the tool call within the assistant message's `tool_calls` array.
    pub index: u32,
    /// Anthropic `id` (e.g. "toolu_01ABC"). Emitted once at start.
    pub id: String,
    /// Function name (e.g. "get_weather"). Emitted once at start.
    pub name: String,
    /// Accumulated partial JSON fragments from input_json_delta.
    pub arguments: String,
}

impl AnthropicToolUseAccumulator {
    /// Create a new accumulator with bounds checking.
    pub fn new_with_bounds(index: u32, id: String, name: String) -> Result<Self> {
        if id.len() > MAX_TOOL_ID_BYTES {
            return Err(CoreError::Parse(format!(
                "Anthropic tool_use id exceeds maximum length of {MAX_TOOL_ID_BYTES} bytes"
            )));
        }
        if name.len() > MAX_TOOL_NAME_BYTES {
            return Err(CoreError::Parse(format!(
                "Anthropic tool_use name exceeds maximum length of {MAX_TOOL_NAME_BYTES} bytes"
            )));
        }
        Ok(Self {
            index,
            id,
            name,
            arguments: String::new(),
        })
    }

    /// Append to arguments with bounds checking.
    pub fn push_arguments(&mut self, fragment: &str) -> Result<()> {
        if self.arguments.len() + fragment.len() > MAX_TOOL_ARGUMENTS_BYTES {
            return Err(CoreError::Parse(format!(
                "Anthropic tool_use arguments exceeds maximum length of {MAX_TOOL_ARGUMENTS_BYTES} bytes"
            )));
        }
        self.arguments.push_str(fragment);
        Ok(())
    }
}

/// Parse a single line from an Anthropic SSE stream.
/// Anthropic SSE uses `event:` lines to set the event type, then `data:` lines
/// with the payload. This function tracks state across calls.
///
/// Returns `Ok(Some(payload))` when a complete data payload is found,
/// `Ok(None)` for non-data lines, and `Err` for parse failures.
pub fn parse_anthropic_sse_stream_line(
    line: &str,
    current_event: &mut Option<String>,
) -> Result<Option<String>> {
    let line = line.trim_end_matches('\r');

    if line.is_empty() {
        // Empty line = end of event, reset
        *current_event = None;
        return Ok(None);
    }

    if let Some(event_type) = line.strip_prefix("event: ") {
        let event_type = event_type.trim();
        if event_type.len() > MAX_SSE_EVENT_TYPE_BYTES {
            tracing::warn!(
                actual_len = event_type.len(),
                max = MAX_SSE_EVENT_TYPE_BYTES,
                "SSE event type exceeds maximum length — truncating"
            );
            // Truncate instead of erroring to keep the stream alive.
            *current_event = None;
            return Ok(None);
        }
        *current_event = Some(event_type.to_string());
        return Ok(None);
    }

    if let Some(data) = line.strip_prefix("data: ") {
        let event_type = current_event.as_deref().unwrap_or("unknown");
        // Return the event type alongside the data so the caller can translate
        // Format: "event_type\ndata_payload"
        return Ok(Some(format!("{event_type}\n{data}")));
    }

    // Ignore id:, retry:, comments, etc.
    Ok(None)
}

/// Translate a single Anthropic SSE payload (event_type + data JSON) into
/// an OpenAI-compatible SSE chunk string.
///
/// The payload format is "event_type\njson_data".
fn build_anthropic_message_start_chunk(
    chunk_id: &str,
    created: u64,
    model: &str,
) -> UpstreamSseChunk {
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
        usage: None,
        stop_reason: None,
        delta_reasoning: None,
        delta_tool_calls: Vec::new(),
        has_content: false,
    }
}

fn translate_anthropic_content_delta(
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

fn translate_anthropic_message_delta(
    data: &Value,
    chunk_id: &str,
    created: u64,
    model: &str,
) -> UpstreamSseChunk {
    let stop_reason = data
        .get("delta")
        .and_then(|d| d.get("stop_reason"))
        .and_then(|r| r.as_str());

    let finish_reason = match stop_reason {
        Some("end_turn" | "stop_sequence") => Some("stop".to_string()),
        Some("max_tokens") => Some("length".to_string()),
        _ => None,
    };

    let usage = data.get("usage").map(|u| crate::translation::OpenAIUsage {
        prompt_tokens: u
            .get("input_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
            .try_into()
            .unwrap_or(u32::MAX),
        completion_tokens: u
            .get("output_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
            .try_into()
            .unwrap_or(u32::MAX),
        total_tokens: 0,
        prompt_tokens_details: None,
    });

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
            chunk_id, created, model,
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

// H5 fix: stateful translation that the streaming loop calls
// per-SSE-event with a caller-owned `AnthropicToolUseAccumulator`.
// On the first `content_block_start` whose block is `type: "tool_use"`
// we open the accumulator and emit a role-tagged chunk with the
// tool_call id+name (no arguments yet). On each subsequent
// `content_block_delta` of subtype `input_json_delta` we append to
// the accumulator and emit a chunk with the partial arguments. On
// `content_block_stop` we close out (no chunk — the next message_delta
// or stream end will signal the client). The OpenAI spec is silent
// on whether partial-arguments chunks are sent or whether the caller
// should buffer; we follow the streaming-tools convention used by
// vLLM and the OpenAI Python SDK: send one chunk at start (id+name
// only) and one final chunk at stop with the assembled arguments
// string. This keeps the wire shape small and lets non-streaming
// consumers re-assemble easily.
//
// PERF: `content_block_delta` is the streaming hot path (N events per
// response, vs 1 each for the lifecycle events). We parse it with a
// targeted `AnthropicContentBlockDeltaProbe` instead of the full
// `serde_json::Value` AST. serde skips unknown fields (e.g. `index`,
// `content_block_index`) without allocating them, which reduces
// per-chunk CPU on the Anthropic text-streaming path significantly.
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

/// Lightweight probe for `content_block_start` events. Only extracts
/// the `content_block.{type,id,name}` fields needed for tool_use
/// dispatch — serde skips the rest without allocating.
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
    // Capture the running length BEFORE appending so
    // the downstream accumulator (sse_accumulator.rs)
    // can append only the NEW fragment, not the
    // whole running total (which would double-encode
    // the arguments JSON across the wire chunks).
    let prev_len = acc.arguments.len();
    if let Some(partial) = delta.partial_json.as_deref() {
        // Bound check: prevent unbounded argument accumulation
        if prev_len + partial.len() <= MAX_TOOL_ARGUMENTS_BYTES {
            acc.arguments.push_str(partial);
        }
    }
    let new_fragment = &acc.arguments[prev_len..];
    // Emit a chunk that carries ONLY the newly-appended
    // fragment in `arguments`. The OpenAI streaming
    // tool_calls spec requires each chunk to carry a
    // FRAGMENT of the arguments JSON; the client
    // concatenates fragments by `index`.
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
    // Mirror ONLY the new fragment in
    // `delta_tool_calls` so the pipeline's accumulator
    // appends it once to its in-flight tool_call's arguments.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_event_line_sets_current_event() {
        let mut current_event = None;
        let result =
            parse_anthropic_sse_stream_line("event: message_start", &mut current_event).unwrap();
        assert!(result.is_none());
        assert_eq!(current_event.as_deref(), Some("message_start"));
    }

    #[test]
    fn anthropic_data_line_returns_payload_with_event() {
        let mut current_event = Some("content_block_delta".to_string());
        let result = parse_anthropic_sse_stream_line(
            r#"data: {"delta":{"text":"Hello"}}"#,
            &mut current_event,
        )
        .unwrap()
        .unwrap();
        assert!(result.starts_with("content_block_delta\n"));
    }

    #[test]
    fn anthropic_empty_line_resets_event() {
        let mut current_event = Some("message_start".to_string());
        let result = parse_anthropic_sse_stream_line("", &mut current_event).unwrap();
        assert!(result.is_none());
        assert!(current_event.is_none());
    }

    #[test]
    fn anthropic_non_data_line_returns_none() {
        let mut current_event = None;
        assert!(
            parse_anthropic_sse_stream_line("id: 123", &mut current_event)
                .unwrap()
                .is_none()
        );
        assert!(
            parse_anthropic_sse_stream_line("retry: 5000", &mut current_event)
                .unwrap()
                .is_none()
        );
        assert!(
            parse_anthropic_sse_stream_line(": comment", &mut current_event)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn anthropic_translate_message_start() {
        let payload = r#"message_start
{"type":"message","role":"assistant","content":[],"model":"claude-3","stop_reason":null,"usage":{"input_tokens":10,"output_tokens":0}}"#;
        let chunk = translate_anthropic_sse_payload(payload, "chunk-1", 1000, "claude-3")
            .unwrap()
            .unwrap();
        assert!(!chunk.done);
        assert_eq!(
            chunk.payload["choices"][0]["delta"]["role"]
                .as_str()
                .unwrap(),
            "assistant"
        );
        assert_eq!(chunk.payload["id"].as_str().unwrap(), "chunk-1");
        // message_start is metadata-only (role announcement, no tokens).
        assert!(
            !chunk.has_content,
            "message_start must have has_content=false"
        );
    }

    #[test]
    fn anthropic_translate_content_block_delta() {
        let payload = r#"content_block_delta
{"delta":{"type":"content_block_delta","text":"Hello"}}"#;
        let chunk = translate_anthropic_sse_payload(payload, "chunk-1", 1000, "claude-3")
            .unwrap()
            .unwrap();
        assert!(!chunk.done);
        assert_eq!(
            chunk.payload["choices"][0]["delta"]["content"]
                .as_str()
                .unwrap(),
            "Hello"
        );
        // content_block_delta with text carries real content.
        assert!(
            chunk.has_content,
            "content_block_delta (text) must have has_content=true"
        );
    }

    #[test]
    fn anthropic_translate_message_delta_with_stop() {
        let payload = r#"message_delta
{"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":50}}"#;
        let chunk = translate_anthropic_sse_payload(payload, "chunk-1", 1000, "claude-3")
            .unwrap()
            .unwrap();
        assert!(chunk.done);
        assert_eq!(
            chunk.payload["choices"][0]["finish_reason"]
                .as_str()
                .unwrap(),
            "stop"
        );
        assert!(chunk.usage.is_some());
        // message_delta carries only stop_reason + usage, no tokens.
        assert!(
            !chunk.has_content,
            "message_delta must have has_content=false"
        );
    }

    #[test]
    fn anthropic_translate_message_delta_max_tokens() {
        let payload = r#"message_delta
{"delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":100}}"#;
        let chunk = translate_anthropic_sse_payload(payload, "chunk-1", 1000, "claude-3")
            .unwrap()
            .unwrap();
        assert!(chunk.done);
        assert_eq!(
            chunk.payload["choices"][0]["finish_reason"]
                .as_str()
                .unwrap(),
            "length"
        );
    }

    // ---- H5 fix: Anthropic tool_use accumulator ----

    #[test]
    fn anthropic_tool_use_start_emits_id_and_name() {
        // The content_block_start event for a tool_use block must
        // emit an OpenAI-shaped chunk with `tool_calls[0]` carrying
        // the id, type=function, and name. The arguments field is
        // empty at this point because the JSON body is delivered
        // in subsequent content_block_delta events.
        let payload = r#"content_block_start
{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_01ABC","name":"get_weather","input":{}}}"#;
        let mut acc: Option<AnthropicToolUseAccumulator> = None;
        let mut counter: u32 = 0;
        let chunk = translate_anthropic_sse_event(
            payload,
            "chunk-1",
            1000,
            "claude-3",
            &mut acc,
            &mut counter,
        )
        .unwrap()
        .unwrap();
        assert!(!chunk.done);
        let tool_call = &chunk.payload["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(tool_call["index"].as_u64().unwrap(), 0);
        assert_eq!(tool_call["id"].as_str().unwrap(), "toolu_01ABC");
        assert_eq!(tool_call["type"].as_str().unwrap(), "function");
        assert_eq!(
            tool_call["function"]["name"].as_str().unwrap(),
            "get_weather"
        );
        assert_eq!(tool_call["function"]["arguments"].as_str().unwrap(), "");
        // The accumulator must be open after start.
        assert!(acc.is_some());
        assert_eq!(acc.as_ref().unwrap().id, "toolu_01ABC");
        assert_eq!(acc.as_ref().unwrap().name, "get_weather");
        // Index counter is monotonically increasing.
        assert_eq!(counter, 1);
        // CRITICAL: content_block_start (tool_use) is a metadata-only
        // event — it announces id+name with EMPTY arguments. The
        // actual argument tokens come later in content_block_delta
        // (input_json_delta) events. `has_content` must be `false`
        // so the pipeline does NOT call `note_content_chunk()` here
        // (which would reset the idle_chunk timer and cause the
        // 10s gap timer to fire while the model is still generating
        // the first argument fragment). This was the root cause of
        // the user-visible "idle_chunk after 10000ms" bug on
        // MiniMax-M3 tool calls.
        assert!(
            !chunk.has_content,
            "content_block_start (tool_use) must have has_content=false \
             — it carries no generated tokens, only id+name metadata"
        );
    }

    #[test]
    fn anthropic_tool_use_input_json_delta_accumulates() {
        // Two content_block_delta events of subtype input_json_delta
        // must be accumulated into a single running arguments
        // string and emitted as two OpenAI-shaped chunks.
        //
        // CRITICAL: each chunk sent to the client must carry ONLY the
        // NEW fragment (not the running total). The OpenAI streaming
        // tool_calls spec requires the client to concatenate fragments
        // by `index`. If we send the running total, the client
        // concatenates f1 + (f1+f2) + (f1+f2+f3) + ..., duplicating
        // early fragments N times. This is the "tool call arguments
        // duplicated" bug that was fixed.
        //
        // We build each wire payload programmatically with
        // serde_json::json! to avoid fragile double/triple-escaped
        // string literals — Anthropic's input_json_delta value is a
        // JSON-encoded string of a JSON fragment, and the escaping
        // rules get noisy fast. The function we're testing
        // (translate_anthropic_sse_event) consumes the same JSON
        // either way; what matters is the resulting accumulated
        // `arguments` field.
        let start = "content_block_start\n".to_string()
            + &serde_json::json!({
                "type": "content_block_start",
                "index": 1,
                "content_block": {
                    "type": "tool_use",
                    "id": "toolu_X",
                    "name": "search",
                    "input": {}
                }
            })
            .to_string();
        let mut acc: Option<AnthropicToolUseAccumulator> = None;
        let mut counter: u32 = 0;
        let _ = translate_anthropic_sse_event(
            &start,
            "chunk-1",
            1000,
            "claude-3",
            &mut acc,
            &mut counter,
        )
        .unwrap()
        .unwrap();
        // First delta — partial_json carries the JSON fragment `{"q":`.
        let delta1 = "content_block_delta\n".to_string()
            + &serde_json::json!({
                "type": "content_block_delta",
                "index": 1,
                "delta": {
                    "type": "input_json_delta",
                    "partial_json": "{\"q\":"
                }
            })
            .to_string();
        let chunk1 = translate_anthropic_sse_event(
            &delta1,
            "chunk-2",
            1000,
            "claude-3",
            &mut acc,
            &mut counter,
        )
        .unwrap()
        .unwrap();
        // Chunk 1 must carry ONLY the first fragment.
        assert_eq!(
            chunk1.payload["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .unwrap(),
            "{\"q\":"
        );
        // Second delta — partial_json carries the rest of the JSON
        // fragment, `"sf"}` (including the closing brace).
        let delta2 = "content_block_delta\n".to_string()
            + &serde_json::json!({
                "type": "content_block_delta",
                "index": 1,
                "delta": {
                    "type": "input_json_delta",
                    "partial_json": "\"sf\"}"
                }
            })
            .to_string();
        let chunk2 = translate_anthropic_sse_event(
            &delta2,
            "chunk-3",
            1000,
            "claude-3",
            &mut acc,
            &mut counter,
        )
        .unwrap()
        .unwrap();
        // Chunk 2 must carry ONLY the second fragment — NOT the
        // running total. This is the fix: previously it sent
        // `{"q":"sf"}` (the full running total), causing the client
        // to concatenate `{"q":` + `{"q":"sf"}` = `{"q":{"q":"sf"}`
        // which is invalid JSON.
        assert_eq!(
            chunk2.payload["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .unwrap(),
            "\"sf\"}"
        );
        // The accumulator must still hold the full running total
        // (for the persisted response body).
        assert_eq!(acc.as_ref().unwrap().arguments, "{\"q\":\"sf\"}");
        // Regression: simulate what a real OpenAI client does —
        // concatenate the `arguments` fragments from all chunks by
        // `index`. The result must parse as valid JSON matching the
        // assembled tool call.
        let fragment1 =
            chunk1.payload["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .unwrap();
        let fragment2 =
            chunk2.payload["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .unwrap();
        let concatenated = format!("{fragment1}{fragment2}");
        let parsed: serde_json::Value = serde_json::from_str(&concatenated)
            .expect("concatenated fragments must parse as valid JSON");
        assert_eq!(parsed["q"], "sf");
    }

    #[test]
    fn anthropic_tool_use_block_stop_clears_accumulator() {
        let start = r#"content_block_start
{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_X","name":"f","input":{}}}"#;
        let stop = r#"content_block_stop
{"type":"content_block_stop","index":1}"#;
        let mut acc: Option<AnthropicToolUseAccumulator> = None;
        let mut counter: u32 = 0;
        let _ = translate_anthropic_sse_event(
            start,
            "chunk-1",
            1000,
            "claude-3",
            &mut acc,
            &mut counter,
        )
        .unwrap();
        assert!(acc.is_some());
        // content_block_stop emits no chunk (clients can detect
        // the end of a tool_call by index reuse / a subsequent
        // message_delta) and clears the accumulator so the next
        // tool_use block in the same turn gets a fresh index.
        let chunk = translate_anthropic_sse_event(
            stop,
            "chunk-2",
            1000,
            "claude-3",
            &mut acc,
            &mut counter,
        )
        .unwrap();
        assert!(chunk.is_none());
        assert!(acc.is_none());
        // The next tool_use block must get index 1, not 0 — the
        // counter only increments on content_block_start, not on
        // every event.
        let start2 = r#"content_block_start
{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_Y","name":"g","input":{}}}"#;
        let chunk2 = translate_anthropic_sse_event(
            start2,
            "chunk-3",
            1000,
            "claude-3",
            &mut acc,
            &mut counter,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            chunk2.payload["choices"][0]["delta"]["tool_calls"][0]["index"]
                .as_u64()
                .unwrap(),
            1
        );
    }

    #[test]
    fn anthropic_text_block_passthrough_does_not_open_accumulator() {
        // Text blocks (the most common case) must not touch the
        // tool_use accumulator. The content_block_start for a
        // text block returns None (no chunk) and the
        // content_block_delta with text_delta reuses the same
        // emission path as the stateless translator.
        let start = r#"content_block_start
{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
        let delta = r#"content_block_delta
{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}"#;
        let mut acc: Option<AnthropicToolUseAccumulator> = None;
        let mut counter: u32 = 0;
        let start_chunk = translate_anthropic_sse_event(
            start,
            "chunk-1",
            1000,
            "claude-3",
            &mut acc,
            &mut counter,
        )
        .unwrap();
        assert!(start_chunk.is_none());
        assert!(acc.is_none());
        let delta_chunk = translate_anthropic_sse_event(
            delta,
            "chunk-2",
            1000,
            "claude-3",
            &mut acc,
            &mut counter,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            delta_chunk.payload["choices"][0]["delta"]["content"]
                .as_str()
                .unwrap(),
            "hello"
        );
    }

    #[test]
    fn anthropic_input_json_delta_without_open_accumulator_is_dropped() {
        // Defensive: if a content_block_delta/input_json_delta
        // arrives without a preceding content_block_start/tool_use
        // (malformed stream), drop it rather than emit a chunk
        // with a phantom tool_call.
        let delta = r#"content_block_delta
{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"x\":1}"}}"#;
        let mut acc: Option<AnthropicToolUseAccumulator> = None;
        let mut counter: u32 = 0;
        let chunk = translate_anthropic_sse_event(
            delta,
            "chunk-1",
            1000,
            "claude-3",
            &mut acc,
            &mut counter,
        )
        .unwrap();
        assert!(chunk.is_none());
        // Counter untouched.
        assert_eq!(counter, 0);
    }

    #[test]
    fn anthropic_message_start_still_works_via_stateful_translator() {
        // The H5 translator must still defer to the existing
        // message_start / message_delta / message_stop handling
        // so legacy chunks (role, finish_reason, usage) keep
        // working.
        let start = r#"message_start
{"type":"message","role":"assistant","content":[],"model":"claude-3","stop_reason":null,"usage":{"input_tokens":10,"output_tokens":0}}"#;
        let mut acc: Option<AnthropicToolUseAccumulator> = None;
        let mut counter: u32 = 0;
        let chunk = translate_anthropic_sse_event(
            start,
            "chunk-1",
            1000,
            "claude-3",
            &mut acc,
            &mut counter,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            chunk.payload["choices"][0]["delta"]["role"]
                .as_str()
                .unwrap(),
            "assistant"
        );
    }

    #[test]
    fn anthropic_translate_message_stop() {
        // H4 fix: `message_stop` is the closing handshake after
        // `message_delta` already emitted the `done: true` chunk.
        // Returning `Ok(None)` here prevents a duplicate end-of-
        // stream signal in the downstream SSE stream.
        let payload = "message_stop\n{}";
        let result = translate_anthropic_sse_payload(payload, "chunk-1", 1000, "claude-3").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn anthropic_translate_ping_skipped() {
        let payload = "ping\n{}";
        let result = translate_anthropic_sse_payload(payload, "chunk-1", 1000, "claude-3").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn anthropic_translate_unknown_event_skipped() {
        let payload = "content_block_start\n{}";
        let result = translate_anthropic_sse_payload(payload, "chunk-1", 1000, "claude-3").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn anthropic_full_stream_simulation() {
        // Simulate a realistic Anthropic SSE stream
        let lines = vec![
            "event: message_start",
            r#"data: {"type":"message","role":"assistant","content":[],"model":"claude-3","stop_reason":null,"usage":{"input_tokens":10,"output_tokens":0}}"#,
            "",
            "event: content_block_delta",
            r#"data: {"delta":{"type":"content_block_delta","text":"Hi"}}"#,
            "",
            "event: content_block_delta",
            r#"data: {"delta":{"type":"content_block_delta","text":" there"}}"#,
            "",
            "event: message_delta",
            r#"data: {"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}"#,
            "",
            "event: message_stop",
            r"data: {}",
            "",
        ];

        let mut current_event = None;
        let mut chunks = Vec::new();

        for line in lines {
            if let Some(payload) =
                parse_anthropic_sse_stream_line(line, &mut current_event).unwrap()
                && let Some(chunk) =
                    translate_anthropic_sse_payload(&payload, "test", 0, "claude-3").unwrap()
            {
                chunks.push(chunk);
            }
        }

        // Should have: message_start, 2 content_block_delta, message_delta.
        // H4 fix: `message_stop` no longer produces a chunk
        // (it's a no-op handshake that would otherwise produce a
        // second `done: true` chunk — see H4 / sse.rs:316).
        assert_eq!(chunks.len(), 4);
        // First chunk: role assignment
        assert_eq!(
            chunks[0].payload["choices"][0]["delta"]["role"]
                .as_str()
                .unwrap(),
            "assistant"
        );
        // Second chunk: "Hi"
        assert_eq!(
            chunks[1].payload["choices"][0]["delta"]["content"]
                .as_str()
                .unwrap(),
            "Hi"
        );
        // Third chunk: " there"
        assert_eq!(
            chunks[2].payload["choices"][0]["delta"]["content"]
                .as_str()
                .unwrap(),
            " there"
        );
        // Fourth chunk: finish_reason
        assert_eq!(
            chunks[3].payload["choices"][0]["finish_reason"]
                .as_str()
                .unwrap(),
            "stop"
        );
        // The single `done: true` chunk comes from `message_delta`
        // — exactly one downstream end-of-stream signal for the
        // full stream, which is the invariant H4 is enforcing.
        let done_chunks: usize = chunks.iter().filter(|c| c.done).count();
        assert_eq!(done_chunks, 1);
    }

    // ---- REVIEWER audit #9 (SSE chunk allocation reuses buffer across
    // providers, 2026-06-18): DISMISSED with a non-regression test. The
    // claim was that bytes read from one upstream's body could leak into
    // another upstream's response because of a shared `BytesMut` or
    // thread-local buffer. After auditing `dispatch_upstream_streaming`
    // (pipeline.rs:2016), `UpstreamBodyStream` (upstream/response.rs:58),
    // and `format_sse_data` (translation.rs:729), every buffer on the
    // streaming path is *local* to the per-call stack frame: the SSE
    // `String` line buffer at pipeline.rs:2211, the per-chunk
    // `String::from_utf8_lossy(&bytes)` at pipeline.rs:2338, and the
    // `serde_json::to_string(&chunk.payload)` at pipeline.rs:2465 all
    // allocate fresh per iteration with no global, no thread-local, and
    // no `Arc<[u8]>` shared buffer. The anthropic tool_use accumulator
    // is passed as `&mut Option<...>` from the caller — i.e. the
    // caller owns it, no global state.
    //
    // This test pins the invariant at the function level: 64 parallel
    // `translate_anthropic_sse_event` callers, each with a distinct
    // chunk_id and tool_call counter, must produce non-interleaved
    // outputs. If a future change introduces a shared buffer this
    // test fails with cross-contamination.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sse_translation_isolates_parallel_requests() {
        use std::sync::Arc;
        use tokio::task::JoinSet;

        const N: usize = 64;
        let mut joins = JoinSet::new();
        let barrier = Arc::new(tokio::sync::Barrier::new(N));

        for i in 0..N {
            let barrier = Arc::clone(&barrier);
            joins.spawn(async move {
                let chunk_id = format!("chatcmpl-{i}");
                let model = format!("claude-isolated-{i}");
                let mut tool_use_acc: Option<AnthropicToolUseAccumulator> = None;
                let mut tool_call_index_counter: u32 = 0;

                // Wait until all tasks are queued so they race in
                // parallel (not sequentially).
                barrier.wait().await;

                // Sequence: content_block_start (tool_use) → deltas →
                // message_delta (stop). Each task sees a distinct
                // tool id+name so any cross-talk would be visible.
                let id = format!("toolu_{i:08x}");
                let name = format!("fn_{i}");
                let start_payload = format!(
                    "content_block_start\n{{\"content_block\":{{\"type\":\"tool_use\",\"id\":\"{id}\",\"name\":\"{name}\",\"input\":{{}}}}}}"
                );
                let delta_payload = "content_block_delta\n{\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}".to_string();
                let stop_payload = "message_delta\n{\"delta\":{\"stop_reason\":\"tool_use\"}}".to_string();

                let mut outs = Vec::new();
                for payload in [&start_payload, &delta_payload, &stop_payload] {
                    let out = translate_anthropic_sse_event(
                        payload,
                        &chunk_id,
                        1_700_000_000 + i as u64,
                        &model,
                        &mut tool_use_acc,
                        &mut tool_call_index_counter,
                    )
                    .expect("translate");
                    outs.push(out);
                }
                (i, chunk_id, model, outs)
            });
        }

        let mut seen_ids = std::collections::HashSet::new();
        let mut seen_models = std::collections::HashSet::new();
        while let Some(j) = joins.join_next().await {
            let (i, chunk_id, model, outs) = j.expect("join");
            // chunk_id must round-trip exactly, no cross-talk from peers.
            assert_eq!(chunk_id, format!("chatcmpl-{i}"));
            assert_eq!(model, format!("claude-isolated-{i}"));
            assert!(seen_ids.insert(chunk_id.clone()), "duplicate chunk_id");
            assert!(seen_models.insert(model.clone()), "duplicate model");

            // First chunk must carry THIS task's tool id and name,
            // not any other task's.
            let first_payload = &outs[0].as_ref().expect("first chunk").payload;
            let tool_id = first_payload["choices"][0]["delta"]["tool_calls"][0]["id"]
                .as_str()
                .expect("tool id");
            let tool_name =
                first_payload["choices"][0]["delta"]["tool_calls"][0]["function"]["name"]
                    .as_str()
                    .expect("tool name");
            assert_eq!(
                tool_id,
                format!("toolu_{i:08x}"),
                "tool id leaked from another parallel task"
            );
            assert_eq!(
                tool_name,
                format!("fn_{i}"),
                "tool name leaked from another parallel task"
            );

            // Model and chunk_id in the wire payload also must be
            // THIS task's, not a peer's.
            assert_eq!(first_payload["model"].as_str().unwrap(), model);
            assert_eq!(first_payload["id"].as_str().unwrap(), chunk_id);
        }
        assert_eq!(seen_ids.len(), N, "expected {N} unique chunk_ids");
    }
}
