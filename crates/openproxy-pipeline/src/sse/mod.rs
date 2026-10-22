//! SSE (Server-Sent Events) parsing and translation for streaming responses.
//!
//! Common types ([`UpstreamSseChunk`]), the byte-level line buffer
//! ([`SseParser`]) and shared helpers live here. Provider-specific parsers
//! (OpenAI, Anthropic, Gemini, Responses API, Atomesus) live in their
//! own submodules and are re-exported below, translating upstream SSE formats
//! into OpenAI-format SSE chunks that clients expect.

mod anthropic;
mod atomesus;
pub mod commandcode;
mod gemini;
mod openai;
mod responses;
mod stream_to_value;

// Re-exported so the streaming state reaches it without importing the rest of
// the Anthropic submodule.
pub(crate) use anthropic::merge_usage;

use crate::translation::OpenAIUsage;
use openproxy_types::ResultExt;
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
    ///
    /// Today this is the Anthropic `thinking_delta` with an empty
    /// `thinking` string plus `estimated_tokens` — server-side reasoning the
    /// vendor withholds from the stream. Nothing is forwarded to the client;
    /// the pipeline uses it to tell the body stream that bytes are still
    /// flowing (see [`Self::is_liveness`] and
    /// `UpstreamBodyStream::note_progress_chunk`), so a request is not
    /// killed by the TTFT gate while the model is still thinking.
    ///
    /// `payload` is `Null` and `done` is `false`, which is exactly what
    /// [`Self::is_liveness`] tests for.
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
    ///
    /// Every translator builds a real JSON payload for output it forwards,
    /// so the shape is unambiguous today, and it keeps the struct layout
    /// frozen for the ~110 literal constructions across the SSE
    /// translators.
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
/// passes the inner JSON with no leading `data: `; `+ 16` covers `"data: "`
/// (6) plus `"\n\n"` (2) plus BytesMut's allocation slack.
pub fn build_sse_frame(payload: &str) -> bytes::Bytes {
    let mut b = bytes::BytesMut::with_capacity(payload.len() + 16);
    b.extend_from_slice(b"data: ");
    b.extend_from_slice(payload.as_bytes());
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

/// Outcome of [`parse_sse_data_or_done`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SseDataOrDone<'a> {
    Payload(&'a str),
    Done,
    Skip,
}

/// Classify an SSE line as payload, `[DONE]` sentinel, or skippable.
#[inline]
pub(crate) fn parse_sse_data_or_done(line: &str) -> SseDataOrDone<'_> {
    let Some(payload) = parse_sse_data_line(line) else {
        return SseDataOrDone::Skip;
    };
    if payload == "[DONE]" {
        return SseDataOrDone::Done;
    }
    SseDataOrDone::Payload(payload)
}

/// Deserialize an SSE payload as JSON, returning
/// `CoreError::Parse("<provider> sse json: ...")`.
#[inline]
pub(crate) fn parse_provider_json<T, S>(payload: S, provider: &str) -> Result<T>
where
    T: for<'de> serde::Deserialize<'de>,
    S: AsRef<str>,
{
    serde_json::from_str(payload.as_ref())
        .ctx_parse(format!("{provider} sse json"))
}

/// Build `OpenAIUsage` from `Option<u64>` token counts, applying the
/// standard `unwrap_or(0).try_into().unwrap_or(u32::MAX)` conversion.
#[inline]
pub(crate) fn build_openai_usage(
    prompt: Option<u64>,
    completion: Option<u64>,
    total: Option<u64>,
    details: Option<openproxy_types::message::PromptTokensDetails>,
) -> OpenAIUsage {
    OpenAIUsage {
        prompt_tokens: prompt.unwrap_or(0).try_into().unwrap_or(u32::MAX),
        completion_tokens: completion.unwrap_or(0).try_into().unwrap_or(u32::MAX),
        total_tokens: total.unwrap_or(0).try_into().unwrap_or(u32::MAX),
        prompt_tokens_details: details,
    }
}

/// OpenAI-shape `chat.completion.chunk` carrying a single text or
/// reasoning delta. `is_reasoning=true` routes the text into
/// `reasoning_content` and populates `delta_reasoning`.
#[inline]
pub(crate) fn make_text_delta(
    chunk_id: &str,
    created: u64,
    model: &str,
    text: &str,
    is_reasoning: bool,
) -> UpstreamSseChunk {
    let delta = if is_reasoning {
        serde_json::json!({ "content": "", "reasoning_content": text })
    } else {
        serde_json::json!({ "content": text })
    };
    let payload = serde_json::json!({
        "id": chunk_id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": null,
        }]
    });
    UpstreamSseChunk {
        raw_payload: None,
        payload,
        done: false,
        usage: None,
        stop_reason: None,
        delta_reasoning: if is_reasoning {
            Some(text.to_string())
        } else {
            None
        },
        delta_tool_calls: Vec::new(),
        has_content: !text.is_empty(),
    }
}

