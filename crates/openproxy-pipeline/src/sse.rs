//! SSE (Server-Sent Events) parsing and translation for streaming responses.
//!
//! Provides parsers for OpenAI and Gemini upstream SSE formats, translating
//! them into OpenAI-format SSE chunks that clients expect.

use crate::translation::OpenAIUsage;
use openproxy_types::error::{CoreError, Result};
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
    /// Create a new chunk from a parsed JSON payload with default metadata.
    pub fn new(payload: Value) -> Self {
        Self {
            raw_payload: None,
            payload,
            done: false,
            usage: None,
            stop_reason: None,
            delta_reasoning: None,
            delta_tool_calls: Vec::new(),
            has_content: true,
        }
    }

    /// Create a new [DONE] sentinel chunk.
    pub fn done() -> Self {
        Self {
            raw_payload: None,
            payload: Value::Null,
            done: true,
            usage: None,
            stop_reason: None,
            delta_reasoning: None,
            delta_tool_calls: Vec::new(),
            has_content: false,
        }
    }

    /// Get the forwardable JSON string. Returns the raw payload if
    /// available (zero allocation), otherwise serializes the parsed payload.
    pub fn into_json_string(self) -> String {
        self.raw_payload.unwrap_or_else(|| {
            serde_json::to_string(&self.payload).unwrap_or_else(|_| "{}".to_string())
        })
    }

    /// Get the SSE frame as pre-formatted `data: {json}\n\n` `Bytes`,
    /// ready for direct socket write. Avoids the intermediate `String`
    /// allocation when the frame is immediately written to the socket.
    pub fn into_sse_bytes(self) -> bytes::Bytes {
        if let Some(raw) = self.raw_payload {
            return build_sse_frame(&raw);
        }
        use bytes::BufMut;
        let mut b = bytes::BytesMut::with_capacity(256);
        b.extend_from_slice(b"data: ");
        if serde_json::to_writer((&mut b).writer(), &self.payload).is_err() {
            b.clear();
            b.extend_from_slice(b"data: {}");
        }
        b.extend_from_slice(b"\n\n");
        b.freeze()
    }
}

/// Build a `data: <payload>\n\n` SSE frame as `Bytes`, ready for socket write.
/// The `+ 16` covers `"data: "` (6) + `"\n\n"` (2) + slack for BytesMut's
/// allocation strategy. Caller passes the inner JSON (no leading `data: `).
pub fn build_sse_frame(payload: &str) -> bytes::Bytes {
    let mut b = bytes::BytesMut::with_capacity(payload.len() + 16);
    b.extend_from_slice(b"data: ");
    b.extend_from_slice(payload.as_bytes());
    b.extend_from_slice(b"\n\n");
    b.freeze()
}

/// Helper to extract data payload from an SSE line.
///
/// Trims line endings (`\r`, `\n`), ignores empty lines and comment lines (starting with `:`),
/// strips the `data:` prefix, and trims leading whitespace.
///
/// Returns `None` if the line is empty, a comment, not a `data:` line, or has an empty payload.
/// Otherwise returns `Some(payload)` (e.g. `Some("[DONE]")` or `Some("{\"content\":\"...\"}")`).
#[inline]
pub fn parse_sse_data_line(line: &str) -> Option<&str> {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    if trimmed.is_empty() || trimmed.starts_with(':') {
        return None;
    }
    let rest = trimmed.strip_prefix("data:")?;
    let payload = rest.trim_start();
    if payload.is_empty() {
        return None;
    }
    Some(payload)
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
/// Maximum allowed length for accumulated tool call arguments string.
/// Prevents unbounded memory growth from malicious input_json_delta fragments.
const MAX_TOOL_ARGUMENTS_BYTES: usize = 1_048_576; // 1 MiB

/// Maximum allowed length for tool call ID string.
const MAX_TOOL_ID_BYTES: usize = 256;

/// Maximum allowed length for tool call name string.
const MAX_TOOL_NAME_BYTES: usize = 256;

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
    #[serde(default)]
    prompt_tokens_details: Option<OpenAiPromptTokensDetailsProbe>,
    #[serde(default)]
    input_tokens_details: Option<OpenAiPromptTokensDetailsProbe>,
    #[serde(default)]
    cached_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
}

