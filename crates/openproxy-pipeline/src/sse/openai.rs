//! OpenAI-compatible SSE parser.

use super::{UpstreamSseChunk, build_openai_usage, parse_provider_json, parse_sse_data_or_done};
use openproxy_types::error::Result;
use openproxy_types::message::PromptTokensDetails;
use serde_json::Value;

/// Extracts chunk metadata only. serde skips the fields the proxy does not
/// need (`delta.content`, `tool_calls`) without allocating them, which is
/// cheaper than parsing into `Value` per chunk. `usage` stays as raw
/// `Value`: free-tier gateways mix OpenAI and Anthropic-style token names
/// (`prompt_tokens` next to `input_tokens`) in one usage object, which
/// serde's derive rejects as `duplicate field`.
#[derive(serde::Deserialize)]
struct OpenAiSseProbe {
    #[serde(default)]
    usage: Option<Value>,
    #[serde(default)]
    choices: Option<Vec<OpenAiChoiceProbe>>,
}

/// First key in `keys` present in `obj` as `u64`. Canonical OpenAI names
/// take precedence over their Anthropic-style aliases; literal duplicate
/// keys were already collapsed to the last occurrence by the `Value` parse.
fn usage_u64(obj: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|k| obj.get(k).and_then(Value::as_u64))
}

const USAGE_DETAILS_KEYS: [&str; 4] = [
    "prompt_tokens_details",
    "prompt_token_details",
    "input_tokens_details",
    "input_token_details",
];
const USAGE_CACHED_KEYS: [&str; 5] = [
    "cached_tokens",
    "cached_prompt_tokens",
    "cache_read_input_tokens",
    "cache_read_tokens",
    "prompt_cache_hit_tokens",
];

/// Cached tokens from the usage details object, whichever naming style the
/// gateway picked.
fn usage_cached_tokens(usage: &Value) -> Option<u64> {
    USAGE_DETAILS_KEYS.iter().find_map(|dk| {
        let details = usage.get(dk)?;
        USAGE_CACHED_KEYS
            .iter()
            .find_map(|ck| details.get(ck).and_then(Value::as_u64))
    })
}

#[derive(serde::Deserialize)]
struct OpenAiChoiceProbe {
    finish_reason: Option<String>,
    #[serde(default)]
    delta: Option<OpenAiDeltaProbe>,
}

#[derive(serde::Deserialize)]
struct OpenAiDeltaProbe {
    #[serde(default)]
    reasoning_content: Option<String>,
}

