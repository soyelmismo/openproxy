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
pub(crate) mod devin;
mod gemini;
pub(crate) mod kiro;
pub mod openai;
mod responses;
mod stream_to_value;

// Re-exported so the streaming state reaches it without importing the rest of
// the Anthropic submodule.
pub(crate) use anthropic::merge_usage;

use crate::translation::OpenAIUsage;
use openproxy_types::ResultExt;
use openproxy_types::error::Result;
use serde_json::Value;
mod chunk;
mod framing;
mod inline_error;

pub use chunk::*;
pub use framing::SseParser;
pub use inline_error::{ParsedInlineError, parse_inline_sse_error};

/// Maximum bytes for one buffered SSE line (4 MiB) — guards the line buffer
/// against unbounded allocation from malformed upstream streams.
pub const MAX_SSE_LINE_BYTES: usize = 4_194_304;
/// Maximum allowed bytes for an SSE event type string (e.g., "message_start",
/// "content_block_delta"). Prevents unbounded memory allocation from malformed
/// upstream event: lines.
pub const MAX_SSE_EVENT_TYPE_BYTES: usize = 1024;

pub fn skip_leading_spaces(bytes: &[u8]) -> &[u8] {
    let pos = bytes.iter().position(|&b| b != b' ').unwrap_or(bytes.len());
    &bytes[pos..]
}


/// Deserialize an SSE payload as JSON, returning
/// `CoreError::Parse("<provider> sse json: ...")`.
///
/// Attempts direct single-pass `serde_json::from_str` first to avoid
/// allocating an intermediate `serde_json::Value` DOM tree on well-formed
/// chunks. Falls back to `Value` deserialization only if `from_str` fails
/// (e.g., when upstream gateways emit literal duplicate object keys that
/// `Value` collapses to last-wins).
#[inline]
pub(crate) fn parse_provider_json<T, S>(payload: S, provider: &str) -> Result<T>
where
    T: for<'de> serde::Deserialize<'de>,
    S: AsRef<str>,
{
    let raw = payload.as_ref();
    if let Ok(parsed) = serde_json::from_str::<T>(raw) {
        return Ok(parsed);
    }
    let value: Value = serde_json::from_str(raw).ctx_parse(format!("{provider} sse json"))?;
    serde_json::from_value(value).ctx_parse(format!("{provider} sse json"))
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


// `MAX_TOOL_*` / `MAX_RESPONSES_*` stay `pub(crate)` in their submodules: no
// in-crate consumer reaches them through `crate::sse::*`, and a `pub(crate) use`
// of an unused item trips `unused_imports` under `-D warnings`.
pub use anthropic::{
    AnthropicToolUseAccumulator, parse_anthropic_sse_stream_line, translate_anthropic_sse_event,
    translate_anthropic_sse_payload,
};
pub use atomesus::parse_atomesus_sse_line;
pub use commandcode::{CommandCodeSseState, parse_commandcode_sse_line};
pub use gemini::{parse_gemini_sse_line, parse_gemini_sse_line_test};
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
    fn append_sse_event_produces_correct_output() {
        let mut buf = bytes::BytesMut::new();
        append_sse_event(&mut buf, "test_event", &serde_json::json!({"foo": "bar"}));
        assert_eq!(
            std::str::from_utf8(&buf).unwrap(),
            "event: test_event\ndata: {\"foo\":\"bar\"}\n\n"
        );
    }

    #[test]
    fn sse_done_constant_value() {
        assert_eq!(
            std::str::from_utf8(crate::pipeline::SSE_DONE_BYTES.as_ref()).unwrap(),
            "data: [DONE]\n\n"
        );
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