#[derive(serde::Deserialize)]
struct OpenAiPromptTokensDetailsProbe {
    #[serde(default)]
    cached_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
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
    let Some(payload) = parse_sse_data_line(line) else {
        return Ok(None);
    };
    if payload == "[DONE]" {
        return Ok(Some(UpstreamSseChunk::done()));
    }
    // Fast targeted parse: only extracts usage + finish_reason,
    // skips all other fields (delta.content, tool_calls, etc.)
    let probe: OpenAiSseProbe = serde_json::from_str(payload)
        .map_err(|e| CoreError::Parse(format!("openai sse json: {e}")))?;

    let usage = probe.usage.map(|u| {
        let cached = u
            .prompt_tokens_details
            .as_ref()
            .and_then(|d| d.cached_tokens.or(d.cache_read_input_tokens))
            .or_else(|| {
                u.input_tokens_details
                    .as_ref()
                    .and_then(|d| d.cached_tokens.or(d.cache_read_input_tokens))
            })
            .or(u.cached_tokens)
            .or(u.cache_read_input_tokens)
            .and_then(|c| u32::try_from(c).ok());

        OpenAIUsage {
            prompt_tokens: u.prompt_tokens.unwrap_or(0).try_into().unwrap_or(u32::MAX),
            completion_tokens: u
                .completion_tokens
                .unwrap_or(0)
                .try_into()
                .unwrap_or(u32::MAX),
            total_tokens: u.total_tokens.unwrap_or(0).try_into().unwrap_or(u32::MAX),
            prompt_tokens_details: cached.map(|c| openproxy_types::message::PromptTokensDetails {
                cached_tokens: Some(c),
            }),
        }
    });
    // o1-style reasoning models (o1, o3, deepseek-r1) emit
    // `delta.reasoning_content` on chunks that also carry `usage`
    // or a non-null `finish_reason` — i.e. the slow path. Surface
    // it on `delta_reasoning` so the pipeline's accumulator
    // (sse_accumulator.rs) can persist it as
    // `choices[0].message.reasoning_content`. The probe does the
    let (delta_reasoning, finish_reason) = match probe.choices.and_then(|mut c| c.pop()) {
        Some(choice) => {
            let reasoning = choice.delta.and_then(|d| d.reasoning_content);
            (reasoning, choice.finish_reason)
        }
        None => (None, None),
    };

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
    #[serde(default)]
    response: Option<GeminiInnerSseProbe>,
}

