//! Gemini SSE parser.

use super::{UpstreamSseChunk, parse_provider_json, parse_sse_data_or_done};
use crate::translation::OpenAIUsage;
use openproxy_types::error::Result;
use serde_json::{Value, json};

/// Reads only the fields the proxy needs. Skipping unknown fields (`role`,
/// `index`, `safetyRatings`) avoids the allocation a per-chunk
/// `from_str::<Value>` would cost. `#[serde(rename = ...)]` maps Gemini's
/// camelCase wire format onto Rust snake_case.
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

#[derive(serde::Deserialize, Default, Clone)]
struct GeminiFunctionCallProbe {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    args: Option<serde_json::Value>,
    #[serde(default)]
    id: Option<String>,
}

#[derive(serde::Deserialize, Default)]
struct GeminiPartProbe {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thought: Option<bool>,
    #[serde(default, rename = "functionCall")]
    function_call: Option<GeminiFunctionCallProbe>,
}

#[derive(serde::Deserialize)]
struct GeminiUsageProbe {
    #[serde(default, rename = "promptTokenCount")]
    prompt_tokens: Option<u64>,
    #[serde(default, rename = "candidatesTokenCount")]
    completion_tokens: Option<u64>,
    #[serde(default, rename = "totalTokenCount")]
    total_tokens: Option<u64>,
    #[serde(default, rename = "cachedContentTokenCount")]
    cached_tokens: Option<u64>,
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

/// Candidates from the top-level array or the `response` wrapper.
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

fn extract_gemini_parts(
    candidates: &[GeminiCandidateProbe],
) -> (String, Option<String>, Vec<GeminiFunctionCallProbe>) {
    let mut content_parts = String::new();
    let mut reasoning_parts = String::new();
    let mut function_calls = Vec::new();
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
            if let Some(fc) = &part.function_call {
                function_calls.push(fc.clone());
            }
        }
    }
    let dr = (!reasoning_parts.is_empty()).then_some(reasoning_parts);
    (content_parts, dr, function_calls)
}

