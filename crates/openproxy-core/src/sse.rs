//! SSE (Server-Sent Events) parsing and translation for streaming responses.
//!
//! Provides parsers for OpenAI and Gemini upstream SSE formats, translating
//! them into OpenAI-format SSE chunks that clients expect.

use crate::error::{CoreError, Result};
use crate::translation::OpenAIUsage;
use serde_json::Value;

/// A single parsed SSE chunk from the upstream, ready to forward.
pub struct UpstreamSseChunk {
    /// Raw JSON string for pass-through formats (OpenAI). When present,
    /// the pipeline forwards this directly without re-serialization.
    pub raw_payload: Option<String>,
    /// The parsed JSON payload. Used for translated formats (Gemini,
    /// Anthropic) that need AST manipulation. Ignored when `raw_payload`
    /// is `Some`.
    pub payload: Value,
    /// Whether this is the final chunk ([DONE] sentinel).
    pub done: bool,
    /// Usage stats if present in this chunk (usually only the final one).
    pub usage: Option<OpenAIUsage>,
    /// Upstream stop reason (e.g. "end_turn", "max_tokens", "stop_sequence"
    /// for Anthropic; mapped finish_reason for OpenAI). Only set on the
    /// final chunk.
    pub stop_reason: Option<String>,
    /// Extracted per-chunk reasoning delta. Populated by:
    /// - Gemini `parts[].thought == true` items,
    /// - Anthropic `content_block_delta` with `delta.type == "thinking_delta"`.
    ///
    /// `None` when this chunk carries no reasoning.
    pub delta_reasoning: Option<String>,
    /// Extracted per-chunk tool_calls deltas. Populated by:
    /// - Anthropic `content_block_start` (tool_use block) emits the
    ///   `{index, id, type, function:{name, arguments:""}}` record,
    /// - Anthropic `content_block_delta` with `delta.type == "input_json_delta"`
    ///   emits the running `{index, function:{arguments:...}}` record.
    ///
    /// Empty when this chunk carries no tool_calls.
    pub delta_tool_calls: Vec<serde_json::Value>,
    /// Whether this chunk carries "real content" — i.e. actual generated
    /// tokens (text, reasoning, or tool-call argument fragments) as
    /// opposed to metadata-only events (block announcements, stop
    /// signals, usage reports).
    ///
    /// The pipeline uses this flag to decide whether to call
    /// [`UpstreamBodyStream::note_content_chunk`], which resets the
    /// chunk-gap (`idle_chunk_ms`) timer. Only chunks with `has_content
    /// == true` should reset the timer — metadata-only events (like
    /// Anthropic's `content_block_start` for a `tool_use` block, which
    /// announces the tool call id+name but carries empty arguments)
    /// must NOT reset it, because the model hasn't started generating
    /// actual argument tokens yet.
    ///
    /// Default is `true` (most chunks carry content). Set to `false`
    /// explicitly in translators for metadata-only events.
    pub has_content: bool,
}

impl UpstreamSseChunk {
    /// Get the forwardable JSON string. Returns the raw payload if
    /// available (zero allocation), otherwise serializes the parsed payload.
    pub fn into_json_string(self) -> String {
        self.raw_payload
            .unwrap_or_else(|| serde_json::to_string(&self.payload).unwrap_or_default())
    }

    /// Get the SSE frame as pre-formatted `data: {json}\n\n` `Bytes`,
    /// ready for direct socket write. Avoids the intermediate `String`
    /// allocation when the frame is immediately written to the socket.
    pub fn into_sse_bytes(self) -> bytes::Bytes {
        build_sse_frame(&self.into_json_string())
    }
}

/// Build a `data: <payload>\n\n` SSE frame as `Bytes`, ready for socket write.
/// The `+ 16` covers `"data: "` (6) + `"\n\n"` (2) + slack for BytesMut's
/// allocation strategy. Caller passes the inner JSON (no leading `data: `).
pub(crate) fn build_sse_frame(payload: &str) -> bytes::Bytes {
    let mut b = bytes::BytesMut::with_capacity(payload.len() + 16);
    b.extend_from_slice(b"data: ");
    b.extend_from_slice(payload.as_bytes());
    b.extend_from_slice(b"\n\n");
    b.freeze()
}

// =====================================================================
// H5 fix: Anthropic tool_use stateful accumulator
// =====================================================================
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

// =====================================================================
// OpenAI SSE parsing
// =====================================================================

/// Lightweight struct for extracting only metadata from OpenAI SSE chunks.
/// serde skips unknown fields (delta content, tool_calls, etc.) without
/// allocating them, making this much faster than parsing into Value.
#[derive(serde::Deserialize)]
struct OpenAiSseProbe {
    #[serde(default)]
    usage: Option<OpenAiUsageProbe>,
    #[serde(default)]
    choices: Option<Vec<OpenAiChoiceProbe>>,
}

#[derive(serde::Deserialize)]
struct OpenAiUsageProbe {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    total_tokens: Option<u64>,
}

#[derive(serde::Deserialize)]
struct OpenAiChoiceProbe {
    finish_reason: Option<String>,
    #[serde(default)]
    delta: Option<OpenAiDeltaProbe>,
}

#[derive(serde::Deserialize)]
struct OpenAiDeltaProbe {
    #[serde(default)]
    reasoning_content: Option<String>,
}