#[derive(serde::Deserialize, Default)]
struct GeminiInnerSseProbe {
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
fn extract_gemini_candidates(probe: &GeminiSseProbe) -> &[GeminiCandidateProbe] {
    if !probe.candidates.is_empty() {
        &probe.candidates
    } else if let Some(inner) = &probe.response {
        &inner.candidates
    } else {
        &probe.candidates
    }
}

fn extract_gemini_usage_metadata(probe: &GeminiSseProbe) -> Option<&GeminiUsageProbe> {
    probe.usage_metadata.as_ref().or_else(|| {
        probe
            .response
            .as_ref()
            .and_then(|r| r.usage_metadata.as_ref())
    })
}

fn extract_gemini_content_and_reasoning(
    candidates: &[GeminiCandidateProbe],
) -> (String, Option<String>) {
    let mut content_parts = String::new();
    let mut reasoning_parts = String::new();
    if let Some(candidate) = candidates.first()
        && let Some(content) = &candidate.content
    {
        for part in &content.parts {
            if let Some(t) = part.text.as_deref() {
                if part.thought.unwrap_or(false) {
                    reasoning_parts.push_str(t);
                } else {
                    content_parts.push_str(t);
                }
            }
        }
    }
    let dr = (!reasoning_parts.is_empty()).then_some(reasoning_parts);
    (content_parts, dr)
}

fn extract_gemini_usage(usage_metadata: Option<&GeminiUsageProbe>) -> Option<OpenAIUsage> {
    let u = usage_metadata?;
    Some(OpenAIUsage {
        prompt_tokens: u.prompt_tokens?.try_into().unwrap_or(u32::MAX),
        completion_tokens: u.completion_tokens?.try_into().unwrap_or(u32::MAX),
        total_tokens: u.total_tokens?.try_into().unwrap_or(u32::MAX),
        prompt_tokens_details: None,
    })
}

pub fn parse_gemini_sse_line(
    line: &str,
    chunk_id: &str,
    created: u64,
    model: &str,
) -> Result<Option<UpstreamSseChunk>> {
    let Some(payload) = parse_sse_data_line(line) else {
        return Ok(None);
    };
    if payload == "[DONE]" {
        return Ok(Some(UpstreamSseChunk::done()));
    }

    let probe: GeminiSseProbe = serde_json::from_str(payload)
        .map_err(|e| CoreError::Parse(format!("gemini sse json: {e}")))?;

    let candidates = extract_gemini_candidates(&probe);
    let usage_metadata = extract_gemini_usage_metadata(&probe);
    let (text, delta_reasoning) = extract_gemini_content_and_reasoning(candidates);

    let finish_reason = candidates
        .first()
        .and_then(|c| c.finish_reason.as_deref())
        .map(map_gemini_finish_reason);

    let delta = if text.is_empty() {
        serde_json::json!({})
    } else {
        serde_json::json!({"content": text})
    };
    let finish_val = finish_reason
        .as_ref()
        .map_or(serde_json::Value::Null, |r| serde_json::json!(r));

    let chunk = serde_json::json!({
        "id": chunk_id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": finish_val,
        }],
    });

    let usage = extract_gemini_usage(usage_metadata);

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

// =====================================================================
// Formatting
// =====================================================================

/// Format a JSON value as an SSE `data:` line.
pub fn format_sse_line(payload: &serde_json::Value) -> String {
    format!(
        "data: {}\n\n",
        serde_json::to_string(payload).unwrap_or_else(|_| "{}".to_string())
    )
}

/// The [DONE] sentinel.
pub const SSE_DONE: &str = "data: [DONE]\n\n";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_sse_data_line_variants() {
        assert_eq!(parse_sse_data_line(""), None);
        assert_eq!(parse_sse_data_line("\r\n"), None);
        assert_eq!(parse_sse_data_line(": keep-alive"), None);
        assert_eq!(parse_sse_data_line("event: message"), None);
        assert_eq!(parse_sse_data_line("data:"), None);
        assert_eq!(parse_sse_data_line("data:   \r\n"), None);
        assert_eq!(parse_sse_data_line("data: [DONE]"), Some("[DONE]"));
        assert_eq!(parse_sse_data_line("data:[DONE]\r\n"), Some("[DONE]"));
        assert_eq!(parse_sse_data_line("data: {\"a\":1}\n"), Some("{\"a\":1}"));
        assert_eq!(
            parse_sse_data_line("data:   hello world  \r\n"),
            Some("hello world  ")
        );
    }

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
        let choice = &chunk.payload["choices"][0];
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
        let chunk = parse_gemini_sse_line(&line, "chunk-42", 1_234_567_890, "gemini-pro")
            .unwrap()
            .unwrap();
        assert_eq!(chunk.payload["id"].as_str().unwrap(), "chunk-42");
        assert_eq!(chunk.payload["created"].as_u64().unwrap(), 1_234_567_890);
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

pub const MAX_SSE_LINE_BYTES: usize = 4_194_304; // 4 MiB
/// Maximum allowed bytes for an SSE event type string (e.g., "message_start", "content_block_delta").
/// Prevents unbounded memory allocation from malformed upstream event: lines.
pub const MAX_SSE_EVENT_TYPE_BYTES: usize = 1024;
/// Maximum allowed tool calls accumulated in ResponsesSseState.
/// Prevents unbounded vector growth from a malicious upstream.
pub const MAX_RESPONSES_TOOL_CALLS: usize = 128;
/// Maximum allowed bytes for accumulated tool call arguments in the Responses API path.
/// Prevents unbounded string growth per tool call from accumulated delta fragments.
pub const MAX_RESPONSES_TOOL_CALL_ARGS_BYTES: usize = 1_048_576; // 1 MiB

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
    let pos = bytes.iter().position(|&b| b != b' ').unwrap_or(bytes.len());
    &bytes[pos..]
}

