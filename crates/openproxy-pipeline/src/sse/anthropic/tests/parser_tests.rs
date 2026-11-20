use super::super::*;
use openproxy_types::message::{OpenAIUsage, PromptTokensDetails};

#[test]
fn anthropic_event_line_sets_current_event() {
    let mut current_event = None;
    let result =
        parse_anthropic_sse_stream_line("event: message_start", &mut current_event).unwrap();
    assert!(result.is_none());
    assert_eq!(current_event.as_deref(), Some("message_start"));
}

#[test]
fn anthropic_data_line_returns_payload_with_event() {
    let mut current_event = Some("content_block_delta".to_string());
    let result =
        parse_anthropic_sse_stream_line(r#"data: {"delta":{"text":"Hello"}}"#, &mut current_event)
            .unwrap()
            .unwrap();
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
    assert!(
        parse_anthropic_sse_stream_line("id: 123", &mut current_event)
            .unwrap()
            .is_none()
    );
    assert!(
        parse_anthropic_sse_stream_line("retry: 5000", &mut current_event)
            .unwrap()
            .is_none()
    );
    assert!(
        parse_anthropic_sse_stream_line(": comment", &mut current_event)
            .unwrap()
            .is_none()
    );
}

#[test]
fn anthropic_translate_message_start() {
    let payload = r#"message_start
{"type":"message","role":"assistant","content":[],"model":"claude-3","stop_reason":null,"usage":{"input_tokens":10,"output_tokens":0}}"#;
    let chunk = translate_anthropic_sse_payload(payload, "chunk-1", 1000, "claude-3")
        .unwrap()
        .unwrap();
    assert!(!chunk.done);
    assert_eq!(
        chunk.payload["choices"][0]["delta"]["role"]
            .as_str()
            .unwrap(),
        "assistant"
    );
    assert_eq!(chunk.payload["id"].as_str().unwrap(), "chunk-1");
    assert!(
        !chunk.has_content,
        "message_start must have has_content=false"
    );
}

#[test]
fn anthropic_translate_content_block_delta() {
    let payload = r#"content_block_delta
{"delta":{"type":"content_block_delta","text":"Hello"}}"#;
    let chunk = translate_anthropic_sse_payload(payload, "chunk-1", 1000, "claude-3")
        .unwrap()
        .unwrap();
    assert!(!chunk.done);
    assert_eq!(
        chunk.payload["choices"][0]["delta"]["content"]
            .as_str()
            .unwrap(),
        "Hello"
    );
    assert!(
        chunk.has_content,
        "content_block_delta (text) must have has_content=true"
    );
}

#[test]
fn anthropic_translate_message_delta_with_stop() {
    let payload = r#"message_delta
{"delta":{"stop_reason":"end_turn"}}"#;
    let chunk = translate_anthropic_sse_payload(payload, "chunk-1", 1000, "claude-3")
        .unwrap()
        .unwrap();
    assert!(chunk.done);
    assert_eq!(
        chunk.payload["choices"][0]["finish_reason"]
            .as_str()
            .unwrap(),
        "stop"
    );
    assert_eq!(chunk.stop_reason.as_deref(), Some("end_turn"));
    assert!(
        !chunk.has_content,
        "message_delta must have has_content=false"
    );
}

#[test]
fn anthropic_translate_message_delta_max_tokens() {
    let payload = r#"message_delta
{"delta":{"stop_reason":"max_tokens"}}"#;
    let chunk = translate_anthropic_sse_payload(payload, "chunk-1", 1000, "claude-3")
        .unwrap()
        .unwrap();
    assert!(chunk.done);
    assert_eq!(
        chunk.payload["choices"][0]["finish_reason"]
            .as_str()
            .unwrap(),
        "length"
    );
    assert_eq!(chunk.stop_reason.as_deref(), Some("max_tokens"));
    assert!(
        !chunk.has_content,
        "message_delta (max_tokens) must have has_content=false"
    );
}

#[test]
fn anthropic_translate_message_delta_tool_use() {
    let payload = r#"message_delta
{"delta":{"stop_reason":"tool_use"}}"#;
    let chunk = translate_anthropic_sse_payload(payload, "chunk-1", 1000, "claude-3")
        .unwrap()
        .unwrap();
    assert!(chunk.done);
    assert_eq!(
        chunk.payload["choices"][0]["finish_reason"]
            .as_str()
            .unwrap(),
        "tool_calls"
    );
    assert_eq!(chunk.stop_reason.as_deref(), Some("tool_use"));
    assert!(
        !chunk.has_content,
        "message_delta (tool_use) must have has_content=false"
    );
}

#[test]
fn anthropic_translate_message_delta_tool_calling_variants() {
    for reason in [
        "tool_use",
        "tool_call",
        "tool_calls",
        "toolUse",
        "toolCall",
        "toolCalls",
    ] {
        let json = serde_json::json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": reason
            }
        });
        let chunk = translate_anthropic_message_delta(&json, "chunk-1", 1000, "claude-3");
        assert!(chunk.done);
        assert_eq!(
            chunk.payload["choices"][0]["finish_reason"]
                .as_str()
                .unwrap(),
            "tool_calls"
        );
        assert_eq!(chunk.stop_reason.as_deref(), Some(reason));
    }
}