/// Parse a single SSE line from an OpenAI-compatible upstream.
///
/// Returns `Ok(None)` for empty lines, comments, and `[DONE]` sentinels.
/// Returns `Ok(Some(chunk))` for valid data lines.
pub fn parse_openai_sse_line(line: &str) -> Result<Option<UpstreamSseChunk>> {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    if trimmed.is_empty() || trimmed.starts_with(':') {
        return Ok(None);
    }
    let payload = match trimmed.strip_prefix("data:") {
        Some(rest) => rest.trim_start(),
        None => return Ok(None),
    };
    if payload == "[DONE]" {
        return Ok(Some(UpstreamSseChunk {
            raw_payload: None,
            payload: Value::Null,
            done: true,
            usage: None,
            stop_reason: None,
            delta_reasoning: None,
            delta_tool_calls: Vec::new(),
            has_content: false, // [DONE] sentinel — no content
        }));
    }
    // Fast targeted parse: only extracts usage + finish_reason,
    // skips all other fields (delta.content, tool_calls, etc.)
    let probe: OpenAiSseProbe = serde_json::from_str(payload)
        .map_err(|e| CoreError::Parse(format!("openai sse json: {e}")))?;

    let usage = probe.usage.map(|u| OpenAIUsage {
        prompt_tokens: u.prompt_tokens.unwrap_or(0) as u32,
        completion_tokens: u.completion_tokens.unwrap_or(0) as u32,
        total_tokens: u.total_tokens.unwrap_or(0) as u32,
    });
    // o1-style reasoning models (o1, o3, deepseek-r1) emit
    // `delta.reasoning_content` on chunks that also carry `usage`
    // or a non-null `finish_reason` — i.e. the slow path. Surface
    // it on `delta_reasoning` so the pipeline's accumulator
    // (sse_accumulator.rs) can persist it as
    // `choices[0].message.reasoning_content`. The probe does the
    // parse work; we just extract one more field. Borrow via
    // `as_ref()` so the subsequent `finish_reason` extraction
    // (which consumes `probe.choices`) can still run.
    let delta_reasoning = probe
        .choices
        .as_ref()
        .and_then(|c| c.first())
        .and_then(|c| c.delta.as_ref())
        .and_then(|d| d.reasoning_content.as_ref())
        .cloned();
    let finish_reason = probe
        .choices
        .and_then(|mut c| c.pop())
        .and_then(|c| c.finish_reason);

    Ok(Some(UpstreamSseChunk {
        raw_payload: Some(payload.to_string()),
        payload: Value::Null,
        done: false,
        usage,
        stop_reason: finish_reason,
        delta_reasoning,
        delta_tool_calls: Vec::new(),
        has_content: true,
    }))
}

// =====================================================================
// Gemini SSE parsing
// =====================================================================

/// Lightweight probe struct for extracting ONLY the fields the proxy
/// needs from a Gemini SSE chunk, without allocating the full
/// `serde_json::Value` AST. serde skips unknown fields (e.g. `role`,
/// `index`, `safetyRatings`) without allocating them, making this
/// ~3-5x faster than `from_str::<Value>` on typical Gemini chunks.
///
/// Field naming uses `#[serde(rename = ...)]` to map Gemini's
/// camelCase wire format to Rust's snake_case conventions.
#[derive(serde::Deserialize, Default)]
struct GeminiSseProbe {
    #[serde(default)]
    candidates: Vec<GeminiCandidateProbe>,
    #[serde(default, rename = "usageMetadata")]
    usage_metadata: Option<GeminiUsageProbe>,
}

#[derive(serde::Deserialize, Default)]
struct GeminiCandidateProbe {
    #[serde(default)]
    content: Option<GeminiContentProbe>,
    #[serde(default, rename = "finishReason")]
    finish_reason: Option<String>,
}

#[derive(serde::Deserialize, Default)]
struct GeminiContentProbe {
    #[serde(default)]
    parts: Vec<GeminiPartProbe>,
}

#[derive(serde::Deserialize, Default)]
struct GeminiPartProbe {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thought: Option<bool>,
}

#[derive(serde::Deserialize)]
struct GeminiUsageProbe {
    #[serde(default, rename = "promptTokenCount")]
    prompt_tokens: Option<u64>,
    #[serde(default, rename = "candidatesTokenCount")]
    completion_tokens: Option<u64>,
    #[serde(default, rename = "totalTokenCount")]
    total_tokens: Option<u64>,
}

/// Map a Gemini finishReason to an OpenAI finish_reason string.
fn map_gemini_finish_reason(reason: &str) -> String {
    match reason {
        "STOP" => "stop".to_string(),
        "MAX_TOKENS" => "length".to_string(),
        "SAFETY" | "RECITATION" | "BLOCKLIST" => "content_filter".to_string(),
        _ => "stop".to_string(),
    }
}

