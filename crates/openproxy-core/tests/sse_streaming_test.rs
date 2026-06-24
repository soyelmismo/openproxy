//! Integration tests for SSE streaming pipeline.
//!
//! These tests exercise the SSE parsing functions in a realistic
//! streaming scenario — simulating how the pipeline reads lines
//! from upstream and translates them for the client.

use openproxy_core::sse::{
    SSE_DONE, UpstreamSseChunk, format_sse_line, parse_gemini_sse_line, parse_openai_sse_line,
};
use openproxy_core::translation::OpenAIUsage;

/// Helper: get the payload as a `Value`, whether it came via `raw_payload`
/// (OpenAI pass-through) or `payload` (translated formats).
fn payload_value(chunk: &UpstreamSseChunk) -> serde_json::Value {
    if let Some(ref raw) = chunk.raw_payload {
        serde_json::from_str(raw).unwrap()
    } else {
        chunk.payload.clone()
    }
}

// =====================================================================
// OpenAI streaming simulation
// =====================================================================

/// Simulate a full OpenAI streaming response end-to-end.
/// Parses a series of SSE lines as a streaming client would receive them.
#[test]
fn openai_streaming_full_response_simulation() {
    let sse_lines = vec![
        r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1234567890,"model":"gpt-4","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}"#,
        r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1234567890,"model":"gpt-4","choices":[{"index":0,"delta":{"content":"The "},"finish_reason":null}]}"#,
        r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1234567890,"model":"gpt-4","choices":[{"index":0,"delta":{"content":"answer "},"finish_reason":null}]}"#,
        r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1234567890,"model":"gpt-4","choices":[{"index":0,"delta":{"content":"is 42."},"finish_reason":null}]}"#,
        r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1234567890,"model":"gpt-4","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#,
        "data: [DONE]",
    ];

    let mut full_content = String::new();
    let mut final_usage: Option<OpenAIUsage> = None;
    let mut done = false;

    for line in &sse_lines {
        let chunk = parse_openai_sse_line(line).unwrap();
        match chunk {
            None => continue, // empty/comment line
            Some(c) => {
                if c.done {
                    done = true;
                    break;
                }
                if let Some(content) = payload_value(&c)["choices"][0]["delta"]["content"].as_str()
                {
                    full_content.push_str(content);
                }
                if c.usage.is_some() {
                    final_usage = c.usage;
                }
            }
        }
    }

    assert!(done, "stream should have ended with [DONE]");
    assert_eq!(full_content, "The answer is 42.");
    let usage = final_usage.expect("usage should be present in final chunk");
    assert_eq!(usage.prompt_tokens, 10);
    assert_eq!(usage.completion_tokens, 5);
    assert_eq!(usage.total_tokens, 15);
}

/// Simulate an OpenAI streaming response with interleaved empty lines
/// and comments (common in real HTTP SSE streams).
#[test]
fn openai_streaming_with_interleaved_empty_and_comments() {
    let sse_lines = vec![
        "",
        ": this is a comment",
        "",
        r#"data: {"id":"1","object":"chat.completion.chunk","created":0,"model":"gpt-4","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}"#,
        "",
        ": keep-alive",
        "",
        r#"data: {"id":"1","object":"chat.completion.chunk","created":0,"model":"gpt-4","choices":[{"index":0,"delta":{"content":" world"},"finish_reason":null}]}"#,
        "",
        "data: [DONE]",
    ];

    let mut content = String::new();
    for line in &sse_lines {
        let chunk = parse_openai_sse_line(line).unwrap();
        match chunk {
            None => continue,
            Some(c) => {
                if c.done {
                    break;
                }
                if let Some(text) = payload_value(&c)["choices"][0]["delta"]["content"].as_str() {
                    content.push_str(text);
                }
            }
        }
    }
    assert_eq!(content, "Hello world");
}

// =====================================================================
// Gemini streaming simulation
// =====================================================================