/// OpenAI-shape chunk for a tool-call start event (id, name, empty arguments).
/// `has_content=false` so the idle-chunk timer is not reset before argument
/// tokens arrive.
#[inline]
pub(crate) fn make_tool_call_start(
    chunk_id: &str,
    created: u64,
    model: &str,
    index: u32,
    id: &str,
    name: &str,
) -> UpstreamSseChunk {
    let tool_call = serde_json::json!({
        "index": index,
        "id": id,
        "type": "function",
        "function": { "name": name, "arguments": "" }
    });
    let payload = serde_json::json!({
        "id": chunk_id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": { "tool_calls": [&tool_call] },
            "finish_reason": null,
        }]
    });
    UpstreamSseChunk {
        raw_payload: None,
        payload,
        done: false,
        usage: None,
        stop_reason: None,
        delta_reasoning: None,
        delta_tool_calls: vec![tool_call],
        has_content: false,
    }
}

/// OpenAI-shape chunk for a tool-call argument-delta event.
#[inline]
pub(crate) fn make_tool_call_delta(
    chunk_id: &str,
    created: u64,
    model: &str,
    index: u32,
    arguments: &str,
) -> UpstreamSseChunk {
    let tool_call = serde_json::json!({
        "index": index,
        "function": { "arguments": arguments }
    });
    let payload = serde_json::json!({
        "id": chunk_id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": { "tool_calls": [&tool_call] },
            "finish_reason": null,
        }]
    });
    UpstreamSseChunk {
        raw_payload: None,
        payload,
        done: false,
        usage: None,
        stop_reason: None,
        delta_reasoning: None,
        delta_tool_calls: vec![tool_call],
        has_content: !arguments.is_empty(),
    }
}

fn check_finish_reason_non_null(payload: &str) -> bool {
    let Some(idx) = payload.find("\"finish_reason\"") else {
        return false;
    };
    let rest = &payload[idx + 15..];
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= bytes.len() || bytes[i] != b':' {
        return false;
    }
    i += 1;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= bytes.len() {
        return false;
    }
    if bytes[i..].starts_with(b"null") {
        let after = i + 4;
        if after == bytes.len()
            || matches!(
                bytes[after],
                b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n'
            )
        {
            return false;
        }
    }
    true
}

fn check_usage_present(payload: &str) -> bool {
    let Some(idx) = payload.find("\"usage\"") else {
        return false;
    };
    let rest = &payload[idx + 7..];
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= bytes.len() || bytes[i] != b':' {
        return false;
    }
    i += 1;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i < bytes.len() && bytes[i] == b'{'
}

pub fn sse_payload_needs_parse(payload: &str) -> bool {
    check_usage_present(payload) || check_finish_reason_non_null(payload)
}

#[derive(serde::Deserialize)]
struct InlineErrorProbe<'a> {
    #[serde(borrow)]
    choices: Option<Vec<&'a serde_json::value::RawValue>>,
    #[serde(borrow)]
    error: Option<InlineErrorPayload<'a>>,
    #[serde(borrow)]
    provider: Option<&'a str>,
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum InlineErrorPayload<'a> {
    Obj(InlineErrorObjProbe<'a>),
    #[serde(borrow)]
    Str(&'a str),
}

#[derive(serde::Deserialize)]
struct InlineErrorObjProbe<'a> {
    #[serde(borrow)]
    code: Option<InlineErrorCodeProbe<'a>>,
    #[serde(borrow)]
    message: Option<&'a str>,
    #[serde(borrow)]
    r#type: Option<&'a str>,
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum InlineErrorCodeProbe<'a> {
    Num(u64),
    #[serde(borrow)]
    Str(&'a str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedInlineError<'a> {
    pub status_code: u16,
    pub message: &'a str,
    pub provider: Option<&'a str>,
}

pub fn parse_inline_sse_error<'a>(json_payload: &'a str) -> Option<ParsedInlineError<'a>> {
    if !json_payload.contains("\"error\"")
        || (json_payload.contains("\"choices\":") && !json_payload.contains("\"choices\":[]"))
    {
        return None;
    }

    let probe = serde_json::from_str::<InlineErrorProbe<'a>>(json_payload).ok()?;
    if !probe.choices.as_ref().is_none_or(std::vec::Vec::is_empty) {
        return None;
    }

    let (status_code, message) = match probe.error? {
        InlineErrorPayload::Str(msg) => (502, msg),
        InlineErrorPayload::Obj(obj) => {
            let msg = obj
                .message
                .unwrap_or("unknown upstream error in SSE stream");
            let mut resolved = match obj.code {
                Some(InlineErrorCodeProbe::Num(n)) if (400..=599).contains(&n) => n as u16,
                Some(InlineErrorCodeProbe::Str(s)) => {
                    if let Ok(n) = s.parse::<u16>()
                        && (400..=599).contains(&n)
                    {
                        n
                    } else {
                        const CODE_PATTERNS: &[(&[&str], u16)] = &[
                            (&["rate_limit", "quota", "resource_exhausted"], 429),
                            (
                                &[
                                    "content_filter",
                                    "safety",
                                    "policy",
                                    "blocked",
                                    "invalid",
                                    "bad_request",
                                    "malformed",
                                ],
                                400,
                            ),
                            (&["auth", "unauthorized", "permission", "forbidden"], 403),
                        ];
                        CODE_PATTERNS
                            .iter()
                            .find_map(|(kws, code)| {
                                kws.iter().any(|k| s.contains(k)).then_some(*code)
                            })
                            .unwrap_or(502)
                    }
                }
                _ => 502,
            };

            if resolved == 502
                && let Some(t) = obj.r#type
            {
                const TYPE_PATTERNS: &[(&[&str], u16)] = &[
                    (&["rate_limit"], 429),
                    (&["content_filter", "safety", "invalid_request"], 400),
                    (&["auth"], 401),
                ];
                if let Some(code) = TYPE_PATTERNS
                    .iter()
                    .find_map(|(kws, code)| kws.iter().any(|k| t.contains(k)).then_some(*code))
                {
                    resolved = code;
                }
            }

            (resolved, msg)
        }
    };

    Some(ParsedInlineError {
        status_code,
        message,
        provider: probe.provider,
    })
}

