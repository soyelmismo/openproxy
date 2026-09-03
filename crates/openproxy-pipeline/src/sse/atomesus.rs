//! Atomesus SSE parser.

use super::{UpstreamSseChunk, make_text_delta, parse_provider_json, parse_sse_data_or_done};
use openproxy_types::error::Result;
use serde_json::Value;

/// Parse an Atomesus SSE line and translate it into an OpenAI-format chunk.
pub fn parse_atomesus_sse_line(
    line: &str,
    chunk_id: &str,
    created: u64,
    model_name: &str,
) -> Result<Option<UpstreamSseChunk>> {
    let data_str = match parse_sse_data_or_done(line) {
        super::SseDataOrDone::Payload(p) => p,
        super::SseDataOrDone::Done => return Ok(Some(UpstreamSseChunk::done())),
        super::SseDataOrDone::Skip => return Ok(None),
    };
    let val: Value = parse_provider_json(data_str, "atomesus")?;
    let event_type = val.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match event_type {
        "heartbeat" | "start" => Ok(None),
        "content" => {
            let content_str = val.get("content").and_then(|v| v.as_str()).unwrap_or("");
            Ok(Some(make_text_delta(
                chunk_id,
                created,
                model_name,
                content_str,
                false,
            )))
        }
        "thinking" => {
            let thought_str = val.get("content").and_then(|v| v.as_str()).unwrap_or("");
            Ok(Some(make_text_delta(
                chunk_id,
                created,
                model_name,
                thought_str,
                true,
            )))
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
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
    fn atomesus_malformed_json_returns_error() {
        // Regression (P3-1): malformed JSON must surface as
        // CoreError::Parse instead of being silently swallowed as
        // Ok(None), matching every other provider parser.
        let result = parse_atomesus_sse_line("data: {not valid json}", "cmpl_1", 100, "atomesus");
        assert!(result.is_err(), "malformed JSON should produce an error");
    }
}
