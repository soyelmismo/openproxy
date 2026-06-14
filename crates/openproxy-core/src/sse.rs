//! SSE (Server-Sent Events) parsing and translation for streaming responses.
//!
//! Provides parsers for OpenAI and Gemini upstream SSE formats, translating
//! them into OpenAI-format SSE chunks that clients expect.

use crate::error::{CoreError, Result};
use crate::translation::OpenAIUsage;
use serde_json::Value;

/// A single parsed SSE chunk from the upstream, ready to forward.
pub struct UpstreamSseChunk {
    /// The raw JSON payload (already in OpenAI format for OpenAI upstream,
    /// or translated from Gemini format).
    pub payload: Value,
    /// Whether this is the final chunk ([DONE] sentinel).
    pub done: bool,
    /// Usage stats if present in this chunk (usually only the final one).
    pub usage: Option<OpenAIUsage>,
}

// =====================================================================
// OpenAI SSE parsing
// =====================================================================

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
            payload: Value::Null,
            done: true,
            usage: None,
        }));
    }
    let v: Value = serde_json::from_str(payload)
        .map_err(|e| CoreError::Parse(format!("openai sse json: {e}")))?;
    let usage = v.get("usage").and_then(|u| {
        Some(OpenAIUsage {
            prompt_tokens: u.get("prompt_tokens")?.as_u64()? as u32,
            completion_tokens: u.get("completion_tokens")?.as_u64()? as u32,
            total_tokens: u.get("total_tokens")?.as_u64()? as u32,
        })
    });
    Ok(Some(UpstreamSseChunk {
        payload: v,
        done: false,
        usage,
    }))
}

// =====================================================================
// Gemini SSE parsing
// =====================================================================

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
            payload: Value::Null,
            done: true,
            usage: None,
        }));
    }
    let v: Value = serde_json::from_str(payload)
        .map_err(|e| CoreError::Parse(format!("gemini sse json: {e}")))?;

    // Extract text from candidates[0].content.parts[]
    let text = v
        .get("candidates")
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.first())
        .and_then(|c| c.get("content"))
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.as_array())
        .and_then(|parts| {
            let mut s = String::new();
            for part in parts {
                if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                    s.push_str(t);
                }
            }
            if s.is_empty() { None } else { Some(s) }
        })
        .unwrap_or_default();

    // Extract finish_reason
    let finish_reason = v
        .get("candidates")
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.first())
        .and_then(|c| c.get("finishReason"))
        .and_then(|r| r.as_str())
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

    // Extract usage if present (final chunk)
    let usage = v.get("usageMetadata").and_then(|u| {
        Some(OpenAIUsage {
            prompt_tokens: u.get("promptTokenCount")?.as_u64()? as u32,
            completion_tokens: u.get("candidatesTokenCount")?.as_u64()? as u32,
            total_tokens: u.get("totalTokenCount")?.as_u64()? as u32,
        })
    });

    Ok(Some(UpstreamSseChunk { payload: chunk, done: false, usage }))
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
                payload: chunk,
                done: false,
                usage: None,
            }))
        }
        "content_block_delta" => {
            // Extract text from delta
            let text = data.get("delta")
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
                payload: chunk,
                done: false,
                usage: None,
            }))
        }
        "message_delta" => {
            // Extract finish reason and usage
            let stop_reason = data.get("delta")
                .and_then(|d| d.get("stop_reason"))
                .and_then(|r| r.as_str());

            let finish_reason = match stop_reason {
                Some("end_turn") | Some("stop_sequence") => Some("stop".to_string()),
                Some("max_tokens") => Some("length".to_string()),
                _ => None,
            };

            let usage = data.get("usage").map(|u| {
                crate::translation::OpenAIUsage {
                    prompt_tokens: u.get("input_tokens").and_then(|t| t.as_u64()).unwrap_or(0) as u32,
                    completion_tokens: u.get("output_tokens").and_then(|t| t.as_u64()).unwrap_or(0) as u32,
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
                payload: chunk,
                done: true,
                usage,
            }))
        }
        "message_stop" => {
            Ok(Some(UpstreamSseChunk {
                payload: serde_json::json!({}),
                done: true,
                usage: None,
            }))
        }
        _ => Ok(None), // content_block_start, content_block_stop, etc.
    }
}