// `MAX_TOOL_*` / `MAX_RESPONSES_*` stay `pub(crate)` in their submodules: no
// in-crate consumer reaches them through `crate::sse::*`, and a `pub(crate) use`
// of an unused item trips `unused_imports` under `-D warnings`.
pub use anthropic::{
    AnthropicToolUseAccumulator, parse_anthropic_sse_stream_line, translate_anthropic_sse_event,
    translate_anthropic_sse_payload,
};
pub use atomesus::parse_atomesus_sse_line;
pub use commandcode::{CommandCodeSseState, parse_commandcode_sse_line};
pub use gemini::parse_gemini_sse_line;
pub use openai::parse_openai_sse_line;
pub use responses::{ResponsesSseState, parse_responses_sse_stream_line};
pub use stream_to_value::parse_sse_stream_to_openai_response;

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
        assert_eq!(parse_sse_data_line(""), None);
        assert_eq!(parse_sse_data_line("data: "), None);

        let utf8_line = "data: {\"content\": \"こんにちは\"}";
        assert_eq!(
            parse_sse_data_line(utf8_line),
            Some("{\"content\": \"こんにちは\"}")
        );

        let malformed_line = "data: {\"content\": \"malformed";
        assert_eq!(
            parse_sse_data_line(malformed_line),
            Some("{\"content\": \"malformed")
        );

        let multi_colon = "data: :data:hello";
        assert_eq!(parse_sse_data_line(multi_colon), Some(":data:hello"));
    }

    #[test]
    fn test_parse_inline_sse_error_string_code() {
        let payload = r#"{"error": {"message": "I'm sorry, but I can't share details of my architecture or training process. Would you like to learn about how language models work in general instead?", "type": "content_filter_error", "param": null, "code": "content_filter"}}"#;
        let err = parse_inline_sse_error(payload).expect("should parse content_filter error");
        assert_eq!(err.status_code, 400);
        assert!(err.message.starts_with("I'm sorry"));
    }

    #[test]
    fn test_parse_inline_sse_error_numeric_code() {
        let payload = r#"{"error": {"message": "Rate limit exceeded", "code": 429}}"#;
        let err = parse_inline_sse_error(payload).expect("should parse rate limit");
        assert_eq!(err.status_code, 429);
        assert_eq!(err.message, "Rate limit exceeded");
    }

    #[test]
    fn test_parse_inline_sse_error_string_payload() {
        let payload = r#"{"error": "Internal upstream failure"}"#;
        let err = parse_inline_sse_error(payload).expect("should parse string error");
        assert_eq!(err.status_code, 502);
        assert_eq!(err.message, "Internal upstream failure");
    }

    #[test]
    fn test_parse_inline_sse_error_ignores_choices() {
        let payload = r#"{"choices": [{"delta": {"content": "Hello"}}], "error": null}"#;
        assert!(parse_inline_sse_error(payload).is_none());
    }

    #[test]
    fn test_sse_payload_needs_parse_whitespace_tolerance() {
        assert!(!sse_payload_needs_parse(
            r#"{"choices":[{"delta":{"content":"a"},"finish_reason":null}]}"#
        ));
        assert!(!sse_payload_needs_parse(
            r#"{"choices":[{"delta":{"content":"a"},"finish_reason": null}]}"#
        ));
        assert!(!sse_payload_needs_parse(
            r#"{"choices":[{"delta":{"content":"a"},"finish_reason" :  null}]}"#
        ));

        assert!(sse_payload_needs_parse(
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#
        ));
        assert!(sse_payload_needs_parse(
            r#"{"choices":[{"delta":{},"finish_reason": "stop"}]}"#
        ));
        assert!(sse_payload_needs_parse(
            r#"{"choices":[{"delta":{},"finish_reason" : "length"}]}"#
        ));

        assert!(sse_payload_needs_parse(
            r#"{"choices":[],"usage":{"prompt_tokens":10}}"#
        ));
        assert!(sse_payload_needs_parse(
            r#"{"choices":[],"usage": {"prompt_tokens":10}}"#
        ));
        assert!(sse_payload_needs_parse(
            r#"{"choices":[],"usage" :  {"prompt_tokens":10}}"#
        ));
    }
}
