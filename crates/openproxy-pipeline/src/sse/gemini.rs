//! Gemini SSE parser.

use super::{UpstreamSseChunk, parse_provider_json, parse_sse_data_or_done};
use crate::translation::OpenAIUsage;
use openproxy_types::ResultExt;
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
    // The snake_case spelling is a separate field instead of a
    // `#[serde(alias)]`: upstreams do emit both spellings in one object, and
    // an alias makes serde reject the frame with `duplicate field`. CamelCase
    // keeps canonical precedence in `extract_gemini_usage_metadata`.
    #[serde(default, rename = "usage_metadata")]
    usage_metadata_snake: Option<GeminiUsageProbe>,
    #[serde(default)]
    response: Option<GeminiInnerSseProbe>,
}

#[derive(serde::Deserialize, Default)]
struct GeminiInnerSseProbe {
    #[serde(default)]
    candidates: Vec<GeminiCandidateProbe>,
    #[serde(default, rename = "usageMetadata")]
    usage_metadata: Option<GeminiUsageProbe>,
    #[serde(default, rename = "usage_metadata")]
    usage_metadata_snake: Option<GeminiUsageProbe>,
}

#[derive(serde::Deserialize, Default)]
struct GeminiCandidateProbe {
    #[serde(default)]
    content: Option<GeminiContentProbe>,
    #[serde(default, rename = "finishReason")]
    finish_reason: Option<String>,
    #[serde(default, rename = "finishMessage")]
    finish_message: Option<String>,
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

/// Gemini usage counters. CamelCase and snake_case are separate fields on
/// purpose: an upstream can emit both spellings in the same object, and
/// `#[serde(alias)]` would abort the frame with `duplicate field`. The
/// canonical camelCase spelling takes precedence; snake_case is the fallback.
#[derive(serde::Deserialize, Default)]
struct GeminiUsageProbe {
    #[serde(default, rename = "promptTokenCount")]
    prompt_tokens: Option<u64>,
    #[serde(default, rename = "prompt_token_count")]
    prompt_tokens_snake: Option<u64>,
    #[serde(default, rename = "candidatesTokenCount")]
    completion_tokens: Option<u64>,
    #[serde(default, rename = "candidates_token_count")]
    completion_tokens_snake: Option<u64>,
    #[serde(default, rename = "totalTokenCount")]
    total_tokens: Option<u64>,
    #[serde(default, rename = "total_token_count")]
    total_tokens_snake: Option<u64>,
    #[serde(default, rename = "cachedContentTokenCount")]
    cached_tokens: Option<u64>,
    #[serde(default, rename = "cached_content_token_count")]
    cached_tokens_snake: Option<u64>,
}

impl GeminiUsageProbe {
    fn prompt(&self) -> Option<u64> {
        self.prompt_tokens.or(self.prompt_tokens_snake)
    }

    fn completion(&self) -> Option<u64> {
        self.completion_tokens.or(self.completion_tokens_snake)
    }

    fn total(&self) -> Option<u64> {
        self.total_tokens.or(self.total_tokens_snake)
    }