// =====================================================================
// Formatting
// =====================================================================

/// Format a JSON value as an SSE `data:` line.
pub fn format_sse_line(payload: &serde_json::Value) -> String {
    format!("data: {}\n\n", serde_json::to_string(payload).unwrap_or_default())
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
        assert!(chunk.payload.get("id").is_some());
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
        assert!(parse_openai_sse_line(": this is a comment").unwrap().is_none());
    }

    #[test]
    fn parse_gemini_data_line() {
        let line = r#"data: {"candidates":[{"content":{"parts":[{"text":"Hello"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"totalTokenCount":15}}"#;
        let chunk = parse_gemini_sse_line(line, "test-id", 0, "gemini-pro").unwrap().unwrap();
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
        let chunk = parse_gemini_sse_line("data: [DONE]", "id", 0, "m").unwrap().unwrap();
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
        assert!(parse_openai_sse_line("event: some_event").unwrap().is_none());
        assert!(parse_openai_sse_line("id: 12345").unwrap().is_none());
        assert!(parse_openai_sse_line("retry: 5000").unwrap().is_none());
        assert!(parse_openai_sse_line("random text without prefix").unwrap().is_none());
    }

    #[test]
    fn openai_line_with_event_prefix_ignored() {
        // Standard SSE event: lines should be ignored (not data: lines).
        assert!(parse_openai_sse_line("event: message").unwrap().is_none());
        assert!(parse_openai_sse_line("event: completion").unwrap().is_none());
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
        assert_eq!(chunk.payload["content"].as_str().unwrap().len(), 10_000);
    }

    #[test]
    fn openai_unicode_content() {
        let payload = serde_json::json!({"content": "こんにちは世界 🌍 ñ ü ö ä"});
        let line = format!("data: {}", serde_json::to_string(&payload).unwrap());
        let chunk = parse_openai_sse_line(&line).unwrap().unwrap();
        assert_eq!(chunk.payload["content"].as_str().unwrap(), "こんにちは世界 🌍 ñ ü ö ä");
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
            if let Some(content) = chunk.payload["choices"][0]["delta"]["content"].as_str() {
                contents.push(content.to_string());
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
        assert!(parse_gemini_sse_line("event: some_event", "id", 0, "m").unwrap().is_none());
        assert!(parse_gemini_sse_line("id: 12345", "id", 0, "m").unwrap().is_none());
    }

    #[test]
    fn gemini_line_with_crlf_ending() {
        let line = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hi\"}]}}]}\r\n";
        let chunk = parse_gemini_sse_line(line, "test", 0, "gemini").unwrap().unwrap();
        assert!(!chunk.done);
        assert_eq!(chunk.payload["choices"][0]["delta"]["content"].as_str().unwrap(), "Hi");
    }

    #[test]
    fn gemini_done_with_crlf() {
        let chunk = parse_gemini_sse_line("data: [DONE]\r\n", "id", 0, "m").unwrap().unwrap();
        assert!(chunk.done);
    }

    #[test]
    fn gemini_empty_line() {
        assert!(parse_gemini_sse_line("", "id", 0, "m").unwrap().is_none());
    }

    #[test]
    fn gemini_comment_line() {
        assert!(parse_gemini_sse_line(": this is a comment", "id", 0, "m").unwrap().is_none());
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
        assert!(delta.get("content").is_none() || delta["content"].as_str().unwrap_or("").is_empty());
    }

    #[test]
    fn gemini_multiple_text_parts_concatenated() {
        let line = r#"data: {"candidates":[{"content":{"parts":[{"text":"Hello "},{"text":"World"}]}}]}"#;
        let chunk = parse_gemini_sse_line(line, "id", 0, "m").unwrap().unwrap();
        assert_eq!(chunk.payload["choices"][0]["delta"]["content"].as_str().unwrap(), "Hello World");
    }

    #[test]
    fn gemini_finish_reason_max_tokens_maps_to_length() {
        let line = r#"data: {"candidates":[{"content":{"parts":[]},"finishReason":"MAX_TOKENS"}]}"#;
        let chunk = parse_gemini_sse_line(line, "id", 0, "m").unwrap().unwrap();
        assert_eq!(chunk.payload["choices"][0]["finish_reason"].as_str().unwrap(), "length");
    }

    #[test]
    fn gemini_finish_reason_safety_maps_to_content_filter() {
        let line = r#"data: {"candidates":[{"content":{"parts":[]},"finishReason":"SAFETY"}]}"#;
        let chunk = parse_gemini_sse_line(line, "id", 0, "m").unwrap().unwrap();
        assert_eq!(chunk.payload["choices"][0]["finish_reason"].as_str().unwrap(), "content_filter");
    }

    #[test]
    fn gemini_long_line() {
        let long_text = "y".repeat(10_000);
        let payload = serde_json::json!({"candidates":[{"content":{"parts":[{"text": long_text}]}}]});
        let line = format!("data: {}", serde_json::to_string(&payload).unwrap());
        let chunk = parse_gemini_sse_line(&line, "id", 0, "gemini").unwrap().unwrap();
        assert_eq!(
            chunk.payload["choices"][0]["delta"]["content"].as_str().unwrap().len(),
            10_000
        );
    }

    #[test]
    fn gemini_unicode_content() {
        let payload = serde_json::json!({"candidates":[{"content":{"parts":[{"text":"日本語テスト 🎉"}]}}]});
        let line = format!("data: {}", serde_json::to_string(&payload).unwrap());
        let chunk = parse_gemini_sse_line(&line, "id", 0, "gemini").unwrap().unwrap();
        assert_eq!(chunk.payload["choices"][0]["delta"]["content"].as_str().unwrap(), "日本語テスト 🎉");
    }

    #[test]
    fn gemini_chunk_metadata_fields() {
        let payload = serde_json::json!({"candidates":[{"content":{"parts":[{"text":"hi"}]}}]});
        let line = format!("data: {}", serde_json::to_string(&payload).unwrap());
        let chunk = parse_gemini_sse_line(&line, "chunk-42", 1234567890, "gemini-pro").unwrap().unwrap();
        assert_eq!(chunk.payload["id"].as_str().unwrap(), "chunk-42");
        assert_eq!(chunk.payload["created"].as_u64().unwrap(), 1234567890);
        assert_eq!(chunk.payload["model"].as_str().unwrap(), "gemini-pro");
        assert_eq!(chunk.payload["object"].as_str().unwrap(), "chat.completion.chunk");
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
        assert_eq!(chunk.payload["choices"][0]["delta"]["content"].as_str().unwrap(), "ok");
    }

    #[test]
    fn gemini_only_whitespace_line() {
        assert!(parse_gemini_sse_line("   \t  ", "id", 0, "m").unwrap().is_none());
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
        let content = chunk.payload["choices"][0]["delta"]["content"].as_str().unwrap_or("");
        assert!(content.is_empty());
    }

    #[test]
    fn gemini_parts_with_non_text_fields_ignored() {
        // Some parts may have "thought: true" or other keys — only "text" parts matter.
        let line = r#"data: {"candidates":[{"content":{"parts":[{"thought":true},{"text":"real answer"}]}}]}"#;
        let chunk = parse_gemini_sse_line(line, "id", 0, "m").unwrap().unwrap();
        assert_eq!(chunk.payload["choices"][0]["delta"]["content"].as_str().unwrap(), "real answer");
    }

    // ---- Anthropic SSE tests ----

    #[test]
    fn anthropic_event_line_sets_current_event() {
        let mut current_event = None;
        let result = parse_anthropic_sse_stream_line("event: message_start", &mut current_event).unwrap();
        assert!(result.is_none());
        assert_eq!(current_event.as_deref(), Some("message_start"));
    }

    #[test]
    fn anthropic_data_line_returns_payload_with_event() {
        let mut current_event = Some("content_block_delta".to_string());
        let result = parse_anthropic_sse_stream_line(
            r#"data: {"delta":{"text":"Hello"}}"#,
            &mut current_event,
        ).unwrap().unwrap();
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
        assert!(parse_anthropic_sse_stream_line("id: 123", &mut current_event).unwrap().is_none());
        assert!(parse_anthropic_sse_stream_line("retry: 5000", &mut current_event).unwrap().is_none());
        assert!(parse_anthropic_sse_stream_line(": comment", &mut current_event).unwrap().is_none());
    }

    #[test]
    fn anthropic_translate_message_start() {
        let payload = r#"message_start
{"type":"message","role":"assistant","content":[],"model":"claude-3","stop_reason":null,"usage":{"input_tokens":10,"output_tokens":0}}"#;
        let chunk = translate_anthropic_sse_payload(payload, "chunk-1", 1000, "claude-3").unwrap().unwrap();
        assert!(!chunk.done);
        assert_eq!(chunk.payload["choices"][0]["delta"]["role"].as_str().unwrap(), "assistant");
        assert_eq!(chunk.payload["id"].as_str().unwrap(), "chunk-1");
    }

    #[test]
    fn anthropic_translate_content_block_delta() {
        let payload = r#"content_block_delta
{"delta":{"type":"content_block_delta","text":"Hello"}}"#;
        let chunk = translate_anthropic_sse_payload(payload, "chunk-1", 1000, "claude-3").unwrap().unwrap();
        assert!(!chunk.done);
        assert_eq!(chunk.payload["choices"][0]["delta"]["content"].as_str().unwrap(), "Hello");
    }

    #[test]
    fn anthropic_translate_message_delta_with_stop() {
        let payload = r#"message_delta
{"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":50}}"#;
        let chunk = translate_anthropic_sse_payload(payload, "chunk-1", 1000, "claude-3").unwrap().unwrap();
        assert!(chunk.done);
        assert_eq!(chunk.payload["choices"][0]["finish_reason"].as_str().unwrap(), "stop");
        assert!(chunk.usage.is_some());
    }

    #[test]
    fn anthropic_translate_message_delta_max_tokens() {
        let payload = r#"message_delta
{"delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":100}}"#;
        let chunk = translate_anthropic_sse_payload(payload, "chunk-1", 1000, "claude-3").unwrap().unwrap();
        assert!(chunk.done);
        assert_eq!(chunk.payload["choices"][0]["finish_reason"].as_str().unwrap(), "length");
    }

    #[test]
    fn anthropic_translate_message_stop() {
        let payload = "message_stop\n{}";
        let chunk = translate_anthropic_sse_payload(payload, "chunk-1", 1000, "claude-3").unwrap().unwrap();
        assert!(chunk.done);
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
            if let Some(payload) = parse_anthropic_sse_stream_line(line, &mut current_event).unwrap() {
                if let Some(chunk) = translate_anthropic_sse_payload(&payload, "test", 0, "claude-3").unwrap() {
                    chunks.push(chunk);
                }
            }
        }

        // Should have: message_start, 2 content_block_delta, message_delta, message_stop
        assert_eq!(chunks.len(), 5);
        // First chunk: role assignment
        assert_eq!(chunks[0].payload["choices"][0]["delta"]["role"].as_str().unwrap(), "assistant");
        // Second chunk: "Hi"
        assert_eq!(chunks[1].payload["choices"][0]["delta"]["content"].as_str().unwrap(), "Hi");
        // Third chunk: " there"
        assert_eq!(chunks[2].payload["choices"][0]["delta"]["content"].as_str().unwrap(), " there");
        // Fourth chunk: finish_reason
        assert_eq!(chunks[3].payload["choices"][0]["finish_reason"].as_str().unwrap(), "stop");
        assert!(chunks[3].done);
        // Fifth chunk: message_stop (done=true)
        assert!(chunks[4].done);
    }
}