#[test]
fn anthropic_translate_message_delta_root_level_stop_reason() {
    let json = serde_json::json!({
        "type": "message_delta",
        "stop_reason": "tool_use"
    });
    let chunk = translate_anthropic_message_delta(&json, "chunk-1", 1000, "claude-3");
    assert!(chunk.done);
    assert_eq!(
        chunk.payload["choices"][0]["finish_reason"]
            .as_str()
            .unwrap(),
        "tool_calls"
    );
    assert_eq!(chunk.stop_reason.as_deref(), Some("tool_use"));
}

#[test]
fn anthropic_streaming_message_start_extracts_usage() {
    let json_str = r#"{
        "type": "message_start",
        "message": {
            "id": "msg_01X",
            "type": "message",
            "role": "assistant",
            "content": [],
            "model": "claude-test",
            "stop_reason": null,
            "stop_sequence": null,
            "usage": {
                "input_tokens": 450,
                "output_tokens": 1,
                "cache_creation_input_tokens": 50,
                "cache_read_input_tokens": 172
            }
        }
    }"#;
    let data: serde_json::Value = serde_json::from_str(json_str).unwrap();
    let chunk = build_anthropic_message_start_chunk("chunk-1", 1234567890, "claude-test", &data);
    let usage = chunk.usage.expect("message_start must carry usage");
    assert_eq!(usage.prompt_tokens, 672);
    assert_eq!(usage.completion_tokens, 1);
    assert_eq!(usage.total_tokens, 673);
    assert_eq!(
        usage.prompt_tokens_details.and_then(|d| d.cached_tokens),
        Some(172)
    );
}

#[test]
fn anthropic_streaming_message_start_without_usage_emits_none() {
    let json_str = r#"{
        "type": "message_start",
        "message": {
            "id": "msg_01X",
            "model": "claude-test"
        }
    }"#;
    let data: serde_json::Value = serde_json::from_str(json_str).unwrap();
    let chunk = build_anthropic_message_start_chunk("chunk-1", 1234567890, "claude-test", &data);
    assert!(chunk.usage.is_none());
}

#[test]
fn anthropic_streaming_message_delta_classic_preserves_prompt() {
    let json_str = r#"{
        "type": "message_delta",
        "delta": {
            "stop_reason": "end_turn",
            "stop_sequence": null
        },
        "usage": {
            "output_tokens": 89
        }
    }"#;
    let data: serde_json::Value = serde_json::from_str(json_str).unwrap();
    let chunk = translate_anthropic_message_delta(&data, "chunk-1", 1234567890, "claude-test");
    let usage = chunk.usage.expect("message_delta must carry usage");
    // 0 is a sentinel, not a count: it tells merge_usage to keep the
    // message_start prompt count.
    assert_eq!(usage.prompt_tokens, 0);
    assert_eq!(usage.completion_tokens, 89);
    assert_eq!(usage.total_tokens, 0);
}

#[test]
fn anthropic_streaming_message_delta_with_input_tokens_takes_both() {
    let json_str = r#"{
        "type": "message_delta",
        "delta": {
            "stop_reason": "end_turn",
            "stop_sequence": null
        },
        "usage": {
            "input_tokens": 450,
            "cache_read_input_tokens": 172,
            "cache_creation_input_tokens": 0,
            "output_tokens": 89
        }
    }"#;
    let data: serde_json::Value = serde_json::from_str(json_str).unwrap();
    let chunk = translate_anthropic_message_delta(&data, "chunk-1", 1234567890, "claude-test");
    let usage = chunk.usage.expect("message_delta must carry usage");
    assert_eq!(usage.prompt_tokens, 622);
    assert_eq!(usage.completion_tokens, 89);
    assert_eq!(usage.total_tokens, 711);
    assert_eq!(
        usage.prompt_tokens_details.and_then(|d| d.cached_tokens),
        Some(172)
    );
}

