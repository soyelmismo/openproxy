use super::super::*;

// ---- H5 fix: Anthropic tool_use accumulator ----

#[test]
fn anthropic_tool_use_start_emits_id_and_name() {
    // The content_block_start event for a tool_use block must
    // emit an OpenAI-shaped chunk with `tool_calls[0]` carrying
    // the id, type=function, and name. The arguments field is
    // empty at this point because the JSON body is delivered
    // in subsequent content_block_delta events.
    let payload = r#"content_block_start
{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_01ABC","name":"get_weather","input":{}}}"#;
    let mut acc: Option<AnthropicToolUseAccumulator> = None;
    let mut counter: u32 = 0;
    let chunk =
        translate_anthropic_sse_event(payload, "chunk-1", 1000, "claude-3", &mut acc, &mut counter)
            .unwrap()
            .unwrap();
    assert!(!chunk.done);
    let tool_call = &chunk.payload["choices"][0]["delta"]["tool_calls"][0];
    assert_eq!(tool_call["index"].as_u64().unwrap(), 0);
    assert_eq!(tool_call["id"].as_str().unwrap(), "toolu_01ABC");
    assert_eq!(tool_call["type"].as_str().unwrap(), "function");
    assert_eq!(
        tool_call["function"]["name"].as_str().unwrap(),
        "get_weather"
    );
    assert_eq!(tool_call["function"]["arguments"].as_str().unwrap(), "");
    // The accumulator must be open after start.
    assert!(acc.is_some());
    assert_eq!(acc.as_ref().unwrap().id, "toolu_01ABC");
    assert_eq!(acc.as_ref().unwrap().name, "get_weather");
    // Index counter is monotonically increasing.
    assert_eq!(counter, 1);
    // Mirroring in delta_tool_calls must also be populated so
    // the downstream pipeline accumulator picks it up.
    assert_eq!(chunk.delta_tool_calls.len(), 1);
    assert_eq!(
        chunk.delta_tool_calls[0]["id"].as_str().unwrap(),
        "toolu_01ABC"
    );
    assert_eq!(
        chunk.delta_tool_calls[0]["function"]["name"]
            .as_str()
            .unwrap(),
        "get_weather"
    );
    assert_eq!(
        chunk.delta_tool_calls[0]["function"]["arguments"]
            .as_str()
            .unwrap(),
        ""
    );
    // content_block_start for tool_use is metadata-only (no tokens yet).
    assert!(
        !chunk.has_content,
        "content_block_start (tool_use) must have has_content=false"
    );
}

#[test]
fn anthropic_tool_use_input_json_delta_accumulates() {
    let mut acc = Some(
        AnthropicToolUseAccumulator::new_with_bounds(
            0,
            "toolu_01ABC".to_string(),
            "get_weather".to_string(),
        )
        .unwrap(),
    );
    let mut counter: u32 = 1;

    // Fragment 1: `{"location":`
    let p1 = r#"content_block_delta
{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"location\":"}}"#;
    let chunk1 =
        translate_anthropic_sse_event(p1, "chunk-1", 1000, "claude-3", &mut acc, &mut counter)
            .unwrap()
            .unwrap();
    assert!(!chunk1.done);
    assert_eq!(
        chunk1.payload["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap(),
        "{\"location\":"
    );
    assert_eq!(
        chunk1.delta_tool_calls[0]["function"]["arguments"]
            .as_str()
            .unwrap(),
        "{\"location\":"
    );
    assert_eq!(
        chunk1.payload["choices"][0]["delta"]["tool_calls"][0]["index"]
            .as_u64()
            .unwrap(),
        0
    );
    // Fragment 1 carries real content (partial JSON arguments).
    assert!(
        chunk1.has_content,
        "input_json_delta chunk 1 must have has_content=true"
    );

    // Fragment 2: ` "San Francisco"}`
    let p2 = r#"content_block_delta
{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":" \"San Francisco\"}"}}"#;
    let chunk2 =
        translate_anthropic_sse_event(p2, "chunk-2", 1000, "claude-3", &mut acc, &mut counter)
            .unwrap()
            .unwrap();
    assert!(!chunk2.done);
    // CRITICAL: chunk2 must carry ONLY the new fragment, NOT the
    // whole running arguments total.
    assert_eq!(
        chunk2.payload["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap(),
        " \"San Francisco\"}"
    );
    assert_eq!(
        chunk2.delta_tool_calls[0]["function"]["arguments"]
            .as_str()
            .unwrap(),
        " \"San Francisco\"}"
    );
    // Fragment 2 carries real content (partial JSON arguments).
    assert!(
        chunk2.has_content,
        "input_json_delta chunk 2 must have has_content=true"
    );

    // The accumulator internally tracks the full accumulated string
    // for validation or debugging.
    assert_eq!(
        acc.as_ref().unwrap().arguments,
        "{\"location\": \"San Francisco\"}"
    );
}

#[test]
fn anthropic_tool_use_block_stop_clears_accumulator() {
    let mut acc = Some(
        AnthropicToolUseAccumulator::new_with_bounds(
            0,
            "toolu_01ABC".to_string(),
            "get_weather".to_string(),
        )
        .unwrap(),
    );
    let mut counter: u32 = 1;

    let payload = r#"content_block_stop
{"type":"content_block_stop","index":1}"#;
    let chunk =
        translate_anthropic_sse_event(payload, "chunk-1", 1000, "claude-3", &mut acc, &mut counter)
            .unwrap();
    // content_block_stop emits no chunk (the downstream pipeline
    // relies on the next message_delta or stream end to flush).
    assert!(chunk.is_none());
    // The accumulator must be cleared.
    assert!(acc.is_none());
}

#[test]
fn anthropic_text_block_passthrough_does_not_open_accumulator() {
    // Text blocks (the most common case) must not touch the
    // tool_use accumulator. The content_block_start for a
    // text block returns None (no chunk) and the
    // content_block_delta with text_delta reuses the same
    // emission path as the stateless translator.
    let start = r#"content_block_start
{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
    let delta = r#"content_block_delta
{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}"#;
    let mut acc: Option<AnthropicToolUseAccumulator> = None;
    let mut counter: u32 = 0;
    let start_chunk =
        translate_anthropic_sse_event(start, "chunk-1", 1000, "claude-3", &mut acc, &mut counter)
            .unwrap();
    assert!(start_chunk.is_none());
    assert!(acc.is_none());
    let delta_chunk =
        translate_anthropic_sse_event(delta, "chunk-2", 1000, "claude-3", &mut acc, &mut counter)
            .unwrap()
            .unwrap();
    assert_eq!(
        delta_chunk.payload["choices"][0]["delta"]["content"]
            .as_str()
            .unwrap(),
        "hello"
    );
    assert!(acc.is_none());
}

#[test]
fn anthropic_input_json_delta_without_open_accumulator_is_dropped() {
    // Malformed stream defense: an input_json_delta arriving without
    // a prior tool_use content_block_start must not panic and must
    // not emit a phantom tool_calls chunk.
    let mut acc: Option<AnthropicToolUseAccumulator> = None;
    let mut counter: u32 = 0;

    let payload = "content_block_delta\n{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"foo\\\":1}\"}}";
    let chunk =
        translate_anthropic_sse_event(payload, "chunk-1", 1000, "claude-3", &mut acc, &mut counter)
            .unwrap();
    assert!(chunk.is_none());
}

#[test]
fn anthropic_message_start_still_works_via_stateful_translator() {
    let mut acc: Option<AnthropicToolUseAccumulator> = None;
    let mut counter: u32 = 0;

    let payload = r#"message_start
{"type":"message","role":"assistant","content":[],"model":"claude-3","stop_reason":null,"usage":{"input_tokens":10,"output_tokens":0}}"#;
    let chunk =
        translate_anthropic_sse_event(payload, "chunk-1", 1000, "claude-3", &mut acc, &mut counter)
            .unwrap()
            .unwrap();
    assert_eq!(
        chunk.payload["choices"][0]["delta"]["role"]
            .as_str()
            .unwrap(),
        "assistant"
    );
}