fn check_finish_reason_non_null(payload: &str) -> bool {
    payload.find("\"finish_reason").is_some_and(|idx| {
        let start = idx + 14;
        if payload.is_char_boundary(start) {
            !payload[start..].starts_with("\":null")
        } else {
            false
        }
    })
}

pub fn sse_payload_needs_parse(payload: &str) -> bool {
    payload.contains("\"usage\":{") || check_finish_reason_non_null(payload)
}

#[derive(Default, Debug)]
pub struct ResponsesSseState {
    pub tool_calls: Vec<serde_json::Value>,
}

pub fn parse_responses_sse_stream_line(
    line: &str,
    chunk_id: &str,
    created: u64,
    model_name: &str,
    state: &mut ResponsesSseState,
) -> Result<Option<UpstreamSseChunk>> {
    let Some(data) = parse_sse_data_line(line) else {
        return Ok(None);
    };
    if data == "[DONE]" {
        return Ok(Some(UpstreamSseChunk::done()));
    }

    let value: Value = serde_json::from_str(data)
        .map_err(|e| CoreError::Parse(format!("responses SSE JSON parse: {e}")))?;

    if let Some(error) = value.get("error") {
        return Err(CoreError::upstream_error(
            500,
            "responses",
            model_name,
            error.to_string(),
            false,
        ));
    }

    let event_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let mut usage = None;
    if let Some(u) = value
        .get("usage")
        .or_else(|| value.get("response").and_then(|r| r.get("usage")))
        && let Ok(mut u_parsed) = <OpenAIUsage as serde::Deserialize>::deserialize(u)
    {
        if let Some(val) = u.get("input_tokens").and_then(serde_json::Value::as_u64) {
            u_parsed.prompt_tokens = val.try_into().unwrap_or(u32::MAX);
        }
        if let Some(val) = u.get("output_tokens").and_then(serde_json::Value::as_u64) {
            u_parsed.completion_tokens = val.try_into().unwrap_or(u32::MAX);
        }
        if u_parsed.total_tokens == 0 {
            u_parsed.total_tokens = u_parsed.prompt_tokens + u_parsed.completion_tokens;
        }
        usage = Some(u_parsed);
    }

    if event_type == "response.output_item.added"
        && let Some(item) = value.get("item")
    {
        let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if item_type == "function_call" {
            // Guard: prevent unbounded tool_calls vector growth
            if state.tool_calls.len() >= MAX_RESPONSES_TOOL_CALLS {
                tracing::warn!(
                    count = state.tool_calls.len(),
                    max = MAX_RESPONSES_TOOL_CALLS,
                    "ResponsesSseState: tool_calls limit reached — dropping new call"
                );
                return Ok(None);
            }
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("call_xyz")
                .to_string();
            let name = item
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            state.tool_calls.push(serde_json::json!({
                "id": &call_id,
                "type": "function",
                "function": { "name": &name, "arguments": "" }
            }));

            return Ok(Some(UpstreamSseChunk {
                raw_payload: None,
                payload: serde_json::json!({
                    "id": chunk_id,
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": model_name,
                    "choices": [{
                        "index": 0,
                        "delta": {
                            "tool_calls": [{
                                "index": state.tool_calls.len() - 1,
                                "id": call_id,
                                "type": "function",
                                "function": {
                                    "name": name,
                                    "arguments": ""
                                }
                            }]
                        }
                    }]
                }),
                done: false,
                usage: None,
                stop_reason: None,
                delta_reasoning: None,
                delta_tool_calls: vec![serde_json::json!({
                    "index": state.tool_calls.len() - 1,
                    "id": call_id,
                    "type": "function",
                    "function": { "name": name, "arguments": "" }
                })],
                has_content: false,
            }));
        }
    }

    if event_type == "response.function_call_arguments.delta"
        && let Some(delta) = value.get("delta").and_then(|v| v.as_str())
    {
        let call_id = value
            .get("call_id")
            .or_else(|| value.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if state.tool_calls.is_empty() {
            return Ok(None);
        }

        let mut index = state.tool_calls.len().saturating_sub(1);

        for (i, tc) in state.tool_calls.iter_mut().enumerate().rev() {
            if let Some(id) = tc.get("id").and_then(|v| v.as_str())
                && (id == call_id || call_id.is_empty())
            {
                if let Some(func) = tc.get_mut("function").and_then(|v| v.as_object_mut())
                    && let Some(args) = func.get_mut("arguments")
                    && let Some(args_str) = args.as_str()
                {
                    // Guard: prevent unbounded arguments accumulation
                    if args_str.len() + delta.len() > MAX_RESPONSES_TOOL_CALL_ARGS_BYTES {
                        tracing::warn!(
                            current_len = args_str.len(),
                            delta_len = delta.len(),
                            max = MAX_RESPONSES_TOOL_CALL_ARGS_BYTES,
                            "ResponsesSseState: tool call arguments limit reached — dropping delta"
                        );
                    } else {
                        let mut new_args = args_str.to_string();
                        new_args.push_str(delta);
                        *args = serde_json::Value::String(new_args);
                    }
                }
                index = i;
                break;
            }
        }

        return Ok(Some(UpstreamSseChunk {
            raw_payload: None,
            payload: serde_json::json!({
                "id": chunk_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model_name,
                "choices": [{
                    "index": 0,
                    "delta": {
                        "tool_calls": [{
                            "index": index,
                            "function": {
                                "arguments": delta
                            }
                        }]
                    }
                }]
            }),
            done: false,
            usage: None,
            stop_reason: None,
            delta_reasoning: None,
            delta_tool_calls: vec![serde_json::json!({
                "index": index,
                "function": { "arguments": delta }
            })],
            has_content: true,
        }));
    }

    if event_type == "response.content_part.added"
        && let Some(part) = value.get("part")
    {
        let text = part.get("text").and_then(|v| v.as_str()).unwrap_or("");
        if !text.is_empty() {
            return Ok(Some(UpstreamSseChunk {
                raw_payload: None,
                payload: serde_json::json!({
                    "id": chunk_id,
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": model_name,
                    "choices": [{
                        "index": 0,
                        "delta": {
                            "content": text
                        }
                    }]
                }),
                done: false,
                usage: None,
                stop_reason: None,
                delta_reasoning: None,
                delta_tool_calls: Vec::new(),
                has_content: true,
            }));
        }
    }

    if matches!(
        event_type,
        "response.output_text.delta" | "response.text.delta" | "response.audio.delta"
    ) {
        let delta = value.get("delta").and_then(|v| v.as_str()).unwrap_or("");
        if !delta.is_empty() {
            return Ok(Some(UpstreamSseChunk {
                raw_payload: None,
                payload: serde_json::json!({
                    "id": chunk_id,
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": model_name,
                    "choices": [{
                        "index": 0,
                        "delta": {
                            "content": delta
                        }
                    }]
                }),
                done: false,
                usage: None,
                stop_reason: None,
                delta_reasoning: None,
                delta_tool_calls: Vec::new(),
                has_content: true,
            }));
        }
    }

    if event_type == "response.done" || event_type == "response.completed" {
        let mut stop_reason = Some("stop".to_string());
        if !state.tool_calls.is_empty() {
            stop_reason = Some("tool_calls".to_string());
        }
        return Ok(Some(UpstreamSseChunk {
            raw_payload: None,
            payload: serde_json::json!({
                "id": chunk_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model_name,
                "choices": [{
                    "index": 0,
                    "delta": {},
                    "finish_reason": stop_reason
                }],
                "usage": usage
            }),
            done: false,
            usage,
            stop_reason,
            delta_reasoning: None,
            delta_tool_calls: Vec::new(),
            has_content: false,
        }));
    }

    Ok(None)
}

#[cfg(test)]
mod responses_sse_tests {
    use super::*;

    #[test]
    fn responses_output_text_delta_translates_to_chat_delta() {
        let mut state = ResponsesSseState::default();
        let line = r#"data: {"type":"response.output_text.delta","delta":"pong"}"#;

        let chunk =
            parse_responses_sse_stream_line(line, "chatcmpl_1", 123, "gpt-test", &mut state)
                .expect("parse")
                .expect("chunk");

        assert_eq!(
            chunk.payload["choices"][0]["delta"]["content"].as_str(),
            Some("pong")
        );
        assert!(chunk.has_content);
    }

    #[test]
    fn responses_completed_uses_nested_usage() {
        let mut state = ResponsesSseState::default();
        let line = r#"data: {"type":"response.completed","response":{"usage":{"prompt_tokens":2,"completion_tokens":3,"total_tokens":5}}}"#;

        let chunk =
            parse_responses_sse_stream_line(line, "chatcmpl_1", 123, "gpt-test", &mut state)
                .expect("parse")
                .expect("chunk");

        assert_eq!(
            chunk.payload["choices"][0]["finish_reason"].as_str(),
            Some("stop")
        );
        assert_eq!(chunk.usage.as_ref().map(|u| u.total_tokens), Some(5));
    }
}

/// Parse an Atomesus SSE line and translate it into an OpenAI-format chunk.
pub fn parse_atomesus_sse_line(
    line: &str,
    chunk_id: &str,
    created: u64,
    model_name: &str,
) -> Result<Option<UpstreamSseChunk>> {
    let Some(data_str) = parse_sse_data_line(line) else {
        return Ok(None);
    };
    if data_str == "[DONE]" {
        return Ok(Some(UpstreamSseChunk::done()));
    }
    let Ok(val) = serde_json::from_str::<serde_json::Value>(data_str) else {
        return Ok(None);
    };
    let event_type = val.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match event_type {
        "heartbeat" | "start" => Ok(None),
        "content" => {
            let content_str = val.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let payload = serde_json::json!({
                "id": chunk_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model_name,
                "choices": [{
                    "index": 0,
                    "delta": {
                        "content": content_str
                    },
                    "finish_reason": null
                }]
            });
            let mut chunk = UpstreamSseChunk::new(payload);
            chunk.has_content = !content_str.is_empty();
            Ok(Some(chunk))
        }
        "thinking" => {
            let thought_str = val.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let payload = serde_json::json!({
                "id": chunk_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model_name,
                "choices": [{
                    "index": 0,
                    "delta": {
                        "reasoning_content": thought_str
                    },
                    "finish_reason": null
                }]
            });
            let mut chunk = UpstreamSseChunk::new(payload);
            chunk.delta_reasoning = Some(thought_str.to_string());
            chunk.has_content = !thought_str.is_empty();
            Ok(Some(chunk))
        }
        _ => Ok(None),
    }
}

/// Parse a Vercel AI SDK Spec v4 / fx.sh SSE line and translate it into an OpenAI-format chunk.
pub fn parse_fx_sse_line(
    line: &str,
    chunk_id: &str,
    created: u64,
    model_name: &str,
) -> Result<Option<UpstreamSseChunk>> {
    let Some(data_str) = parse_sse_data_line(line) else {
        return Ok(None);
    };
    if data_str == "[DONE]" {
        return Ok(Some(UpstreamSseChunk::done()));
    }
    let Ok(val) = serde_json::from_str::<serde_json::Value>(data_str) else {
        return Ok(None);
    };
    let event_type = val.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match event_type {
        "text-delta" => {
            let text = val.get("delta").and_then(|v| v.as_str()).unwrap_or("");
            let payload = serde_json::json!({
                "id": chunk_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model_name,
                "choices": [{
                    "index": 0,
                    "delta": {
                        "content": text
                    },
                    "finish_reason": null
                }]
            });
            let mut chunk = UpstreamSseChunk::new(payload);
            chunk.has_content = !text.is_empty();
            Ok(Some(chunk))
        }
        "reasoning-delta" => {
            let reasoning = val.get("delta").and_then(|v| v.as_str()).unwrap_or("");
            let payload = serde_json::json!({
                "id": chunk_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model_name,
                "choices": [{
                    "index": 0,
                    "delta": {
                        "reasoning_content": reasoning
                    },
                    "finish_reason": null
                }]
            });
            let mut chunk = UpstreamSseChunk::new(payload);
            chunk.delta_reasoning = Some(reasoning.to_string());
            chunk.has_content = !reasoning.is_empty();
            Ok(Some(chunk))
        }
        "tool-call" => {
            let call_id = val.get("toolCallId").and_then(|v| v.as_str()).unwrap_or("");
            let name = val.get("toolName").and_then(|v| v.as_str()).unwrap_or("");
            let input_str = match val.get("input") {
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => String::new(),
            };
            let tool_call_json = serde_json::json!({
                "index": 0,
                "id": call_id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": input_str
                }
            });
            let payload = serde_json::json!({
                "id": chunk_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model_name,
                "choices": [{
                    "index": 0,
                    "delta": {
                        "tool_calls": [&tool_call_json]
                    },
                    "finish_reason": null
                }]
            });
            let mut chunk = UpstreamSseChunk::new(payload);
            chunk.has_content = true;
            chunk.delta_tool_calls.push(tool_call_json);
            Ok(Some(chunk))
        }
        "finish" => {
            let raw_finish = val
                .get("finishReason")
                .and_then(|fr| fr.get("unified").or_else(|| fr.get("raw")))
                .and_then(|v| v.as_str())
                .unwrap_or("stop");

            let finish_reason = match raw_finish {
                "tool-calls" => "tool_calls",
                other => other,
            };

            let prompt_tokens = val
                .get("usage")
                .and_then(|u| u.get("raw"))
                .and_then(|r| r.get("prompt_tokens"))
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);

            let completion_tokens = val
                .get("usage")
                .and_then(|u| u.get("raw"))
                .and_then(|r| r.get("completion_tokens"))
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);

            let total_tokens = match (prompt_tokens, completion_tokens) {
                (Some(p), Some(c)) => Some(p + c),
                _ => None,
            };

            let usage = prompt_tokens.map(|p| OpenAIUsage {
                prompt_tokens: p,
                completion_tokens: completion_tokens.unwrap_or(0),
                total_tokens: total_tokens.unwrap_or(p),
                prompt_tokens_details: None,
            });

            let payload = serde_json::json!({
                "id": chunk_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model_name,
                "choices": [{
                    "index": 0,
                    "delta": {},
                    "finish_reason": finish_reason
                }],
                "usage": usage
            });

            let mut chunk = UpstreamSseChunk::new(payload);
            chunk.done = true;
            chunk.stop_reason = Some(finish_reason.to_string());
            chunk.usage = usage;
            Ok(Some(chunk))
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod atomesus_sse_tests {
    use super::*;

    #[test]
    fn atomesus_parses_content_chunk() {
        let line = r#"data: {"type":"content","content":"Hola"}"#;
        let chunk = parse_atomesus_sse_line(line, "cmpl_1", 100, "atomesus")
            .unwrap()
            .unwrap();
        assert_eq!(
            chunk.payload["choices"][0]["delta"]["content"].as_str(),
            Some("Hola")
        );
        assert!(chunk.has_content);
    }

    #[test]
    fn atomesus_parses_thinking_chunk() {
        let line = r#"data: {"type":"thinking","content":"razonando..."}"#;
        let chunk = parse_atomesus_sse_line(line, "cmpl_1", 100, "atomesus")
            .unwrap()
            .unwrap();
        assert_eq!(
            chunk.payload["choices"][0]["delta"]["reasoning_content"].as_str(),
            Some("razonando...")
        );
        assert_eq!(chunk.delta_reasoning.as_deref(), Some("razonando..."));
    }

    #[test]
    fn atomesus_skips_heartbeat_and_comments() {
        let comment = ": ping";
        assert!(
            parse_atomesus_sse_line(comment, "cmpl_1", 100, "atomesus")
                .unwrap()
                .is_none()
        );
        let heartbeat = r#"data: {"type":"heartbeat"}"#;
        assert!(
            parse_atomesus_sse_line(heartbeat, "cmpl_1", 100, "atomesus")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn fx_sse_parses_text_and_reasoning_and_finish() {
        let line_reasoning =
            r#"data: {"type":"reasoning-delta","id":"reasoning-0","delta":"pensando"}"#;
        let chunk_r = parse_fx_sse_line(line_reasoning, "cmpl_1", 100, "zai/glm-5.2")
            .unwrap()
            .unwrap();
        assert_eq!(
            chunk_r.payload["choices"][0]["delta"]["reasoning_content"].as_str(),
            Some("pensando")
        );
        assert_eq!(chunk_r.delta_reasoning.as_deref(), Some("pensando"));

        let line_text = r#"data: {"type":"text-delta","id":"txt-0","delta":"Hola mundo"}"#;
        let chunk_t = parse_fx_sse_line(line_text, "cmpl_1", 100, "zai/glm-5.2")
            .unwrap()
            .unwrap();
        assert_eq!(
            chunk_t.payload["choices"][0]["delta"]["content"].as_str(),
            Some("Hola mundo")
        );
        assert!(chunk_t.has_content);

        let line_finish = r#"data: {"type":"finish","finishReason":{"unified":"stop"},"usage":{"raw":{"prompt_tokens":10,"completion_tokens":20}}}"#;
        let chunk_f = parse_fx_sse_line(line_finish, "cmpl_1", 100, "zai/glm-5.2")
            .unwrap()
            .unwrap();
        assert!(chunk_f.done);
        assert_eq!(chunk_f.stop_reason.as_deref(), Some("stop"));
        assert_eq!(chunk_f.usage.unwrap().prompt_tokens, 10);

        let line_tool = r#"data: {"type":"tool-call","toolCallId":"call_123","toolName":"read_file","input":{"path":"/etc/hosts"}}"#;
        let chunk_tc = parse_fx_sse_line(line_tool, "cmpl_1", 100, "zai/glm-5.2")
            .unwrap()
            .unwrap();
        assert!(chunk_tc.has_content);
        assert_eq!(chunk_tc.delta_tool_calls.len(), 1);
        assert_eq!(chunk_tc.delta_tool_calls[0]["id"], "call_123");
        assert_eq!(
            chunk_tc.delta_tool_calls[0]["function"]["name"],
            "read_file"
        );
        assert_eq!(
            chunk_tc.delta_tool_calls[0]["function"]["arguments"],
            r#"{"path":"/etc/hosts"}"#
        );
    }

    #[test]
    fn openai_sse_parses_cached_tokens() {
        let line = r#"data: {"choices":[],"usage":{"prompt_tokens":1200,"completion_tokens":300,"total_tokens":1500,"prompt_tokens_details":{"cached_tokens":1000}}}"#;
        let chunk = parse_openai_sse_line(line).unwrap().unwrap();
        let usage = chunk.usage.expect("usage should be parsed");
        assert_eq!(usage.prompt_tokens, 1200);
        assert_eq!(usage.completion_tokens, 300);
        assert_eq!(
            usage
                .prompt_tokens_details
                .as_ref()
                .and_then(|d| d.cached_tokens),
            Some(1000)
        );
    }

    #[test]
    fn parse_sse_data_line_edge_cases() {
        // Empty payloads
        assert_eq!(parse_sse_data_line(""), None);
        assert_eq!(parse_sse_data_line("data: "), None);

        // Multibyte UTF-8 boundaries
        let utf8_line = "data: {\"content\": \"こんにちは\"}";
        assert_eq!(
            parse_sse_data_line(utf8_line),
            Some("{\"content\": \"こんにちは\"}")
        );

        // Malformed JSON (handled gracefully because it just extracts the data string)
        let malformed_line = "data: {\"content\": \"malformed";
        assert_eq!(
            parse_sse_data_line(malformed_line),
            Some("{\"content\": \"malformed")
        );

        // Edge cases with colons
        let multi_colon = "data: :data:hello";
        assert_eq!(parse_sse_data_line(multi_colon), Some(":data:hello"));
    }
}