    fn cached(&self) -> Option<u64> {
        self.cached_tokens.or(self.cached_tokens_snake)
    }
}

/// Map a Gemini finishReason to an OpenAI finish_reason string.
fn map_gemini_finish_reason(reason: &str) -> String {
    match reason {
        "STOP" => "stop".to_string(),
        "MAX_TOKENS" => "length".to_string(),
        "SAFETY" | "RECITATION" | "BLOCKLIST" => "content_filter".to_string(),
        "MALFORMED_FUNCTION_CALL" | "UNEXPECTED_TOOL_CALL" => "tool_calls".to_string(),
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
    probe
        .usage_metadata
        .as_ref()
        .or(probe.usage_metadata_snake.as_ref())
        .or_else(|| {
            probe.response.as_ref().and_then(|r| {
                r.usage_metadata
                    .as_ref()
                    .or(r.usage_metadata_snake.as_ref())
            })
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

fn extract_gemini_usage_presence(
    usage_metadata: Option<&GeminiUsageProbe>,
) -> openproxy_types::UsagePresence {
    usage_metadata.map_or_else(openproxy_types::UsagePresence::none, |u| {
        openproxy_types::UsagePresence {
            prompt: u.prompt().is_some(),
            completion: u.completion().is_some(),
            total: u.total().is_some(),
            cached: u.cached().is_some(),
        }
    })
}

/// Internal (non-wire) usage: every field the upstream reported, with the
/// missing ones left at zero and flagged absent by
/// [`extract_gemini_usage_presence`]. Gemini emits `usageMetadata` on several
/// frames and the cache counter frequently arrives without a candidate count,
/// so dropping the whole block would discard observed prompt/cache tokens.
/// `None` only when the metadata carried no counter at all.
fn extract_gemini_usage_internal(usage_metadata: Option<&GeminiUsageProbe>) -> Option<OpenAIUsage> {
    let u = usage_metadata?;
    if u.prompt().is_none()
        && u.completion().is_none()
        && u.total().is_none()
        && u.cached().is_none()
    {
        return None;
    }
    Some(super::build_openai_usage(
        u.prompt(),
        u.completion(),
        u.total(),
        u.cached()
            .map(|cached| openproxy_types::PromptTokensDetails {
                cached_tokens: Some(cached.try_into().unwrap_or(u32::MAX)),
            }),
    ))
}

fn extract_gemini_usage(usage_metadata: Option<&GeminiUsageProbe>) -> Option<OpenAIUsage> {
    let u = usage_metadata?;
    // Wire contract: only a complete observation is emitted. Partial usage
    // metadata is dropped from the frame rather than emitting an `OpenAIUsage`
    // of `0`s, which would corrupt downstream billing.
    Some(super::build_openai_usage(
        Some(u.prompt()?),
        Some(u.completion()?),
        Some(u.total()?),
        u.cached()
            .map(|cached| openproxy_types::PromptTokensDetails {
                cached_tokens: Some(cached.try_into().unwrap_or(u32::MAX)),
            }),
    ))
}

pub fn parse_gemini_sse_line_test(
    line: &str,
    chunk_id: &str,
    created: u64,
    model: &str,
) -> Result<Option<UpstreamSseChunk>> {
    let mut counter = 0;
    parse_gemini_sse_line(line, chunk_id, created, model, &mut counter)
}

pub fn parse_gemini_sse_line(
    line: &str,
    chunk_id: &str,
    created: u64,
    model: &str,
    tool_call_index_counter: &mut u32,
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
    for fc in function_calls {
        let global_idx = *tool_call_index_counter;
        *tool_call_index_counter += 1;
        let call_id = fc
            .id
            .unwrap_or_else(|| format!("call_{chunk_id}_{global_idx}"));
        let args_str = fc.args.map_or_else(
            || "{}".to_string(),
            |a| serde_json::to_string(&a).unwrap_or_else(|_| "{}".to_string()),
        );
        delta_tool_calls.push(json!({
            "index": global_idx,
            "id": call_id,
            "type": "function",
            "function": {
                "name": fc.name.unwrap_or_default(),
                "arguments": args_str,
            }
        }));
    }

    // Synthesize tool_call if Gemini aborted with MALFORMED_FUNCTION_CALL
    if let Some(candidate) = candidates.first()
        && let Some(fr) = candidate.finish_reason.as_deref()
        && (fr == "MALFORMED_FUNCTION_CALL" || fr == "UNEXPECTED_TOOL_CALL")
        && delta_tool_calls.is_empty()
    {
        let global_idx = *tool_call_index_counter;
        *tool_call_index_counter += 1;
        let call_id = format!("call_{chunk_id}_{global_idx}");
        let finish_msg = candidate.finish_message.as_deref().unwrap_or(fr);

        // Try to parse tool name and arguments from Gemini's finishMessage:
        // Format e.g.: "Malformed function call: call:terminal{command:\"ls\"}"
        let (name, args_str) = if let Some(idx) = finish_msg.find("call:") {
            let rest = &finish_msg[idx + 5..];
            if let Some(brace_idx) = rest.find('{') {
                let tool_name = rest[..brace_idx].trim();
                let raw_args = &rest[brace_idx..];
                // If args end with '}' use it, else serialize as error object
                (tool_name.to_string(), raw_args.to_string())
            } else {
                (
                    "malformed_tool_call".to_string(),
                    json!({"error": fr, "message": finish_msg}).to_string(),
                )
            }
        } else {
            (
                "malformed_tool_call".to_string(),
                json!({"error": fr, "message": finish_msg}).to_string(),
            )
        };

        delta_tool_calls.push(json!({
            "index": global_idx,
            "id": call_id,
            "type": "function",
            "function": {
                "name": name,
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

    // Wire usage: only a complete observation is serialized into the frame, as
    // before. `internal_usage` additionally keeps the fields that did arrive
    // (prompt-only, cache-only) so the fold and the usage record do not lose an
    // observed count just because a sibling counter is missing.
    let wire_usage = extract_gemini_usage(usage_metadata);
    let internal_usage = extract_gemini_usage_internal(usage_metadata);
    let usage_presence = extract_gemini_usage_presence(usage_metadata);
    if let Some(usage) = &wire_usage {
        chunk["usage"] = serde_json::to_value(usage).ctx_parse("gemini usage serialize")?;
    }
    let has_content = !text.is_empty() || delta_reasoning.is_some() || !delta_tool_calls.is_empty();

    // If the chunk carries no visible output, no finish reason, no tool calls and no usage,
    // but the upstream reported candidates (e.g. empty thought parts, empty content while
    // reasoning server-side) or an empty generation response heartbeat, emit a liveness chunk.
    // This notifies the streaming body stream that upstream generation is in progress,
    // switching off the TTFT deadline without emitting empty null content to the client.
    // If the chunk carries no visible output, no finish reason and no complete
    // wire usage, it must not be forwarded (liveness). The counters that did
    // arrive still ride along so the fold/record keeps the observation.
    if !has_content && finish_reason.is_none() && wire_usage.is_none() {
        return Ok(Some(UpstreamSseChunk {
            raw_payload: None,
            payload: Value::Null,
            done: false,
            usage: internal_usage,
            usage_presence,
            stop_reason: None,
            delta_reasoning: None,
            delta_tool_calls: Vec::new(),
            has_content: false,
        }));
    }

    Ok(Some(UpstreamSseChunk {
        raw_payload: None,
        payload: chunk,
        done: false,
        usage: internal_usage,
        usage_presence,
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
    use super::parse_gemini_sse_line_test as parse_gemini_sse_line;
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
    fn gemini_usage_from_response_wrapper_and_snake_case_aliases() {
        // The Gemini adapter accepts a `response` envelope and snake_case
        // keys; usage parsing must accept the same shapes instead of only the
        // root-level camelCase `usageMetadata`.
        let wrapped = r#"data: {"response":{"candidates":[{"content":{"parts":[{"text":"hi"}]}}],"usageMetadata":{"promptTokenCount":11,"candidatesTokenCount":4,"totalTokenCount":15,"cachedContentTokenCount":8}}}"#;
        let chunk = parse_gemini_sse_line(wrapped, "id", 0, "m")
            .unwrap()
            .unwrap();
        let u = chunk.usage.expect("wrapper usage");
        assert_eq!(u.prompt_tokens, 11);
        assert_eq!(u.completion_tokens, 4);
        assert_eq!(u.total_tokens, 15);
        assert_eq!(
            u.prompt_tokens_details
                .as_ref()
                .and_then(|d| d.cached_tokens),
            Some(8)
        );
        assert_eq!(
            chunk.usage_presence,
            openproxy_types::UsagePresence {
                prompt: true,
                completion: true,
                total: true,
                cached: true,
            }
        );

        let snake = r#"data: {"usage_metadata":{"prompt_token_count":9,"candidates_token_count":3,"total_token_count":12,"cached_content_token_count":2}}"#;
        let chunk = parse_gemini_sse_line(snake, "id", 0, "m").unwrap().unwrap();
        let u = chunk.usage.expect("snake_case usage");
        assert_eq!(u.prompt_tokens, 9);
        assert_eq!(u.completion_tokens, 3);
        assert_eq!(u.total_tokens, 12);
        assert!(chunk.usage_presence.cached);
    }

    #[test]
    fn gemini_dual_camel_and_snake_case_in_one_object_parses() {
        // The review probe saw `duplicate field` when both spellings arrived
        // in the same usage object (a `#[serde(alias)]` artifact). The frame
        // must parse, with camelCase winning.
        let line = r#"data: {"usageMetadata":{"promptTokenCount":47,"prompt_token_count":47,"candidatesTokenCount":0,"totalTokenCount":47}}"#;
        let chunk = parse_gemini_sse_line(line, "id", 0, "m")
            .expect("both spellings must parse")
            .expect("chunk");
        let u = chunk.usage.expect("usage");
        assert_eq!(u.prompt_tokens, 47);
        assert_eq!(u.completion_tokens, 0);
        assert_eq!(u.total_tokens, 47);
        assert!(
            chunk.usage_presence.completion,
            "the explicit zero is present"
        );
        assert_eq!(u.prompt_tokens, 47, "camelCase takes canonical precedence");
    }

    #[test]
    fn gemini_dual_usage_metadata_and_usage_metadata_keys_parse() {
        // Both `usageMetadata` and `usage_metadata` present at the top level:
        // camelCase is canonical, the snake_case one is not a hard error.
        let line = r#"data: {"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2,"totalTokenCount":7},"usage_metadata":{"prompt_token_count":9,"candidates_token_count":3,"total_token_count":12}}"#;
        let chunk = parse_gemini_sse_line(line, "id", 0, "m").unwrap().unwrap();
        let u = chunk.usage.expect("usage");
        assert_eq!(
            (u.prompt_tokens, u.completion_tokens, u.total_tokens),
            (5, 2, 7)
        );
    }

    #[test]
    fn gemini_partial_usage_keeps_observed_fields_out_of_the_wire() {
        // Partial metadata (prompt+total, no candidates) must not fabricate a
        // completion, but the counters that did arrive are still observations:
        // they stay in the internal usage for the fold/record while the frame
        // keeps carrying no usage block, exactly as before.
        let line = r#"data: {"usageMetadata":{"promptTokenCount":5,"totalTokenCount":5}}"#;
        let chunk = parse_gemini_sse_line(line, "id", 0, "m").unwrap().unwrap();
        assert!(
            chunk.payload.get("usage").is_none(),
            "the wire must not gain a partial usage block"
        );
        assert_eq!(
            chunk.usage_presence,
            openproxy_types::UsagePresence {
                prompt: true,
                completion: false,
                total: true,
                cached: false,
            }
        );
        let u = chunk
            .usage
            .expect("internal usage keeps the observed fields");
        assert_eq!(u.prompt_tokens, 5);
        assert_eq!(u.total_tokens, 5);
    }

    #[test]
    fn gemini_cache_only_frame_keeps_prompt_and_cached_observation() {
        // 47 prompt + 31 cached with no candidates counter: the review probe
        // saw both dropped. They must survive internally and stay flagged.
        let line =
            r#"data: {"usageMetadata":{"promptTokenCount":47,"cachedContentTokenCount":31}}"#;
        let chunk = parse_gemini_sse_line(line, "id", 0, "m").unwrap().unwrap();
        assert!(chunk.payload.get("usage").is_none());
        assert_eq!(
            chunk.usage_presence,
            openproxy_types::UsagePresence {
                prompt: true,
                completion: false,
                total: false,
                cached: true,
            }
        );
        let u = chunk.usage.expect("internal usage");
        assert_eq!(u.prompt_tokens, 47);
        assert_eq!(
            u.prompt_tokens_details
                .as_ref()
                .and_then(|d| d.cached_tokens),
            Some(31)
        );
    }

    #[test]
    fn gemini_split_cache_across_frames_is_not_lost() {
        // Real Gemini streams report the prompt on one frame and the cache
        // counter on a later one. Folding must keep both, and the presence
        // flags must be the union of what each frame declared.
        let first = r#"data: {"usageMetadata":{"promptTokenCount":47,"candidatesTokenCount":9,"totalTokenCount":56}}"#;
        let second = r#"data: {"candidates":[{"content":{"parts":[{"text":"hi"}]},"finishReason":"STOP"}],"usageMetadata":{"cachedContentTokenCount":31,"totalTokenCount":56}}"#;
        let (resp, presence) = crate::sse::parse_sse_stream_to_openai_response_with_presence(
            openproxy_types::TargetFormat::Gemini,
            &format!("{first}\n\n{second}\n\n"),
            "gemini-2.5-pro",
        )
        .unwrap();
        let u = resp.usage.expect("folded usage");
        assert_eq!(u.prompt_tokens, 47);
        assert_eq!(u.completion_tokens, 9);
        assert_eq!(
            u.prompt_tokens_details
                .as_ref()
                .and_then(|d| d.cached_tokens),
            Some(31),
            "cache counter from the later frame survives the fold"
        );
        assert_eq!(
            presence,
            openproxy_types::UsagePresence {
                prompt: true,
                completion: true,
                total: true,
                cached: true,
            }
        );
    }

    #[test]
    fn gemini_explicit_zero_completion_stays_observed() {
        let line = r#"data: {"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":0,"totalTokenCount":7}}"#;
        let chunk = parse_gemini_sse_line(line, "id", 0, "m").unwrap().unwrap();
        let u = chunk.usage.expect("explicit zero completion");
        assert_eq!(u.completion_tokens, 0);
        assert!(chunk.usage_presence.prompt);
        assert!(chunk.usage_presence.completion);
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
    fn gemini_only_ellipsis_tokens_emits_liveness() {
        let line = r#"data: {"candidates":[{"content":{"parts":[]}}]}"#;
        let chunk = parse_gemini_sse_line(line, "id", 0, "m").unwrap().unwrap();
        assert!(chunk.is_liveness());
    }

    #[test]
    fn gemini_empty_thought_emits_liveness() {
        let line = r#"data: {"candidates":[{"content":{"parts":[{"thought":true}]}}]}"#;
        let chunk = parse_gemini_sse_line(line, "id", 0, "m").unwrap().unwrap();
        assert!(chunk.is_liveness());
    }

    #[test]
    fn gemini_empty_thought_empty_text_emits_liveness() {
        let line = r#"data: {"candidates":[{"content":{"parts":[{"thought":true,"text":""}]}}]}"#;
        let chunk = parse_gemini_sse_line(line, "id", 0, "m").unwrap().unwrap();
        assert!(chunk.is_liveness());
    }

    #[test]
    fn gemini_empty_response_heartbeat_emits_liveness() {
        let line = r#"data: {"response":{"candidates":[{"content":{"parts":[]}}]}}"#;
        let chunk = parse_gemini_sse_line(line, "id", 0, "m").unwrap().unwrap();
        assert!(chunk.is_liveness());
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
