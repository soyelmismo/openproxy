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
    /// Usage stats if present in this chunk, usually only the final one.
    pub usage: Option<OpenAIUsage>,
    /// Upstream stop reason (e.g. "end_turn", "max_tokens", "stop_sequence"
    /// for Anthropic; mapped finish_reason for OpenAI). Only set on the
    /// final chunk.
    pub stop_reason: Option<String>,
    /// Extracted per-chunk reasoning delta. Populated by Gemini
    /// `parts[].thought == true` items and by Anthropic
    /// `content_block_delta` with `delta.type == "thinking_delta"`.
    ///
    /// `None` when this chunk carries no reasoning.
    pub delta_reasoning: Option<String>,
    /// Extracted per-chunk tool_calls deltas. Anthropic
    /// `content_block_start` (tool_use block) emits the
    /// `{index, id, type, function:{name, arguments:""}}` record, and
    /// `content_block_delta` with `delta.type == "input_json_delta"` emits
    /// the running `{index, function:{arguments:...}}` record.
    ///
    /// Empty when this chunk carries no tool_calls.
    pub delta_tool_calls: Vec<serde_json::Value>,
    /// Whether this chunk carries real generated tokens (text, reasoning or
    /// tool-call argument fragments) as opposed to metadata-only events.
    ///
    /// The pipeline calls `note_content_chunk` (which resets the chunk-gap
    /// `idle_chunk_ms` timer) only when this is `true`. An Anthropic
    /// `content_block_start` for a `tool_use` block announces the tool id and
    /// name with empty arguments, so it must not reset the timer.
    ///
    /// Defaults to `true`; translators set `false` for metadata-only events.
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

    /// A liveness chunk: the upstream reported progress that carries no
    /// client-visible output.
    pub(crate) fn liveness() -> Self {
        Self {
            raw_payload: None,
            payload: Value::Null,
            done: false,
            usage: None,
            stop_reason: None,
            delta_reasoning: None,
            delta_tool_calls: Vec::new(),
            has_content: false,
        }
    }

    /// True when this chunk reports upstream progress but must not be
    /// forwarded to the client: a `Null` payload on a non-final chunk.
    #[inline]
    pub fn is_liveness(&self) -> bool {
        self.raw_payload.is_none() && self.payload.is_null() && !self.done
    }

    /// Get the forwardable JSON string. Returns the raw payload if
    /// available, otherwise serializes the parsed payload.
    pub fn into_json_string(self) -> String {
        self.raw_payload.unwrap_or_else(|| {
            serde_json::to_string(&self.payload).unwrap_or_else(|_| "{}".to_string())
        })
    }

    /// Pre-formatted `data: {json}\n\n` frame, ready for a direct socket write
    /// with no intermediate `String`.
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

/// `data: <payload>\n\n` as `Bytes`, ready for a socket write. The caller
/// passes the inner JSON with no leading `data: `; allocates exact size (payload.len() + 8).
#[inline]
pub fn build_sse_frame(payload: &str) -> bytes::Bytes {
    let p_bytes = payload.as_bytes();
    let total_len = p_bytes.len() + 8;
    let mut b = bytes::BytesMut::with_capacity(total_len);
    b.extend_from_slice(b"data: ");
    b.extend_from_slice(p_bytes);
    b.extend_from_slice(b"\n\n");
    b.freeze()
}

/// Data payload of an SSE line, trimming line endings and leading whitespace
/// and skipping empty, comment (leading `:`) and non-`data:` lines. `None` for
/// anything but a non-empty payload.
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

/// Format a JSON value as an SSE `data:` line.
pub fn format_sse_line(payload: &serde_json::Value) -> String {
    format!(
        "data: {}\n\n",
        serde_json::to_string(payload).unwrap_or_else(|_| "{}".to_string())
    )
}

/// Append a named SSE event frame (`event: <name>\ndata: <payload>\n\n`) to a byte buffer.
pub fn append_sse_event(out: &mut bytes::BytesMut, event_name: &str, payload: &serde_json::Value) {
    use bytes::BufMut;
    out.extend_from_slice(b"event: ");
    out.extend_from_slice(event_name.as_bytes());
    out.extend_from_slice(b"\ndata: ");
    if serde_json::to_writer((&mut *out).writer(), payload).is_err() {
        out.extend_from_slice(
            br#"{"type":"error","error":{"type":"internal_error","message":"Internal server error"}}"#,
        );
    }
    out.extend_from_slice(b"\n\n");
}

/// Outcome of [`parse_sse_data_or_done`].
#[derive(Debug, PartialEq, Eq)]
pub enum SseDataOrDone<'a> {
    Payload(&'a str),
    Done,
    Skip,
}

/// Classify an SSE line as payload, `[DONE]` sentinel, or skippable.
#[inline]
pub fn parse_sse_data_or_done(line: &str) -> SseDataOrDone<'_> {
    let Some(payload) = parse_sse_data_line(line) else {
        return SseDataOrDone::Skip;
    };
    if payload == "[DONE]" {
        return SseDataOrDone::Done;
    }
    SseDataOrDone::Payload(payload)
}
