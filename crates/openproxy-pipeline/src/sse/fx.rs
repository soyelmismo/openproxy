//! Vercel AI SDK Spec v4 / fx.sh SSE parser.

use super::{
    UpstreamSseChunk, make_text_delta, parse_provider_json, parse_sse_data_or_done,
};
use crate::translation::OpenAIUsage;
use openproxy_types::error::Result;
use serde_json::Value;

/// Parse a Vercel AI SDK Spec v4 / fx.sh SSE line and translate it into an OpenAI-format chunk.
pub fn parse_fx_sse_line(
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
    let val: Value = parse_provider_json(data_str, "fx")?;
    let event_type = val.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match event_type {
        "text-delta" => {
            let text = val.get("delta").and_then(|v| v.as_str()).unwrap_or("");
            Ok(Some(make_text_delta(chunk_id, created, model_name, text, false)))
        }
        "reasoning-delta" => {
            let reasoning = val.get("delta").and_then(|v| v.as_str()).unwrap_or("");
            Ok(Some(make_text_delta(chunk_id, created, model_name, reasoning, true)))
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
mod tests {
    use super::*;

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
    fn fx_malformed_json_returns_error() {
        // Regression (P3-1): malformed JSON must surface as
        // CoreError::Parse instead of being silently swallowed as
        // Ok(None), matching every other provider parser.
        let result = parse_fx_sse_line("data: {not valid json}", "cmpl_1", 100, "fx");
        assert!(result.is_err(), "malformed JSON should produce an error");
    }
}
