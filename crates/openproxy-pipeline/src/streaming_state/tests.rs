use super::normalizer::{ReasoningNormalizer, ToolCallAccumulator};
use crate::streaming::{StreamAction, StreamingChunkStage};

#[test]
fn test_reasoning_normalizer_stage_mutates_think_tags() {
    let mut normalizer = ReasoningNormalizer::new();
    let payload = r#"{"choices":[{"delta":{"content":"<think>reasoning</think>answer"}}]}"#;
    let action = normalizer.process_chunk(payload);
    let StreamAction::Mutate(mutated) = action else {
        panic!("expected StreamAction::Mutate, got {action:?}");
    };
    assert!(mutated.contains("\"reasoning_content\":\"reasoning\""));
    assert!(mutated.contains("\"content\":\"answer\""));
}

#[test]
fn test_reasoning_normalizer_stage_passthrough_clean_chunk() {
    let mut normalizer = ReasoningNormalizer::new();
    let payload = r#"{"choices":[{"delta":{"content":"clean chunk"}}]}"#;
    let action = normalizer.process_chunk(payload);
    assert_eq!(action, StreamAction::Passthrough);
}

#[test]
fn test_tool_call_accumulator_handles_fragments() {
    let mut acc = ToolCallAccumulator::new();
    let f1 = acc.process(0, "{\"location\":");
    assert_eq!(f1, "{\"location\":");
    let f2 = acc.process(0, " \"Paris\"}");
    assert_eq!(f2, " \"Paris\"}");
}

#[test]
fn test_decode_and_record_line_resets_event_type_on_empty_line() {
    let mut state = super::StreamingState::new(false);
    state.current_event_type = Some("content_block_start".to_string());

    // Non-empty line does not reset
    let line1 = b"event: content_block_delta";
    let res1 = super::processor::decode_and_record_line(&mut state, line1);
    assert_eq!(res1, Some("event: content_block_delta"));
    assert_eq!(state.current_event_type.as_deref(), Some("content_block_start"));

    // SSE comment does not reset
    let comment = b": ping";
    let res_comment = super::processor::decode_and_record_line(&mut state, comment);
    assert!(res_comment.is_none());
    assert_eq!(state.current_event_type.as_deref(), Some("content_block_start"));

    // Empty line resets current_event_type
    let empty = b"";
    let res_empty = super::processor::decode_and_record_line(&mut state, empty);
    assert!(res_empty.is_none());
    assert!(state.current_event_type.is_none());

    // Empty line with CRLF resets current_event_type
    state.current_event_type = Some("message_delta".to_string());
    let crlf = b"\r";
    let res_crlf = super::processor::decode_and_record_line(&mut state, crlf);
    assert!(res_crlf.is_none());
    assert!(state.current_event_type.is_none());
}

#[test]
fn test_anthropic_multi_event_stream_with_comments_and_empty_lines() {
    let raw_stream_lines: Vec<&[u8]> = vec![
        b": initial comment before any event",
        b"event: message_start",
        b"data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-3\",\"stop_reason\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}",
        b"", // empty line delimiter
        b": keep-alive between events",
        b"", // consecutive empty line
        b"event: content_block_start",
        b": keep-alive inside event before data",
        b"data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}",
        b"\r", // CRLF empty line delimiter
        b"event: content_block_delta",
        b"data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}",
        b"",
        b": keep-alive",
        b"event: content_block_delta",
        b"data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}",
        b"",
        b"event: content_block_stop",
        b"data: {\"type\":\"content_block_stop\",\"index\":0}",
        b"",
        b": keep-alive",
        b"event: message_delta",
        b"data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}",
        b"",
        b"event: message_stop",
        b"data: {\"type\":\"message_stop\"}",
        b"",
        b": trailing comment",
    ];

    let mut state = super::StreamingState::new(false);
    let mut collected_chunks = Vec::new();
    let mut chunk_counter = 0;

    for line_bytes in raw_stream_lines {
        let decoded = super::processor::decode_and_record_line(&mut state, line_bytes);
        if line_bytes.is_empty() || *line_bytes == b"\r"[..] {
            assert!(
                state.current_event_type.is_none(),
                "empty line must reset current_event_type"
            );
            assert!(decoded.is_none());
            continue;
        }
        if line_bytes.starts_with(b":") {
            assert!(decoded.is_none(), "comment line must return None");
            continue;
        }

        let line = decoded.expect("non-empty non-comment line must be decoded");
        let payload_opt = crate::sse::parse_anthropic_sse_stream_line(line, &mut state.current_event_type)
            .expect("parse_anthropic_sse_stream_line must succeed");

        if let Some(payload) = payload_opt {
            chunk_counter += 1;
            let chunk_id = format!("chunk-{chunk_counter}");
            if let Some(chunk) = crate::sse::translate_anthropic_sse_event(
                &payload,
                &chunk_id,
                1000,
                "claude-3",
                &mut state.tool_use_acc,
                &mut state.tool_call_index_counter,
            )
            .expect("translation must succeed")
            {
                collected_chunks.push(chunk);
            }
        }
    }

    // Expected chunks:
    // 1: message_start -> role announcement (has_content = false)
    // 2: content_block_delta -> "Hello" (has_content = true)
    // 3: content_block_delta -> " world" (has_content = true)
    // 4: message_delta -> finish_reason "stop", done: true
    assert_eq!(collected_chunks.len(), 4, "must collect exactly 4 translated chunks");
    assert_eq!(
        collected_chunks[0].payload["choices"][0]["delta"]["role"].as_str().unwrap(),
        "assistant"
    );
    assert_eq!(
        collected_chunks[1].payload["choices"][0]["delta"]["content"].as_str().unwrap(),
        "Hello"
    );
    assert_eq!(
        collected_chunks[2].payload["choices"][0]["delta"]["content"].as_str().unwrap(),
        " world"
    );
    assert!(collected_chunks[3].done);
    assert_eq!(
        collected_chunks[3].payload["choices"][0]["finish_reason"].as_str().unwrap(),
        "stop"
    );
    assert_eq!(
        collected_chunks[3].usage.as_ref().unwrap().completion_tokens,
        5
    );
    assert!(state.current_event_type.is_none(), "state must be clean after stream ends");
}