/// Simulate a full Gemini streaming response translated to OpenAI format.
#[test]
fn gemini_streaming_full_response_simulation() {
    let sse_lines = vec![
        r#"data: {"candidates":[{"content":{"parts":[{"text":"Hello"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"totalTokenCount":15}}"#,
        "data: [DONE]",
    ];

    let mut content = String::new();
    let mut final_usage: Option<OpenAIUsage> = None;

    for line in &sse_lines {
        let chunk = parse_gemini_sse_line(line, "chunk-1", 1234567890, "gemini-pro").unwrap();
        match chunk {
            None => continue,
            Some(c) => {
                if c.done {
                    break;
                }
                if let Some(text) = c.payload["choices"][0]["delta"]["content"].as_str() {
                    content.push_str(text);
                }
                if c.usage.is_some() {
                    final_usage = c.usage;
                }
            }
        }
    }

    assert_eq!(content, "Hello");
    let usage = final_usage.expect("usage should be present");
    assert_eq!(usage.prompt_tokens, 10);
    assert_eq!(usage.completion_tokens, 5);
}

/// Simulate a Gemini streaming response with multiple chunks.
#[test]
fn gemini_streaming_multiple_chunks() {
    let sse_lines = vec![
        r#"data: {"candidates":[{"content":{"parts":[{"text":"Chunk 1 "}]}}]}"#,
        r#"data: {"candidates":[{"content":{"parts":[{"text":"Chunk 2 "}]}}]}"#,
        r#"data: {"candidates":[{"content":{"parts":[{"text":"Chunk 3"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":3,"totalTokenCount":4}}"#,
        "data: [DONE]",
    ];

    let mut content = String::new();
    let mut usage: Option<OpenAIUsage> = None;

    for line in &sse_lines {
        let chunk = parse_gemini_sse_line(line, "c", 0, "m").unwrap();
        match chunk {
            None => continue,
            Some(c) => {
                if c.done {
                    break;
                }
                if let Some(text) = c.payload["choices"][0]["delta"]["content"].as_str() {
                    content.push_str(text);
                }
                if c.usage.is_some() {
                    usage = c.usage;
                }
            }
        }
    }

    assert_eq!(content, "Chunk 1 Chunk 2 Chunk 3");
    assert!(usage.is_some());
}

/// Gemini stream with CRLF line endings (real HTTP transport).
#[test]
fn gemini_streaming_with_crlf() {
    let raw = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ok\"}]}}]}\r\ndata: [DONE]\r\n";
    let lines: Vec<&str> = raw.lines().collect();
    let mut content = String::new();

    for line in &lines {
        let chunk = parse_gemini_sse_line(line, "c", 0, "m").unwrap();
        match chunk {
            None => continue,
            Some(c) => {
                if c.done {
                    break;
                }
                if let Some(text) = c.payload["choices"][0]["delta"]["content"].as_str() {
                    content.push_str(text);
                }
            }
        }
    }
    assert_eq!(content, "ok");
}

// =====================================================================
// format_sse_line round-trip
// =====================================================================

/// Verify that format_sse_line output can be parsed back by parse_openai_sse_line.
#[test]
fn format_then_parse_roundtrip() {
    let original = serde_json::json!({
        "id": "chatcmpl-test",
        "object": "chat.completion.chunk",
        "created": 1234567890,
        "model": "gpt-4",
        "choices": [{
            "index": 0,
            "delta": {"content": "Hello"},
            "finish_reason": null
        }]
    });

    let formatted = format_sse_line(&original);
    // formatted is "data: {...}\n\n"
    // Parse just the data line (strip the trailing \n\n).
    let data_line = formatted.trim_end();
    let chunk = parse_openai_sse_line(data_line).unwrap().unwrap();

    assert!(!chunk.done);
    let pv = payload_value(&chunk);
    assert_eq!(pv["id"].as_str().unwrap(), "chatcmpl-test");
    assert_eq!(
        pv["choices"][0]["delta"]["content"].as_str().unwrap(),
        "Hello"
    );
}

/// Verify SSE_DONE constant round-trips correctly.
#[test]
fn sse_done_constant_parses_as_done() {
    let line = SSE_DONE.trim_end();
    let chunk = parse_openai_sse_line(line).unwrap().unwrap();
    assert!(chunk.done);
}

/// Format and parse a done sentinel.
#[test]
fn format_done_sentinel() {
    let done_value = serde_json::Value::Null;
    let formatted = format_sse_line(&done_value);
    // The formatted line is "data: null\n\n" — not the same as [DONE].
    // So we verify that format_sse_line for Null is different from SSE_DONE.
    assert_ne!(formatted, SSE_DONE);
    assert_eq!(formatted, "data: null\n\n");
}

// =====================================================================
// Edge cases in streaming context
// =====================================================================

