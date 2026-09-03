//! SSE (Server-Sent Events) parsing and translation for streaming responses.
//!
//! Common types ([`UpstreamSseChunk`]), the byte-level line buffer
//! ([`SseParser`]) and shared helpers live here. Provider-specific parsers
//! (OpenAI, Anthropic, Gemini, Responses API, Atomesus, fx.sh) live in their
//! own submodules and are re-exported below, translating upstream SSE formats
//! into OpenAI-format SSE chunks that clients expect.

mod anthropic;
mod atomesus;
mod fx;
mod gemini;
mod openai;
mod responses;

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

/// Format a JSON value as an SSE `data:` line.
pub fn format_sse_line(payload: &serde_json::Value) -> String {
    format!(
        "data: {}\n\n",
        serde_json::to_string(payload).unwrap_or_else(|_| "{}".to_string())
    )
}

/// The [DONE] sentinel.
pub const SSE_DONE: &str = "data: [DONE]\n\n";

pub const MAX_SSE_LINE_BYTES: usize = 4_194_304; // 4 MiB
/// Maximum allowed bytes for an SSE event type string (e.g., "message_start", "content_block_delta").
/// Prevents unbounded memory allocation from malformed upstream event: lines.
pub const MAX_SSE_EVENT_TYPE_BYTES: usize = 1024;

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

// `MAX_TOOL_*` / `MAX_RESPONSES_*` are `pub(crate)` in their submodules
// (spec §2.3: crate-visible, hidden from the external API). They are NOT
// re-exported here because no in-crate consumer references them via
// `crate::sse::MAX_TOOL_*` (verified in `streaming_state.rs` and friends:
// all SSE-bound constants are accessed from inside the module that owns
// them, or from `mod.rs` directly). Re-exporting `pub(crate)` items via
// `pub(crate) use` is legal but triggers `unused_imports` under
// `-D warnings`; this is the root-cause fix that keeps clippy clean.
pub use anthropic::{
    parse_anthropic_sse_stream_line, translate_anthropic_sse_event, translate_anthropic_sse_payload,
    AnthropicToolUseAccumulator,
};
pub use atomesus::parse_atomesus_sse_line;
pub use fx::parse_fx_sse_line;
pub use gemini::parse_gemini_sse_line;
pub use openai::parse_openai_sse_line;
pub use responses::{parse_responses_sse_stream_line, ResponsesSseState};

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
    fn format_sse_line_produces_correct_output() {
        let v = serde_json::json!({"test": true});
        let line = format_sse_line(&v);
        assert_eq!(line, "data: {\"test\":true}\n\n");
    }

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