/// Parse a single SSE line from an OpenAI-compatible upstream.
///
/// Returns `Ok(None)` for empty lines, comments, and `[DONE]` sentinels.
/// Returns `Ok(Some(chunk))` for valid data lines.
pub fn parse_openai_sse_line(line: &str) -> Result<Option<UpstreamSseChunk>> {
    let payload = match parse_sse_data_or_done(line) {
        super::SseDataOrDone::Payload(p) => p,
        super::SseDataOrDone::Done => return Ok(Some(UpstreamSseChunk::done())),
        super::SseDataOrDone::Skip => return Ok(None),
    };
    // Fast path: the probe only ever reads `usage`, `finish_reason` and
    // `delta.reasoning_content`. A memchr scan for the three key names costs
    // ~30ns per chunk and skips the serde parse (~2-4µs) on the >90% of
    // chunks that are pure content deltas. Substring match is sound here:
    // a false positive just takes the slow path and parses as before.
    if !(payload.contains("\"usage\"")
        || payload.contains("\"finish_reason\"")
        || payload.contains("\"reasoning_content\""))
    {
        return Ok(Some(UpstreamSseChunk {
            raw_payload: Some(payload.to_string()),
            payload: Value::Null,
            done: false,
            usage: None,
            stop_reason: None,
            delta_reasoning: None,
            delta_tool_calls: Vec::new(),
            has_content: true,
        }));
    }
    // Only usage and finish_reason are read; delta content and tool
    // calls are skipped.
    let probe: OpenAiSseProbe = parse_provider_json(payload, "openai")?;

    let usage = probe.usage.map(|u| {
        let cached = usage_cached_tokens(&u)
            .or_else(|| {
                usage_u64(
                    &u,
                    &[
                        "prompt_cache_hit_tokens",
                        "cached_tokens",
                        "cached_prompt_tokens",
                        "cache_read_input_tokens",
                        "cache_read_tokens",
                    ],
                )
            })
            .and_then(|c| u32::try_from(c).ok());

        build_openai_usage(
            usage_u64(&u, &["prompt_tokens", "input_tokens"]),
            usage_u64(&u, &["completion_tokens", "output_tokens"]),
            usage_u64(&u, &["total_tokens"]),
            cached.map(|c| PromptTokensDetails {
                cached_tokens: Some(c),
            }),
        )
    });
    // o1-style reasoning models (o1, o3, deepseek-r1) emit
    // `delta.reasoning_content` on chunks that also carry `usage` or a
    // non-null `finish_reason`, the slow path. Surface it on
    // `delta_reasoning` so the accumulator persists it as
    // `choices[0].message.reasoning_content`.
    let (delta_reasoning, finish_reason) = match probe.choices.and_then(|mut c| c.pop()) {
        Some(choice) => {
            let reasoning = choice.delta.and_then(|d| d.reasoning_content);
            (reasoning, choice.finish_reason)
        }
        None => (None, None),
    };

    Ok(Some(UpstreamSseChunk {
        raw_payload: Some(payload.to_string()),
        payload: Value::Null,
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
    fn parse_openai_data_line() {
        let line = r#"data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":0,"model":"gpt-4","choices":[{"index":0,"delta":{"content":"Hi"},"finish_reason":null}]}"#;
        let chunk = parse_openai_sse_line(line).unwrap().unwrap();
        assert!(!chunk.done);
        assert!(chunk.raw_payload.is_some());
        let v: serde_json::Value =
            serde_json::from_str(chunk.raw_payload.as_ref().unwrap()).unwrap();
        assert!(v.get("id").is_some());
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
        assert!(
            parse_openai_sse_line(": this is a comment")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn openai_line_without_data_prefix_returns_none() {
        assert!(
            parse_openai_sse_line("event: some_event")
                .unwrap()
                .is_none()
        );
        assert!(parse_openai_sse_line("id: 12345").unwrap().is_none());
        assert!(parse_openai_sse_line("retry: 5000").unwrap().is_none());
        assert!(
            parse_openai_sse_line("random text without prefix")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn openai_line_with_event_prefix_ignored() {
        assert!(parse_openai_sse_line("event: message").unwrap().is_none());
        assert!(
            parse_openai_sse_line("event: completion")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn openai_line_with_crlf_ending() {
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
        let long_content = "x".repeat(10_000);
        let payload = serde_json::json!({"content": long_content});
        let line = format!("data: {}", serde_json::to_string(&payload).unwrap());
        let chunk = parse_openai_sse_line(&line).unwrap().unwrap();
        assert!(!chunk.done);
        let v: serde_json::Value =
            serde_json::from_str(chunk.raw_payload.as_ref().unwrap()).unwrap();
        assert_eq!(v["content"].as_str().unwrap().len(), 10_000);
    }

    #[test]
    fn openai_unicode_content() {
        let payload = serde_json::json!({"content": "こんにちは世界 🌍 ñ ü ö ä"});
        let line = format!("data: {}", serde_json::to_string(&payload).unwrap());
        let chunk = parse_openai_sse_line(&line).unwrap().unwrap();
        let v: serde_json::Value =
            serde_json::from_str(chunk.raw_payload.as_ref().unwrap()).unwrap();
        assert_eq!(v["content"].as_str().unwrap(), "こんにちは世界 🌍 ñ ü ö ä");
    }

    #[test]
    fn openai_malformed_json_returns_error() {
        // The fast path only parses payloads that mention the probed keys;
        // use a payload with `usage` so it still reaches the JSON parser.
        let result = parse_openai_sse_line(r#"data: {"usage": not valid json}"#);
        assert!(result.is_err(), "malformed JSON should produce an error");
        match result {
            Err(CoreError::Parse(_)) => {} // expected
            Err(other) => panic!("expected Parse error, got: {other}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }

    #[test]
    fn openai_multiple_sequential_lines_processed_independently() {
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
            if let Some(ref raw) = chunk.raw_payload {
                let v: serde_json::Value = serde_json::from_str(raw).unwrap();
                if let Some(content) = v["choices"][0]["delta"]["content"].as_str() {
                    contents.push(content.to_string());
                }
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

    #[test]
    fn openai_data_prefix_with_extra_spaces() {
        let line = r#"data:  {"id":"x","object":"chat.completion.chunk","created":0,"model":"gpt-4","choices":[]}"#;
        let chunk = parse_openai_sse_line(line).unwrap().unwrap();
        assert!(!chunk.done);
    }

    #[test]
    fn openai_only_whitespace_line() {
        assert!(parse_openai_sse_line("   \t  ").unwrap().is_none());
    }

    #[test]
    fn openai_sse_parses_cached_tokens() {
        let line = r#"data: {"choices":[],"usage":{"prompt_tokens":1200,"completion_tokens":300,"total_tokens":1500,"prompt_tokens_details":{"cached_tokens":1000}}}"#;
        let chunk = parse_openai_sse_line(line).unwrap().unwrap();
        let usage = chunk.usage.expect("usage should be parsed");
        assert_eq!(usage.prompt_tokens, 1200);
        assert_eq!(usage.completion_tokens, 300);
        assert_eq!(
            usage
                .prompt_tokens_details
                .as_ref()
                .and_then(|d| d.cached_tokens),
            Some(1000)
        );
    }

    #[test]
    fn openai_sse_parses_prompt_cache_hit_tokens() {
        let line = r#"data: {"choices":[],"usage":{"prompt_tokens":9669,"completion_tokens":1820,"total_tokens":11489,"prompt_cache_hit_tokens":5120,"cached_tokens":0}}"#;
        let chunk = parse_openai_sse_line(line).unwrap().unwrap();
        let usage = chunk.usage.expect("usage should be parsed");
        assert_eq!(usage.prompt_tokens, 9669);
        assert_eq!(
            usage
                .prompt_tokens_details
                .as_ref()
                .and_then(|d| d.cached_tokens),
            Some(5120)
        );
    }

    #[test]
    fn openai_usage_accepts_mixed_openai_and_anthropic_style_keys() {
        // Free-tier gateways emit both naming styles in one usage object;
        // serde's derive rejects the pair as `duplicate field`.
        let line = r#"data: {"choices":[],"usage":{"prompt_tokens":1200,"input_tokens":1200,"completion_tokens":300,"output_tokens":300,"total_tokens":1500}}"#;
        let chunk = parse_openai_sse_line(line).unwrap().unwrap();
        let usage = chunk.usage.expect("usage should be parsed");
        assert_eq!(usage.prompt_tokens, 1200);
        assert_eq!(usage.completion_tokens, 300);
        assert_eq!(usage.total_tokens, 1500);
    }

    #[test]
    fn openai_usage_duplicate_key_collapses_to_last() {
        let line = r#"data: {"choices":[],"usage":{"prompt_tokens":900,"prompt_tokens":1200,"completion_tokens":300,"total_tokens":1500}}"#;
        let chunk = parse_openai_sse_line(line).unwrap().unwrap();
        let usage = chunk.usage.expect("usage should be parsed");
        assert_eq!(usage.prompt_tokens, 1200);
    }

    #[test]
    fn openai_chunk_duplicate_top_level_keys_collapse() {
        let line =
            r#"data: {"usage":{"prompt_tokens":1},"usage":{"prompt_tokens":2},"choices":[]}"#;
        let chunk = parse_openai_sse_line(line).unwrap().unwrap();
        let usage = chunk.usage.expect("usage should be parsed");
        assert_eq!(usage.prompt_tokens, 2);
    }
}
