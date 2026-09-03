//! Atomesus SSE parser.

use super::{parse_sse_data_line, UpstreamSseChunk};
use openproxy_types::error::Result;
use serde_json::Value;

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
    let Ok(val) = serde_json::from_str::<Value>(data_str) else {
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
}