fn extract_gemini_usage(usage_metadata: Option<&GeminiUsageProbe>) -> Option<OpenAIUsage> {
    let u = usage_metadata?;
    // Partial usage metadata is dropped rather than emitting an `OpenAIUsage`
    // of `0`s, which would corrupt downstream billing.
    Some(super::build_openai_usage(
        Some(u.prompt_tokens?),
        Some(u.completion_tokens?),
        Some(u.total_tokens?),
        u.cached_tokens
            .map(|cached| openproxy_types::PromptTokensDetails {
                cached_tokens: Some(cached.try_into().unwrap_or(u32::MAX)),
            }),
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
    let (text, delta_reasoning, function_calls) = extract_gemini_parts(candidates);

    let mut finish_reason = candidates
        .first()
        .and_then(|c| c.finish_reason.as_deref())
        .map(map_gemini_finish_reason);

    let mut delta_tool_calls = Vec::new();
    for (idx, fc) in function_calls.into_iter().enumerate() {
        let call_id = fc.id.unwrap_or_else(|| format!("call_{chunk_id}_{idx}"));
        let args_str = fc.args.map_or_else(
            || "{}".to_string(),
            |a| serde_json::to_string(&a).unwrap_or_else(|_| "{}".to_string()),
        );
        delta_tool_calls.push(json!({
            "index": idx,
            "id": call_id,
            "type": "function",
            "function": {
                "name": fc.name.unwrap_or_default(),
                "arguments": args_str,
            }
        }));
    }

    if !delta_tool_calls.is_empty()
        && (finish_reason.is_none() || finish_reason.as_deref() == Some("stop"))
    {
        finish_reason = Some("tool_calls".to_string());
    }

    let mut delta = json!({});
    if !text.is_empty() {
        delta["content"] = json!(text);
    }
    if let Some(reasoning) = &delta_reasoning {
        delta["reasoning_content"] = json!(reasoning);
        if !delta.as_object().is_some_and(|o| o.contains_key("content")) {
            delta["content"] = json!("");
        }
    }
    if !delta_tool_calls.is_empty() {
        delta["tool_calls"] = json!(delta_tool_calls);
    }
    let finish_val = finish_reason.as_ref().map_or(Value::Null, |r| json!(r));

    let mut chunk = json!({
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
    if let Some(usage) = &usage {
        chunk["usage"] = serde_json::to_value(usage)
            .map_err(|e| openproxy_types::error::CoreError::Parse(e.to_string()))?;
    }
    let has_content = !text.is_empty() || delta_reasoning.is_some() || !delta_tool_calls.is_empty();

    Ok(Some(UpstreamSseChunk {
        raw_payload: None,
        payload: chunk,
        done: false,
        usage,
        stop_reason: finish_reason,
        delta_reasoning,
        delta_tool_calls,
        has_content,
    }))
}

#[cfg(test)]
#[path = "gemini_cache_tests.rs"]
mod cache_tests;

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
        let line = r#"data: {"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":0,"totalTokenCount":1}}"#;
        let chunk = parse_gemini_sse_line(line, "id", 0, "m").unwrap().unwrap();
        assert!(!chunk.done);
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
        let line = r#"data: {"candidates":[{"content":{"parts":[{"text":"a"}]}}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":7,"totalTokenCount":10}}"#;
        let chunk = parse_gemini_sse_line(line, "id", 0, "m").unwrap().unwrap();
        assert!(chunk.usage.is_some());
        let u = chunk.usage.unwrap();
        assert_eq!(u.prompt_tokens, 3);
        assert_eq!(u.completion_tokens, 7);
        assert_eq!(u.total_tokens, 10);
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
        let line = r#"data: {"candidates":[{"content":{"parts":[]}}]}"#;
        let chunk = parse_gemini_sse_line(line, "id", 0, "m").unwrap().unwrap();
        let content = chunk.payload["choices"][0]["delta"]["content"]
            .as_str()
            .unwrap_or("");
        assert!(content.is_empty());
    }

    #[test]
    fn gemini_parts_with_non_text_fields_ignored() {
        let line = r#"data: {"candidates":[{"content":{"parts":[{"thought":true},{"text":"real answer"}]}}]}"#;
        let chunk = parse_gemini_sse_line(line, "id", 0, "m").unwrap().unwrap();
        assert_eq!(
            chunk.payload["choices"][0]["delta"]["content"]
                .as_str()
                .unwrap(),
            "real answer"
        );
    }

    /// `finish()` rebuilds the persisted message from `delta.content`, so
    /// thought text landing there would corrupt the user's `content`.
    #[test]
    fn gemini_streaming_response_body_separates_thought_from_text() {
        let line = r#"data: {"candidates":[{"content":{"parts":[{"text":"r","thought":true},{"text":"a"}]}}]}"#;
        let chunk = parse_gemini_sse_line(line, "id", 0, "m").unwrap().unwrap();
        assert_eq!(
            chunk.delta_reasoning.as_deref(),
            Some("r"),
            "delta_reasoning must contain the thought:true text"
        );
        assert_eq!(
            chunk.payload["choices"][0]["delta"]["content"]
                .as_str()
                .unwrap(),
            "a",
            "the translated payload's delta.content must carry ONLY non-thought text"
        );
    }

    #[test]
    fn gemini_streaming_function_call_translates_to_openai_tool_calls() {
        let line = r#"data: {"candidates":[{"content":{"parts":[{"functionCall":{"name":"get_weather","args":{"location":"Paris"}}}]},"finishReason":"STOP"}]}"#;
        let chunk = parse_gemini_sse_line(line, "chunk_123", 1000, "gemini-1.5-pro")
            .unwrap()
            .unwrap();
        assert!(!chunk.done);
        assert!(chunk.has_content);
        assert_eq!(chunk.stop_reason.as_deref(), Some("tool_calls"));

        let choice = &chunk.payload["choices"][0];
        assert_eq!(choice["finish_reason"].as_str().unwrap(), "tool_calls");
        let tool_calls = &choice["delta"]["tool_calls"];
        assert!(tool_calls.is_array());
        let tc = &tool_calls[0];
        assert_eq!(tc["index"].as_u64().unwrap(), 0);
        assert_eq!(tc["type"].as_str().unwrap(), "function");
        assert_eq!(tc["function"]["name"].as_str().unwrap(), "get_weather");
        assert_eq!(
            tc["function"]["arguments"].as_str().unwrap(),
            r#"{"location":"Paris"}"#
        );
        assert_eq!(chunk.delta_tool_calls.len(), 1);
        assert_eq!(chunk.delta_tool_calls[0]["function"]["name"], "get_weather");
    }

    #[test]
    fn gemini_streaming_function_call_empty_and_null_arguments() {
        let line_empty_obj = r#"data: {"candidates":[{"content":{"parts":[{"functionCall":{"name":"f_empty","args":{}}}]},"finishReason":"STOP"}]}"#;
        let chunk1 = parse_gemini_sse_line(line_empty_obj, "c1", 1000, "gemini-1.5-flash")
            .unwrap()
            .unwrap();
        let tc1 = &chunk1.payload["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(tc1["function"]["name"], "f_empty");
        assert_eq!(tc1["function"]["arguments"], "{}");

        let line_omitted = r#"data: {"candidates":[{"content":{"parts":[{"functionCall":{"name":"f_no_args"}}]}}]}"#;
        let chunk2 = parse_gemini_sse_line(line_omitted, "c2", 1000, "gemini-1.5-flash")
            .unwrap()
            .unwrap();
        let tc2 = &chunk2.payload["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(tc2["function"]["name"], "f_no_args");
        assert_eq!(tc2["function"]["arguments"], "{}");

        let line_null = r#"data: {"candidates":[{"content":{"parts":[{"functionCall":{"name":"f_null","args":null}}]}}]}"#;
        let chunk3 = parse_gemini_sse_line(line_null, "c3", 1000, "gemini-1.5-flash")
            .unwrap()
            .unwrap();
        let tc3 = &chunk3.payload["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(tc3["function"]["name"], "f_null");
        println!(
            "chunk3 arguments for null args: {:?}",
            tc3["function"]["arguments"]
        );
    }

    #[test]
    fn gemini_streaming_multiple_function_calls_in_one_chunk() {
        let line = r#"data: {"candidates":[{"content":{"parts":[
            {"functionCall":{"name":"get_weather","args":{"city":"Madrid"},"id":"call_madrid"}},
            {"functionCall":{"name":"get_time","args":{"tz":"UTC"},"id":"call_time"}},
            {"functionCall":{"name":"calc","args":{"expr":"2+2"}}}
        ]}}]}"#;
        let chunk = parse_gemini_sse_line(line, "c_multi", 1000, "gemini-1.5-pro")
            .unwrap()
            .unwrap();
        assert!(!chunk.done);
        assert!(chunk.has_content);
        assert_eq!(chunk.stop_reason.as_deref(), Some("tool_calls"));
        assert_eq!(chunk.delta_tool_calls.len(), 3);

        let tcs = chunk.payload["choices"][0]["delta"]["tool_calls"]
            .as_array()
            .unwrap();
        assert_eq!(tcs.len(), 3);
        assert_eq!(tcs[0]["index"], 0);
        assert_eq!(tcs[0]["id"], "call_madrid");
        assert_eq!(tcs[0]["function"]["name"], "get_weather");
        assert_eq!(tcs[1]["index"], 1);
        assert_eq!(tcs[1]["id"], "call_time");
        assert_eq!(tcs[1]["function"]["name"], "get_time");
        assert_eq!(tcs[2]["index"], 2);
        assert_eq!(tcs[2]["id"], "call_c_multi_2");
        assert_eq!(tcs[2]["function"]["name"], "calc");
    }

    #[test]
    fn gemini_streaming_thought_and_text_with_function_call() {
        let line = r#"data: {"candidates":[{"content":{"parts":[
            {"text":"I should check the forecast first.","thought":true},
            {"text":"Checking weather for you..."},
            {"functionCall":{"name":"weather_lookup","args":{"loc":"Rome"}}}
        ]}}]}"#;
        let chunk = parse_gemini_sse_line(line, "c_mixed", 1000, "gemini-2.0")
            .unwrap()
            .unwrap();
        assert_eq!(
            chunk.delta_reasoning.as_deref(),
            Some("I should check the forecast first.")
        );
        let choice = &chunk.payload["choices"][0];
        assert_eq!(
            choice["delta"]["content"].as_str().unwrap(),
            "Checking weather for you..."
        );
        let tcs = choice["delta"]["tool_calls"].as_array().unwrap();
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0]["function"]["name"], "weather_lookup");
        assert_eq!(chunk.stop_reason.as_deref(), Some("tool_calls"));
    }
}