/// Parse a single SSE line from a Gemini upstream and translate to OpenAI format.
///
/// Gemini SSE lines are `data: {...}` with `candidates[].content.parts[].text`.
/// Translates to OpenAI `chat.completion.chunk` format.
///
/// PERF: uses a targeted `GeminiSseProbe` deserializer instead of
/// `serde_json::Value` to avoid allocating the full JSON AST per chunk.
/// serde skips unknown fields without allocating them, which reduces
/// per-chunk CPU on the Gemini path significantly.
pub fn parse_gemini_sse_line(
    line: &str,
    chunk_id: &str,
    created: u64,
    model: &str,
) -> Result<Option<UpstreamSseChunk>> {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    if trimmed.is_empty() || trimmed.starts_with(':') {
        return Ok(None);
    }
    let payload = match trimmed.strip_prefix("data:") {
        Some(rest) => rest.trim_start(),
        None => return Ok(None),
    };
    if payload == "[DONE]" {
        return Ok(Some(UpstreamSseChunk {
            raw_payload: None,
            payload: Value::Null,
            done: true,
            usage: None,
            stop_reason: None,
            delta_reasoning: None,
            delta_tool_calls: Vec::new(),
            has_content: true,
        }));
    }
    // Fast targeted parse: only extracts candidates[].content.parts[].text
    // (+ thought flag), candidates[].finishReason, and usageMetadata.
    // Skips unknown fields (role, index, safetyRatings, citations, etc.)
    // without allocating them.
    let probe: GeminiSseProbe = serde_json::from_str(payload)
        .map_err(|e| CoreError::Parse(format!("gemini sse json: {e}")))?;

    // Extract text from candidates[0].content.parts[].
    //
    // Each part may have a `thought: true` flag indicating it's a
    // reasoning fragment (Gemini 2.0/2.5 thinking models). We split
    // the parts into two accumulators so the accumulator downstream
    // (sse_accumulator.rs::ResponseAccumulator) can build separate
    // `content` and `reasoning` fields for the persisted JSON.
    //
    // A single part can carry BOTH `text` and `thought: true` — that
    // is the thinking-then-answering interleaved case. We route the
    // text based on the `thought` flag, never on the field ordering.
    let (text, delta_reasoning) = {
        let mut content_parts: Vec<String> = Vec::new();
        let mut reasoning_parts: Vec<String> = Vec::new();
        if let Some(candidate) = probe.candidates.first()
            && let Some(content) = &candidate.content
        {
            for part in &content.parts {
                if let Some(t) = part.text.as_deref() {
                    let is_thought = part.thought.unwrap_or(false);
                    if is_thought {
                        reasoning_parts.push(t.to_string());
                    } else {
                        content_parts.push(t.to_string());
                    }
                }
            }
        }
        let joined = content_parts.concat();
        let dr = if reasoning_parts.is_empty() {
            None
        } else {
            Some(reasoning_parts.concat())
        };
        // The wire payload's `delta.content` carries ONLY the
        // non-thought text (OpenAI streaming convention:
        // reasoning goes in a separate field). The pipeline's
        // `append_openai_raw` -> `finish()` flow extracts
        // `delta.content` from each stored payload to rebuild the
        // persisted message's `content` field, so this MUST NOT
        // include thought text or thought text would leak into
        // the user's `content`. The thought text is routed to
        // `delta_reasoning` (which the pipeline separately feeds
        // to `append_reasoning`).
        (joined, dr)
    };

    // Extract finish_reason
    let finish_reason = probe
        .candidates
        .first()
        .and_then(|c| c.finish_reason.as_deref())
        .map(map_gemini_finish_reason);

    // Build OpenAI chunk
    let choice = if let Some(ref reason) = finish_reason {
        serde_json::json!({
            "index": 0,
            "delta": if text.is_empty() { serde_json::json!({}) } else { serde_json::json!({"content": text}) },
            "finish_reason": reason,
        })
    } else {
        serde_json::json!({
            "index": 0,
            "delta": if text.is_empty() { serde_json::json!({}) } else { serde_json::json!({"content": text}) },
            "finish_reason": serde_json::Value::Null,
        })
    };

    let chunk = serde_json::json!({
        "id": chunk_id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [choice],
    });

    // Extract usage if present (final chunk). Uses `?` semantics to
    // match the original behavior: if any token count is missing or
    // the wrong type, the whole usage is `None`.
    let usage = probe.usage_metadata.and_then(|u| {
        Some(OpenAIUsage {
            prompt_tokens: u.prompt_tokens? as u32,
            completion_tokens: u.completion_tokens? as u32,
            total_tokens: u.total_tokens? as u32,
        })
    });

    Ok(Some(UpstreamSseChunk {
        raw_payload: None,
        payload: chunk,
        done: false,
        usage,
        stop_reason: finish_reason,
        delta_reasoning,
        delta_tool_calls: Vec::new(),
        has_content: true,
    }))
}