/// A stream that sends [DONE] immediately (no content).
#[test]
fn openai_stream_immediate_done() {
    let sse_lines = vec!["data: [DONE]"];
    let mut content = String::new();
    let mut done = false;

    for line in &sse_lines {
        let chunk = parse_openai_sse_line(line).unwrap().unwrap();
        if chunk.done {
            done = true;
            break;
        }
        if let Some(text) = payload_value(&chunk)["choices"][0]["delta"]["content"].as_str() {
            content.push_str(text);
        }
    }

    assert!(done);
    assert!(content.is_empty());
}

/// A stream with multiple empty lines before [DONE].
#[test]
fn openai_stream_only_empty_lines_then_done() {
    let sse_lines = vec!["", "", "", "", "data: [DONE]"];
    let mut done = false;

    for line in &sse_lines {
        match parse_openai_sse_line(line).unwrap() {
            None => continue,
            Some(c) => {
                if c.done {
                    done = true;
                    break;
                }
            }
        }
    }
    assert!(done);
}

/// Gemini stream with no text in any candidate (only finish_reason).
#[test]
fn gemini_stream_finish_only_no_text() {
    let sse_lines = vec![
        r#"data: {"candidates":[{"content":{"parts":[]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":0,"totalTokenCount":5}}"#,
        "data: [DONE]",
    ];

    let mut content = String::new();
    let mut usage: Option<OpenAIUsage> = None;

    for line in &sse_lines {
        let chunk = parse_gemini_sse_line(line, "c", 0, "m").unwrap();
        match chunk {
            None => continue,
            Some(c) => {
                if c.done {
                    break;
                }
                if let Some(text) = c.payload["choices"][0]["delta"]["content"].as_str() {
                    content.push_str(text);
                }
                if c.usage.is_some() {
                    usage = c.usage;
                }
            }
        }
    }

    assert!(content.is_empty());
    assert!(usage.is_some());
    assert_eq!(usage.unwrap().completion_tokens, 0);
}

/// OpenAI stream with very long content across many chunks.
#[test]
fn openai_stream_many_small_chunks() {
    let mut sse_lines: Vec<String> = Vec::new();
    let expected: String = (0..1000).map(|i| format!("{} ", i)).collect();

    for i in 0..1000 {
        sse_lines.push(format!(
            r#"data: {{"id":"1","object":"chat.completion.chunk","created":0,"model":"gpt-4","choices":[{{"index":0,"delta":{{"content":"{} "}},"finish_reason":null}}]}}"#,
            i
        ));
    }
    sse_lines.push("data: [DONE]".to_string());

    let mut content = String::new();
    for line in &sse_lines {
        let chunk = parse_openai_sse_line(line).unwrap();
        match chunk {
            None => continue,
            Some(c) => {
                if c.done {
                    break;
                }
                if let Some(text) = payload_value(&c)["choices"][0]["delta"]["content"].as_str() {
                    content.push_str(text);
                }
            }
        }
    }

    assert_eq!(content, expected);
}

// =====================================================================
// Concurrent-ish parsing: same data parsed twice yields same result
// =====================================================================

/// Parsing the same SSE data multiple times produces identical results (idempotency).
#[test]
fn openai_parse_idempotent() {
    let line = r#"data: {"id":"1","object":"chat.completion.chunk","created":0,"model":"gpt-4","choices":[{"index":0,"delta":{"content":"test"},"finish_reason":null}]}"#;

    for _ in 0..100 {
        let chunk = parse_openai_sse_line(line).unwrap().unwrap();
        assert!(!chunk.done);
        let pv = payload_value(&chunk);
        assert_eq!(
            pv["choices"][0]["delta"]["content"].as_str().unwrap(),
            "test"
        );
    }
}

/// Parsing the same Gemini SSE data multiple times produces identical results.
#[test]
fn gemini_parse_idempotent() {
    let line = r#"data: {"candidates":[{"content":{"parts":[{"text":"test"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":2,"totalTokenCount":3}}"#;

    for _ in 0..100 {
        let chunk = parse_gemini_sse_line(line, "id", 0, "m").unwrap().unwrap();
        assert!(!chunk.done);
        assert_eq!(
            chunk.payload["choices"][0]["delta"]["content"]
                .as_str()
                .unwrap(),
            "test"
        );
        assert!(chunk.usage.is_some());
    }
}
