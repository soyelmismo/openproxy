//! Gemini SSE parser.

use super::{UpstreamSseChunk, parse_provider_json, parse_sse_data_or_done};
use crate::translation::OpenAIUsage;
use openproxy_types::error::Result;
use serde_json::{Value, json};

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
    // All three token counts must be present (matches legacy semantics:
    // partial usage metadata is dropped to avoid emitting an
    // `OpenAIUsage` with `0`s, which would corrupt downstream billing).
    Some(super::build_openai_usage(
        Some(u.prompt_tokens?),
        Some(u.completion_tokens?),
        Some(u.total_tokens?),
        None,
    ))
}

pub fn parse_gemini_sse_line(
    line: &str,
    chunk_id: &str,
    created: u64,
    model: &str,
) -> Result<Option<UpstreamSseChunk>> {
    let payload = match parse_sse_data_or_done(line) {
        super::SseDataOrDone::Payload(p) => p,
        super::SseDataOrDone::Done => return Ok(Some(UpstreamSseChunk::done())),
        super::SseDataOrDone::Skip => return Ok(None),
    };

    let probe: GeminiSseProbe = parse_provider_json(payload, "gemini")?;

    let candidates = extract_gemini_candidates(&probe);
    let usage_metadata = extract_gemini_usage_metadata(&probe);
    let (text, delta_reasoning) = extract_gemini_content_and_reasoning(candidates);

    let finish_reason = candidates
        .first()
        .and_then(|c| c.finish_reason.as_deref())
        .map(map_gemini_finish_reason);

    let delta = if text.is_empty() {
        json!({})
    } else {
        json!({"content": text})
    };
    let finish_val = finish_reason.as_ref().map_or(Value::Null, |r| json!(r));

    let chunk = json!({
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

#[cfg(test)]
mod tests {
    use super::*;
    use openproxy_types::error::CoreError;

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
}