// =====================================================================
// Anthropic SSE parsing
// =====================================================================

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
        *current_event = Some(event_type.trim().to_string());
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
pub fn translate_anthropic_sse_payload(
    payload: &str,
    chunk_id: &str,
    created: u64,
    model: &str,
) -> Result<Option<UpstreamSseChunk>> {
    let (event_type, data_json) = if let Some(pos) = payload.find('\n') {
        (&payload[..pos], &payload[pos + 1..])
    } else {
        return Ok(None);
    };

    // Skip ping events
    if event_type == "ping" {
        return Ok(None);
    }

    let data: Value = serde_json::from_str(data_json)
        .map_err(|e| CoreError::Parse(format!("anthropic sse json: {e}")))?;

    match event_type {
        "message_start" => {
            // Return role-only chunk
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
            Ok(Some(UpstreamSseChunk {
                raw_payload: None,
                payload: chunk,
                done: false,
                usage: None,
                stop_reason: None,
                delta_reasoning: None,
                delta_tool_calls: Vec::new(),
                // message_start is a metadata-only event: it announces
                // the assistant role but carries no generated tokens.
                // Must NOT reset the idle_chunk timer — the model
                // hasn't started producing content yet.
                has_content: false,
            }))
        }
        "content_block_delta" => {
            // Determine the delta subtype. Anthropic distinguishes
            //   - text_delta        (regular text)
            //   - thinking_delta    (extended thinking / Claude reasoning)
            //   - input_json_delta  (tool_use argument fragments;
            //                       handled by `translate_anthropic_sse_event`
            //                       and the H5 accumulator, NOT here)
            // Missing `type` defaults to text_delta (legacy clients).
            let delta_type = data
                .get("delta")
                .and_then(|d| d.get("type"))
                .and_then(|t| t.as_str())
                .unwrap_or("text_delta");

            if delta_type == "thinking_delta" {
                // Anthropic extended thinking. Extract `delta.thinking`
                // and surface it as `delta_reasoning` on the chunk so
                // the downstream accumulator (sse_accumulator.rs) can
                // build the persisted `reasoning` field.
                let thinking = data
                    .get("delta")
                    .and_then(|d| d.get("thinking"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                if thinking.is_empty() {
                    return Ok(None);
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
                return Ok(Some(UpstreamSseChunk {
                    raw_payload: None,
                    payload: chunk,
                    done: false,
                    usage: None,
                    stop_reason: None,
                    delta_reasoning: Some(thinking.to_string()),
                    delta_tool_calls: Vec::new(),
                    has_content: true,
                }));
            }

            // text_delta (or unknown subtype — fall back to text
            // extraction to preserve existing behavior for any
            // future subtype we don't recognize).
            let text = data
                .get("delta")
                .and_then(|d| d.get("text"))
                .and_then(|t| t.as_str())
                .unwrap_or("");

            if text.is_empty() {
                return Ok(None);
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
            Ok(Some(UpstreamSseChunk {
                raw_payload: None,
                payload: chunk,
                done: false,
                usage: None,
                stop_reason: None,
                delta_reasoning: None,
                delta_tool_calls: Vec::new(),
                has_content: true,
            }))
        }
        "message_delta" => {
            // Extract finish reason and usage
            let stop_reason = data
                .get("delta")
                .and_then(|d| d.get("stop_reason"))
                .and_then(|r| r.as_str());

            let finish_reason = match stop_reason {
                Some("end_turn") | Some("stop_sequence") => Some("stop".to_string()),
                Some("max_tokens") => Some("length".to_string()),
                _ => None,
            };

            let usage = data.get("usage").map(|u| {
                crate::translation::OpenAIUsage {
                    prompt_tokens: u.get("input_tokens").and_then(|t| t.as_u64()).unwrap_or(0)
                        as u32,
                    completion_tokens: u.get("output_tokens").and_then(|t| t.as_u64()).unwrap_or(0)
                        as u32,
                    total_tokens: 0, // Will be computed
                }
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
            Ok(Some(UpstreamSseChunk {
                raw_payload: None,
                payload: chunk,
                done: true,
                usage,
                stop_reason: stop_reason.map(|s| s.to_string()),
                delta_reasoning: None,
                delta_tool_calls: Vec::new(),
                // message_delta carries only stop_reason + usage —
                // no generated tokens. Must NOT reset the idle_chunk
                // timer.
                has_content: false,
            }))
        }
        "message_stop" => {
            // H4 fix: `message_delta` already emitted the
            // `done: true` chunk (line 307). `message_stop` is just
            // Anthropic's closing handshake and would otherwise
            // produce a SECOND `done: true` chunk downstream — and
            // combined with the post-loop `[DONE]` at pipeline.rs
            // :2431 the client would see three end-of-stream
            // signals. Swallow it.
            Ok(None)
        }
        _ => Ok(None), // content_block_start, content_block_stop, etc.
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

pub fn translate_anthropic_sse_event(
    payload: &str,
    chunk_id: &str,
    created: u64,
    model: &str,
    tool_use_acc: &mut Option<AnthropicToolUseAccumulator>,
    tool_call_index_counter: &mut u32,
) -> Result<Option<UpstreamSseChunk>> {
    let (event_type, data_json) = match payload.find('\n') {
        Some(pos) => (&payload[..pos], &payload[pos + 1..]),
        None => return Ok(None),
    };

    // Skip ping events.
    if event_type == "ping" {
        return Ok(None);
    }

    // Fast path for content_block_delta: use a targeted probe that
    // only extracts delta.{type,text,thinking,partial_json}, avoiding
    // the full Value AST allocation on the streaming hot path.
    if event_type == "content_block_delta" {
        let probe: AnthropicContentBlockDeltaProbe = serde_json::from_str(data_json)
            .map_err(|e| CoreError::Parse(format!("anthropic sse json: {e}")))?;
        let delta = probe.delta.unwrap_or_default();
        let delta_type = delta.delta_type.as_deref().unwrap_or("");

        if delta_type == "input_json_delta" {
            // We need an open accumulator to receive deltas. If
            // somehow we don't (malformed stream), drop the
            // fragment rather than emit a chunk with a phantom
            // tool call.
            if let Some(acc) = tool_use_acc.as_mut() {
                // Capture the running length BEFORE appending so
                // the downstream accumulator (sse_accumulator.rs)
                // can append only the NEW fragment, not the
                // whole running total (which would double-encode
                // the arguments JSON across the wire chunks).
                let prev_len = acc.arguments.len();
                if let Some(partial) = delta.partial_json.as_deref() {
                    acc.arguments.push_str(partial);
                }
                let new_fragment = &acc.arguments[prev_len..];
                // Emit a chunk that carries ONLY the newly-appended
                // fragment in `arguments`. The OpenAI streaming
                // tool_calls spec requires each chunk to carry a
                // FRAGMENT of the arguments JSON; the client
                // concatenates fragments by `index`. Sending the
                // running total here would cause the client to
                // concatenate f1 + (f1+f2) + (f1+f2+f3) + ...,
                // duplicating early fragments N times — which is
                // exactly the "tool call arguments duplicated"
                // bug the user reported.
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
                // (`update_anthropic_tool_use(Delta { partial_json })`)
                // appends it once to its in-flight tool_call's
                // arguments — otherwise the running total gets
                // concatenated with itself across chunks and the
                // persisted arguments string is the running total
                // repeated per fragment (broken JSON).
                let tool_call_obj = serde_json::json!({
                    "index": acc.index,
                    "function": {
                        "arguments": new_fragment,
                    }
                });
                return Ok(Some(UpstreamSseChunk {
                    raw_payload: None,
                    payload: chunk,
                    done: false,
                    usage: None,
                    stop_reason: None,
                    delta_reasoning: None,
                    delta_tool_calls: vec![tool_call_obj],
                    has_content: true,
                }));
            }
            // No accumulator open — drop the fragment.
            return Ok(None);
        }
        if delta_type == "thinking_delta" {
            // Anthropic extended thinking. Extract `delta.thinking`
            // and surface it as `delta_reasoning` on the chunk so
            // the downstream accumulator (sse_accumulator.rs) can
            // persist it as `choices[0].message.reasoning_content`.
            // Mirrors the structure of the stateless
            // `translate_anthropic_sse_payload` thinking_delta
            // branch (sse.rs:412-444).
            let thinking = delta.thinking.as_deref().unwrap_or("");
            if thinking.is_empty() {
                return Ok(None);
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
            return Ok(Some(UpstreamSseChunk {
                raw_payload: None,
                payload: chunk,
                done: false,
                usage: None,
                stop_reason: None,
                delta_reasoning: Some(thinking.to_string()),
                delta_tool_calls: Vec::new(),
                has_content: true,
            }));
        }
        // text_delta (or unknown subtype — fall back to text
        // extraction to preserve existing behavior for any
        // future subtype we don't recognize).
        let text = delta.text.as_deref().unwrap_or("");
        if text.is_empty() {
            return Ok(None);
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
        return Ok(Some(UpstreamSseChunk {
            raw_payload: None,
            payload: chunk,
            done: false,
            usage: None,
            stop_reason: None,
            delta_reasoning: None,
            delta_tool_calls: Vec::new(),
            has_content: true,
        }));
    }

    // Fast path for content_block_start: use a targeted probe for the
    // tool_use dispatch. Only allocates the `type`/`id`/`name` strings
    // — serde skips the rest of the AST.
    if event_type == "content_block_start" {
        let probe: AnthropicContentBlockStartProbe = serde_json::from_str(data_json)
            .map_err(|e| CoreError::Parse(format!("anthropic sse json: {e}")))?;
        let block_type = probe
            .content_block
            .as_ref()
            .and_then(|b| b.block_type.as_deref())
            .unwrap_or("");
        if block_type == "tool_use" {
            let id = probe
                .content_block
                .as_ref()
                .and_then(|b| b.id.as_deref())
                .unwrap_or("")
                .to_string();
            let name = probe
                .content_block
                .as_ref()
                .and_then(|b| b.name.as_deref())
                .unwrap_or("")
                .to_string();
            // Allocate a new tool_call index for this turn.
            let index = *tool_call_index_counter;
            *tool_call_index_counter += 1;
            *tool_use_acc = Some(AnthropicToolUseAccumulator {
                index,
                id: id.clone(),
                name: name.clone(),
                arguments: String::new(),
            });
            // Emit the initial OpenAI-style tool_call chunk with
            // id+type+name and empty arguments (the standard
            // OpenAI streaming-tools shape). `finish_reason` stays
            // null because more chunks for this same choice index
            // are coming.
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
            // Mirror the wire-level tool_call record into
            // `delta_tool_calls` so the downstream accumulator
            // (sse_accumulator.rs) can build its tool_calls
            // list without re-parsing the wire payload.
            let tool_call_obj = serde_json::json!({
                "index": index,
                "id": id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": ""
                }
            });
            return Ok(Some(UpstreamSseChunk {
                raw_payload: None,
                payload: chunk,
                done: false,
                usage: None,
                stop_reason: None,
                delta_reasoning: None,
                delta_tool_calls: vec![tool_call_obj],
                // content_block_start (tool_use) is a metadata-only
                // event: it announces the tool call id+name with EMPTY
                // arguments. The actual argument tokens come later in
                // content_block_delta (input_json_delta) events. Must
                // NOT reset the idle_chunk timer here — the model
                // hasn't produced any argument tokens yet. This was
                // the root cause of the user-visible bug where
                // MiniMax-M3 tool calls failed with "idle_chunk after
                // 10000ms" even though ttft_ms was Some(0): the
                // content_block_start event arrived at ~0ms, the
                // pipeline called note_content_chunk(), the chunk-gap
                // timer started, and 10s later the timer fired while
                // the model was still generating the first argument
                // fragment.
                has_content: false,
            }));
        }
        // Non-tool_use content_block_start (e.g. text block) —
        // fall through to Ok(None); the content_block_delta arm
        // handles the actual emission.
        return Ok(None);
    }

    if event_type == "content_block_stop" {
        // Close out the accumulator. We don't emit a chunk here;
        // the next `message_delta` or `message_stop` will carry
        // finish_reason and any final usage. The client can
        // detect the tool_call is complete by index reuse.
        *tool_use_acc = None;
        return Ok(None);
    }

    // For all other events (message_start, message_delta, message_stop,
    // and unknown future events), defer to the stateless translator so
    // they keep their existing behavior. These events are O(1) per
    // response (not per chunk), so the Value-based parse is acceptable.
    let rebuilt = format!("{event_type}\n{data_json}");
    translate_anthropic_sse_payload(&rebuilt, chunk_id, created, model)
}

// =====================================================================
// Formatting
// =====================================================================

/// Format a JSON value as an SSE `data:` line.
pub fn format_sse_line(payload: &serde_json::Value) -> String {
    format!(
        "data: {}\n\n",
        serde_json::to_string(payload).unwrap_or_default()
    )
}

/// The [DONE] sentinel.
pub const SSE_DONE: &str = "data: [DONE]\n\n";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_openai_data_line() {
        let line = r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":0,"model":"gpt-4","choices":[{"index":0,"delta":{"content":"Hi"},"finish_reason":null}]}"#;
        let chunk = parse_openai_sse_line(line).unwrap().unwrap();
        assert!(!chunk.done);
        assert!(chunk.raw_payload.is_some());
        let v: serde_json::Value =
            serde_json::from_str(chunk.raw_payload.as_ref().unwrap()).unwrap();
        assert!(v.get("id").is_some());
    }

    #[test]
    fn parse_openai_done() {
        let chunk = parse_openai_sse_line("data: [DONE]").unwrap().unwrap();
        assert!(chunk.done);
    }

    #[test]
    fn parse_openai_empty_line() {
        assert!(parse_openai_sse_line("").unwrap().is_none());
    }

    #[test]
    fn parse_openai_comment() {
        assert!(
            parse_openai_sse_line(": this is a comment")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn parse_gemini_data_line() {
        let line = r#"data: {"candidates":[{"content":{"parts":[{"text":"Hello"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"totalTokenCount":15}}"#;
        let chunk = parse_gemini_sse_line(line, "test-id", 0, "gemini-pro")
            .unwrap()
            .unwrap();
        assert!(!chunk.done);
        let choice = chunk.payload["choices"][0].clone();
        assert_eq!(choice["delta"]["content"].as_str().unwrap(), "Hello");
        assert_eq!(choice["finish_reason"].as_str().unwrap(), "stop");
        assert!(chunk.usage.is_some());
        let u = chunk.usage.unwrap();
        assert_eq!(u.prompt_tokens, 10);
        assert_eq!(u.completion_tokens, 5);
    }

    #[test]
    fn parse_gemini_done() {
        let chunk = parse_gemini_sse_line("data: [DONE]", "id", 0, "m")
            .unwrap()
            .unwrap();
        assert!(chunk.done);
    }

    #[test]
    fn format_sse_line_produces_correct_output() {
        let v = serde_json::json!({"test": true});
        let line = format_sse_line(&v);
        assert_eq!(line, "data: {\"test\":true}\n\n");
    }

    // =====================================================================
    // Additional SSE edge-case tests
    // =====================================================================

    #[test]
    fn openai_line_without_data_prefix_returns_none() {
        // Lines that don't start with "data:" should be silently skipped.
        assert!(
            parse_openai_sse_line("event: some_event")
                .unwrap()
                .is_none()
        );
        assert!(parse_openai_sse_line("id: 12345").unwrap().is_none());
        assert!(parse_openai_sse_line("retry: 5000").unwrap().is_none());
        assert!(
            parse_openai_sse_line("random text without prefix")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn openai_line_with_event_prefix_ignored() {
        // Standard SSE event: lines should be ignored (not data: lines).
        assert!(parse_openai_sse_line("event: message").unwrap().is_none());
        assert!(
            parse_openai_sse_line("event: completion")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn openai_line_with_crlf_ending() {
        // \r\n line endings (common in HTTP) should be stripped.
        let line = "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"gpt-4\",\"choices\":[]}\r\n";
        let chunk = parse_openai_sse_line(line).unwrap().unwrap();
        assert!(!chunk.done);
    }

    #[test]
    fn openai_done_with_crlf() {
        let chunk = parse_openai_sse_line("data: [DONE]\r\n").unwrap().unwrap();
        assert!(chunk.done);
    }

    #[test]
    fn openai_long_line() {
        // A very long SSE data line (10KB payload) should parse without issues.
        let long_content = "x".repeat(10_000);
        let payload = serde_json::json!({"content": long_content});
        let line = format!("data: {}", serde_json::to_string(&payload).unwrap());
        let chunk = parse_openai_sse_line(&line).unwrap().unwrap();
        assert!(!chunk.done);
        let v: serde_json::Value =
            serde_json::from_str(chunk.raw_payload.as_ref().unwrap()).unwrap();
        assert_eq!(v["content"].as_str().unwrap().len(), 10_000);
    }

    #[test]
    fn openai_unicode_content() {
        let payload = serde_json::json!({"content": "こんにちは世界 🌍 ñ ü ö ä"});
        let line = format!("data: {}", serde_json::to_string(&payload).unwrap());
        let chunk = parse_openai_sse_line(&line).unwrap().unwrap();
        let v: serde_json::Value =
            serde_json::from_str(chunk.raw_payload.as_ref().unwrap()).unwrap();
        assert_eq!(v["content"].as_str().unwrap(), "こんにちは世界 🌍 ñ ü ö ä");
    }

    #[test]
    fn openai_malformed_json_returns_error() {
        let result = parse_openai_sse_line("data: {not valid json}");
        assert!(result.is_err(), "malformed JSON should produce an error");
        match result {
            Err(CoreError::Parse(_)) => {} // expected
            Err(other) => panic!("expected Parse error, got: {other}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }

    #[test]
    fn openai_multiple_sequential_lines_processed_independently() {
        // Simulate processing multiple SSE lines one by one, as a real stream would.
        let lines = vec![
            r#"data: {"id":"1","object":"chat.completion.chunk","created":0,"model":"gpt-4","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}"#,
            r#"data: {"id":"1","object":"chat.completion.chunk","created":0,"model":"gpt-4","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}"#,
            r#"data: {"id":"1","object":"chat.completion.chunk","created":0,"model":"gpt-4","choices":[{"index":0,"delta":{"content":" world"},"finish_reason":null}]}"#,
            "data: [DONE]",
        ];
        let mut contents = Vec::new();
        for line in lines {
            let chunk = parse_openai_sse_line(line).unwrap().unwrap();
            if chunk.done {
                break;
            }
            if let Some(ref raw) = chunk.raw_payload {
                let v: serde_json::Value = serde_json::from_str(raw).unwrap();
                if let Some(content) = v["choices"][0]["delta"]["content"].as_str() {
                    contents.push(content.to_string());
                }
            }
        }
        assert_eq!(contents.join(""), "Hello world");
    }

    #[test]
    fn openai_usage_in_chunk() {
        let payload = serde_json::json!({
            "id": "x",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "gpt-4",
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30}
        });
        let line = format!("data: {}", serde_json::to_string(&payload).unwrap());
        let chunk = parse_openai_sse_line(&line).unwrap().unwrap();
        assert!(chunk.usage.is_some());
        let u = chunk.usage.unwrap();
        assert_eq!(u.prompt_tokens, 10);
        assert_eq!(u.completion_tokens, 20);
        assert_eq!(u.total_tokens, 30);
    }

    // ---- Gemini SSE edge cases ----

    #[test]
    fn gemini_line_without_data_prefix_returns_none() {
        assert!(
            parse_gemini_sse_line("event: some_event", "id", 0, "m")
                .unwrap()
                .is_none()
        );
        assert!(
            parse_gemini_sse_line("id: 12345", "id", 0, "m")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn gemini_line_with_crlf_ending() {
        let line = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hi\"}]}}]}\r\n";
        let chunk = parse_gemini_sse_line(line, "test", 0, "gemini")
            .unwrap()
            .unwrap();
        assert!(!chunk.done);
        assert_eq!(
            chunk.payload["choices"][0]["delta"]["content"]
                .as_str()
                .unwrap(),
            "Hi"
        );
    }

    #[test]
    fn gemini_done_with_crlf() {
        let chunk = parse_gemini_sse_line("data: [DONE]\r\n", "id", 0, "m")
            .unwrap()
            .unwrap();
        assert!(chunk.done);
    }

    #[test]
    fn gemini_empty_line() {
        assert!(parse_gemini_sse_line("", "id", 0, "m").unwrap().is_none());
    }

    #[test]
    fn gemini_comment_line() {
        assert!(
            parse_gemini_sse_line(": this is a comment", "id", 0, "m")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn gemini_malformed_json_returns_error() {
        let result = parse_gemini_sse_line("data: {not json}", "id", 0, "m");
        assert!(result.is_err(), "malformed JSON should produce an error");
        match result {
            Err(CoreError::Parse(_)) => {} // expected
            Err(other) => panic!("expected Parse error, got: {other}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }

    #[test]
    fn gemini_no_candidates_in_payload() {
        // Payload with no candidates array — text should be empty string, no error.
        let line = r#"data: {"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":0,"totalTokenCount":1}}"#;
        let chunk = parse_gemini_sse_line(line, "id", 0, "m").unwrap().unwrap();
        assert!(!chunk.done);
        // No text, no finish_reason → delta.content should be empty/null.
        let delta = &chunk.payload["choices"][0]["delta"];
        assert!(
            delta.get("content").is_none() || delta["content"].as_str().unwrap_or("").is_empty()
        );
    }

    #[test]
    fn gemini_multiple_text_parts_concatenated() {
        let line =
            r#"data: {"candidates":[{"content":{"parts":[{"text":"Hello "},{"text":"World"}]}}]}"#;
        let chunk = parse_gemini_sse_line(line, "id", 0, "m").unwrap().unwrap();
        assert_eq!(
            chunk.payload["choices"][0]["delta"]["content"]
                .as_str()
                .unwrap(),
            "Hello World"
        );
    }

    #[test]
    fn gemini_finish_reason_max_tokens_maps_to_length() {
        let line = r#"data: {"candidates":[{"content":{"parts":[]},"finishReason":"MAX_TOKENS"}]}"#;
        let chunk = parse_gemini_sse_line(line, "id", 0, "m").unwrap().unwrap();
        assert_eq!(
            chunk.payload["choices"][0]["finish_reason"]
                .as_str()
                .unwrap(),
            "length"
        );
    }

    #[test]
    fn gemini_finish_reason_safety_maps_to_content_filter() {
        let line = r#"data: {"candidates":[{"content":{"parts":[]},"finishReason":"SAFETY"}]}"#;
        let chunk = parse_gemini_sse_line(line, "id", 0, "m").unwrap().unwrap();
        assert_eq!(
            chunk.payload["choices"][0]["finish_reason"]
                .as_str()
                .unwrap(),
            "content_filter"
        );
    }

    #[test]
    fn gemini_long_line() {
        let long_text = "y".repeat(10_000);
        let payload =
            serde_json::json!({"candidates":[{"content":{"parts":[{"text": long_text}]}}]});
        let line = format!("data: {}", serde_json::to_string(&payload).unwrap());
        let chunk = parse_gemini_sse_line(&line, "id", 0, "gemini")
            .unwrap()
            .unwrap();
        assert_eq!(
            chunk.payload["choices"][0]["delta"]["content"]
                .as_str()
                .unwrap()
                .len(),
            10_000
        );
    }

    #[test]
    fn gemini_unicode_content() {
        let payload =
            serde_json::json!({"candidates":[{"content":{"parts":[{"text":"日本語テスト 🎉"}]}}]});
        let line = format!("data: {}", serde_json::to_string(&payload).unwrap());
        let chunk = parse_gemini_sse_line(&line, "id", 0, "gemini")
            .unwrap()
            .unwrap();
        assert_eq!(
            chunk.payload["choices"][0]["delta"]["content"]
                .as_str()
                .unwrap(),
            "日本語テスト 🎉"
        );
    }

    #[test]
    fn gemini_chunk_metadata_fields() {
        let payload = serde_json::json!({"candidates":[{"content":{"parts":[{"text":"hi"}]}}]});
        let line = format!("data: {}", serde_json::to_string(&payload).unwrap());
        let chunk = parse_gemini_sse_line(&line, "chunk-42", 1234567890, "gemini-pro")
            .unwrap()
            .unwrap();
        assert_eq!(chunk.payload["id"].as_str().unwrap(), "chunk-42");
        assert_eq!(chunk.payload["created"].as_u64().unwrap(), 1234567890);
        assert_eq!(chunk.payload["model"].as_str().unwrap(), "gemini-pro");
        assert_eq!(
            chunk.payload["object"].as_str().unwrap(),
            "chat.completion.chunk"
        );
    }

    #[test]
    fn gemini_usage_without_finish_reason() {
        // Usage present but no finishReason — should still parse usage.
        let line = r#"data: {"candidates":[{"content":{"parts":[{"text":"a"}]}}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":7,"totalTokenCount":10}}"#;
        let chunk = parse_gemini_sse_line(line, "id", 0, "m").unwrap().unwrap();
        assert!(chunk.usage.is_some());
        let u = chunk.usage.unwrap();
        assert_eq!(u.prompt_tokens, 3);
        assert_eq!(u.completion_tokens, 7);
        assert_eq!(u.total_tokens, 10);
        // finish_reason should be null (not present).
        assert!(chunk.payload["choices"][0]["finish_reason"].is_null());
    }

    // ---- format_sse_line edge cases ----

    #[test]
    fn format_sse_line_with_null() {
        let line = format_sse_line(&Value::Null);
        assert_eq!(line, "data: null\n\n");
    }

    #[test]
    fn format_sse_line_with_empty_object() {
        let line = format_sse_line(&serde_json::json!({}));
        assert_eq!(line, "data: {}\n\n");
    }

    #[test]
    fn sse_done_constant_value() {
        assert_eq!(SSE_DONE, "data: [DONE]\n\n");
    }

    #[test]
    fn openai_data_prefix_with_extra_spaces() {
        // "data:  {" (extra space) should still work — trim_start handles it.
        let line = r#"data:  {"id":"x","object":"chat.completion.chunk","created":0,"model":"gpt-4","choices":[]}"#;
        let chunk = parse_openai_sse_line(line).unwrap().unwrap();
        assert!(!chunk.done);
    }

    #[test]
    fn gemini_data_prefix_with_extra_spaces() {
        let line = r#"data:  {"candidates":[{"content":{"parts":[{"text":"ok"}]}}]}"#;
        let chunk = parse_gemini_sse_line(line, "id", 0, "m").unwrap().unwrap();
        assert_eq!(
            chunk.payload["choices"][0]["delta"]["content"]
                .as_str()
                .unwrap(),
            "ok"
        );
    }

    #[test]
    fn gemini_only_whitespace_line() {
        assert!(
            parse_gemini_sse_line("   \t  ", "id", 0, "m")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn openai_only_whitespace_line() {
        assert!(parse_openai_sse_line("   \t  ").unwrap().is_none());
    }

    #[test]
    fn gemini_only_ellipsis_tokens() {
        // Empty parts array — no text extracted.
        let line = r#"data: {"candidates":[{"content":{"parts":[]}}]}"#;
        let chunk = parse_gemini_sse_line(line, "id", 0, "m").unwrap().unwrap();
        // text is empty → delta.content should be empty string or null.
        let content = chunk.payload["choices"][0]["delta"]["content"]
            .as_str()
            .unwrap_or("");
        assert!(content.is_empty());
    }

    #[test]
    fn gemini_parts_with_non_text_fields_ignored() {
        // Some parts may have "thought: true" or other keys — only "text" parts matter.
        let line = r#"data: {"candidates":[{"content":{"parts":[{"thought":true},{"text":"real answer"}]}}]}"#;
        let chunk = parse_gemini_sse_line(line, "id", 0, "m").unwrap().unwrap();
        assert_eq!(
            chunk.payload["choices"][0]["delta"]["content"]
                .as_str()
                .unwrap(),
            "real answer"
        );
    }

    /// G1 §5.4 (test 9): Gemini input `[{"text":"r","thought":true},{"text":"a"}]`
    /// must route the thought:true part into `delta_reasoning`
    /// and leave the non-thought text as the only content in the
    /// translated payload's `delta.content`, so the downstream
    /// accumulator can persist the user's `content` and the
    /// model's reasoning into separate fields. Without the split,
    /// the thought text leaks into the persisted `content` and
    /// the response is corrupted.
    #[test]
    fn gemini_streaming_response_body_separates_thought_from_text() {
        let line = r#"data: {"candidates":[{"content":{"parts":[{"text":"r","thought":true},{"text":"a"}]}}]}"#;
        let chunk = parse_gemini_sse_line(line, "id", 0, "m").unwrap().unwrap();
        // Thought text is routed to reasoning so the accumulator
        // can persist it as `choices[0].message.reasoning_content`.
        assert_eq!(
            chunk.delta_reasoning.as_deref(),
            Some("r"),
            "delta_reasoning must contain the thought:true text"
        );
        // The OpenAI-translated payload's `delta.content` carries
        // ONLY the non-thought text. This matches OpenAI streaming
        // convention where reasoning is a separate field; the
        // pipeline's `append_openai_raw` -> `finish()` flow extracts
        // `delta.content` to rebuild the persisted message's
        // `content`, so any thought text here would leak into the
        // user's `content` and corrupt the response.
        assert_eq!(
            chunk.payload["choices"][0]["delta"]["content"]
                .as_str()
                .unwrap(),
            "a",
            "the translated payload's delta.content must carry ONLY non-thought text"
        );
    }

    // ---- Anthropic SSE tests ----

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
        let concatenated = format!("{}{}", fragment1, fragment2);
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
            r#"data: {}"#,
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
            let barrier = barrier.clone();
            joins.spawn(async move {
                let chunk_id = format!("chatcmpl-{}", i);
                let model = format!("claude-isolated-{}", i);
                let mut tool_use_acc: Option<AnthropicToolUseAccumulator> = None;
                let mut tool_call_index_counter: u32 = 0;

                // Wait until all tasks are queued so they race in
                // parallel (not sequentially).
                barrier.wait().await;

                // Sequence: content_block_start (tool_use) → deltas →
                // message_delta (stop). Each task sees a distinct
                // tool id+name so any cross-talk would be visible.
                let id = format!("toolu_{:08x}", i);
                let name = format!("fn_{}", i);
                let start_payload = format!(
                    "content_block_start\n{{\"content_block\":{{\"type\":\"tool_use\",\"id\":\"{}\",\"name\":\"{}\",\"input\":{{}}}}}}",
                    id, name
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
            assert_eq!(chunk_id, format!("chatcmpl-{}", i));
            assert_eq!(model, format!("claude-isolated-{}", i));
            assert!(seen_ids.insert(chunk_id.clone()), "duplicate chunk_id");
            assert!(seen_models.insert(model.clone()), "duplicate model");

            // First chunk must carry THIS task's tool id and name,
            // not any other task's.
            let first_payload = outs[0].as_ref().expect("first chunk").payload.clone();
            let tool_id = first_payload["choices"][0]["delta"]["tool_calls"][0]["id"]
                .as_str()
                .expect("tool id");
            let tool_name =
                first_payload["choices"][0]["delta"]["tool_calls"][0]["function"]["name"]
                    .as_str()
                    .expect("tool name");
            assert_eq!(
                tool_id,
                format!("toolu_{:08x}", i),
                "tool id leaked from another parallel task"
            );
            assert_eq!(
                tool_name,
                format!("fn_{}", i),
                "tool name leaked from another parallel task"
            );

            // Model and chunk_id in the wire payload also must be
            // THIS task's, not a peer's.
            assert_eq!(first_payload["model"].as_str().unwrap(), model);
            assert_eq!(first_payload["id"].as_str().unwrap(), chunk_id);
        }
        assert_eq!(seen_ids.len(), N, "expected {} unique chunk_ids", N);
    }
}

pub const MAX_SSE_LINE_BYTES: usize = 65_536;

pub struct SseParser {
    buffer: bytes::BytesMut,
    max_line_bytes: usize,
}

impl SseParser {
    pub fn new(max_line_bytes: usize) -> Self {
        Self {
            buffer: bytes::BytesMut::with_capacity(8192),
            max_line_bytes,
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<()> {
        self.buffer.extend_from_slice(chunk);
        if self.buffer.len() > self.max_line_bytes {
            return Err(CoreError::UpstreamConnection(format!(
                "SSE line buffer exceeded {} bytes (memory-DoS guard)",
                self.max_line_bytes
            )));
        }
        Ok(())
    }

    pub fn next_line(&mut self) -> Option<bytes::BytesMut> {
        use bytes::Buf;
        if let Some(pos) = memchr::memchr(b'\n', &self.buffer) {
            let line_bytes = self.buffer.split_to(pos);
            self.buffer.advance(1); // skip '\n'

            // Pre-reserve buffer space to avoid repeated reallocations
            if self.buffer.capacity() - self.buffer.len() < 4096 {
                self.buffer.reserve(16384);
            }
            Some(line_bytes)
        } else {
            None
        }
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    pub fn remaining_bytes(&self) -> &[u8] {
        &self.buffer
    }
}

pub fn skip_leading_spaces(bytes: &[u8]) -> &[u8] {
    let mut i = 0;
    while i < bytes.len() && bytes[i] == b' ' {
        i += 1;
    }
    &bytes[i..]
}

pub fn sse_payload_needs_parse(payload: &str) -> bool {
    let bytes = payload.as_bytes();
    let mut has_usage = false;
    let mut has_finish_reason = false;
    let mut has_finish_reason_null = false;

    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            if !has_usage && i + 9 <= bytes.len() && &bytes[i..i + 9] == b"\"usage\":{" {
                has_usage = true;
            }
            if !has_finish_reason
                && i + 14 <= bytes.len()
                && &bytes[i..i + 14] == b"\"finish_reason"
            {
                has_finish_reason = true;
                if i + 20 <= bytes.len() && &bytes[i + 14..i + 20] == b"\":null" {
                    has_finish_reason_null = true;
                }
            }
            if has_usage || (has_finish_reason && !has_finish_reason_null) {
                return true;
            }
            i += 1;
            while i < bytes.len() && bytes[i] != b'"' {
                i += 1;
            }
        }
        i += 1;
    }

    has_usage || (has_finish_reason && !has_finish_reason_null)
}