#[test]
fn anthropic_streaming_message_delta_without_usage_emits_none() {
    let json_str = r#"{
        "type": "message_delta",
        "delta": {
            "stop_reason": "end_turn"
        }
    }"#;
    let data: serde_json::Value = serde_json::from_str(json_str).unwrap();
    let chunk = translate_anthropic_message_delta(&data, "chunk-1", 1234567890, "claude-test");
    assert!(chunk.usage.is_none());
}

#[test]
fn merge_usage_preserves_prompt_from_zero_sentinel() {
    // A classic message_delta (prompt_tokens and total_tokens at the 0
    // sentinel) must not clobber the counts message_start already
    // carried.
    let existing = OpenAIUsage {
        prompt_tokens: 622,
        completion_tokens: 2,
        total_tokens: 624,
        prompt_tokens_details: Some(PromptTokensDetails {
            cached_tokens: Some(100),
        }),
    };
    let new = OpenAIUsage {
        prompt_tokens: 0,
        completion_tokens: 89,
        total_tokens: 0,
        prompt_tokens_details: None,
    };
    let merged = merge_usage(existing, new);
    assert_eq!(merged.prompt_tokens, 622, "preserved from message_start");
    assert_eq!(merged.completion_tokens, 89, "updated from message_delta");
    assert_eq!(merged.total_tokens, 624, "preserved from message_start");
    assert_eq!(
        merged
            .prompt_tokens_details
            .as_ref()
            .and_then(|d| d.cached_tokens),
        Some(100),
        "details preserved from message_start"
    );
}

#[test]
fn merge_usage_takes_max_of_newer_counts() {
    let start_usage = OpenAIUsage {
        prompt_tokens: 622,
        completion_tokens: 0,
        total_tokens: 622,
        prompt_tokens_details: Some(PromptTokensDetails {
            cached_tokens: Some(172),
        }),
    };
    let delta_usage = OpenAIUsage {
        prompt_tokens: 622,
        completion_tokens: 89,
        total_tokens: 711,
        prompt_tokens_details: Some(PromptTokensDetails {
            cached_tokens: Some(172),
        }),
    };
    let merged = merge_usage(start_usage, delta_usage);
    assert_eq!(merged.prompt_tokens, 622);
    assert_eq!(merged.completion_tokens, 89);
    assert_eq!(merged.total_tokens, 711);
    assert_eq!(
        merged
            .prompt_tokens_details
            .as_ref()
            .and_then(|d| d.cached_tokens),
        Some(172)
    );
}

#[test]
fn anthropic_translate_message_stop() {
    // message_delta already emitted the `done: true` chunk. Emitting
    // one here too would send a duplicate terminal frame downstream.
    let payload = "message_stop\n{}";
    let result = translate_anthropic_sse_payload(payload, "chunk-1", 1000, "claude-3").unwrap();
    assert!(result.is_none());
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
fn merge_usage_with_presence_keeps_only_observed_totals() {
    use openproxy_types::UsagePresence;

    let no_total = UsagePresence {
        prompt: true,
        completion: false,
        total: false,
        cached: false,
    };
    let zero_total = UsagePresence {
        prompt: true,
        completion: true,
        total: true,
        cached: false,
    };

    let first = OpenAIUsage {
        prompt_tokens: 17,
        completion_tokens: 0,
        total_tokens: 0,
        prompt_tokens_details: None,
    };
    let second = OpenAIUsage {
        prompt_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
        prompt_tokens_details: None,
    };

    // Total absent in both snapshots: stays unmeasured, not synthesized.
    let (merged, presence) =
        merge_usage_with_presence(first.clone(), no_total, second.clone(), no_total);
    assert_eq!(merged.total_tokens, 0);
    assert!(!presence.total);

    // An explicit zero is a real observation and wins over absence.
    let (merged, presence) = merge_usage_with_presence(first.clone(), no_total, second, zero_total);
    assert_eq!(merged.total_tokens, 0);
    assert!(presence.total);
    assert_eq!(merged.prompt_tokens, 17);

    // Between two real observations the maximum wins, and a later absent
    // total never erases it.
    let higher = OpenAIUsage {
        total_tokens: 40,
        ..first.clone()
    };
    let lower = OpenAIUsage {
        total_tokens: 12,
        ..first.clone()
    };
    let all_total = UsagePresence::all();
    let (merged, presence) = merge_usage_with_presence(higher, all_total, lower, all_total);
    assert_eq!(merged.total_tokens, 40);
    assert!(presence.total);
    let (merged, presence) = merge_usage_with_presence(merged, presence, first, no_total);
    assert_eq!(merged.total_tokens, 40);
    assert!(presence.total);
}
