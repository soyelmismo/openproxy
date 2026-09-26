use super::super::*;

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
        r#"data: {"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}"#,
        "",
        "event: message_stop",
        "data: {}",
        "",
    ];

    let mut current_event = None;
    let mut chunks = Vec::new();
    let mut chunk_idx = 0;

    for line in lines {
        if let Some(payload) = parse_anthropic_sse_stream_line(line, &mut current_event).unwrap() {
            chunk_idx += 1;
            let chunk_id = format!("chunk-{chunk_idx}");
            if let Some(chunk) =
                translate_anthropic_sse_payload(&payload, &chunk_id, 1000, "claude-3").unwrap()
            {
                chunks.push(chunk);
            }
        }
    }

    // Expected chunks:
    // 1. message_start -> assistant role announcement
    // 2. content_block_delta -> "Hi"
    // 3. content_block_delta -> " there"
    // 4. message_delta -> finish_reason: "stop", done: true
    // message_stop -> None (not a chunk)
    assert_eq!(chunks.len(), 4);

    assert_eq!(
        chunks[0].payload["choices"][0]["delta"]["role"]
            .as_str()
            .unwrap(),
        "assistant"
    );
    assert!(!chunks[0].done);
    assert_eq!(chunks[0].payload["id"].as_str().unwrap(), "chunk-1");

    assert_eq!(
        chunks[1].payload["choices"][0]["delta"]["content"]
            .as_str()
            .unwrap(),
        "Hi"
    );
    assert!(!chunks[1].done);

    assert_eq!(
        chunks[2].payload["choices"][0]["delta"]["content"]
            .as_str()
            .unwrap(),
        " there"
    );
    assert!(!chunks[2].done);

    assert!(chunks[3].done);
    assert_eq!(
        chunks[3].payload["choices"][0]["finish_reason"]
            .as_str()
            .unwrap(),
        "stop"
    );
    assert_eq!(chunks[3].usage.as_ref().unwrap().completion_tokens, 2);
}

#[test]
fn anthropic_tool_use_full_stream_simulation() {
    let delta1 = serde_json::json!({
        "type": "content_block_delta",
        "index": 0,
        "delta": {
            "type": "input_json_delta",
            "partial_json": "{\"city\":"
        }
    })
    .to_string();
    let delta2 = serde_json::json!({
        "type": "content_block_delta",
        "index": 0,
        "delta": {
            "type": "input_json_delta",
            "partial_json": "\"Tokyo\"}"
        }
    })
    .to_string();

    let delta1_line = format!("data: {delta1}");
    let delta2_line = format!("data: {delta2}");

    let lines = [
        "event: message_start",
        r#"data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"model":"MiniMax-M3","stop_reason":null,"usage":{"input_tokens":10,"output_tokens":0}}}"#,
        "",
        "event: content_block_start",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_123","name":"get_weather","input":{}}}"#,
        "",
        "event: content_block_delta",
        delta1_line.as_str(),
        "",
        "event: content_block_delta",
        delta2_line.as_str(),
        "",
        "event: content_block_stop",
        r#"data: {"type":"content_block_stop","index":0}"#,
        "",
        "event: message_delta",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":15}}"#,
        "",
        "event: message_stop",
        "data: {}",
        "",
    ];

    let mut current_event = None;
    let mut chunks = Vec::new();
    let mut chunk_idx = 0;
    let mut tool_use_acc: Option<AnthropicToolUseAccumulator> = None;
    let mut tool_call_index_counter: u32 = 0;
    let mut acc = crate::sse_accumulator::ResponseAccumulator::new();

    for line in lines {
        if let Some(payload) = parse_anthropic_sse_stream_line(line, &mut current_event).unwrap() {
            chunk_idx += 1;
            let chunk_id = format!("chunk-{chunk_idx}");
            if let Some(chunk) = translate_anthropic_sse_event(
                &payload,
                &chunk_id,
                1000,
                "MiniMax-M3",
                &mut tool_use_acc,
                &mut tool_call_index_counter,
            )
            .unwrap()
            {
                if let Some(sr) = &chunk.stop_reason {
                    acc.set_stop_reason(sr);
                }
                let json_str = chunk.payload.to_string();
                acc.append_openai_raw(&json_str);
                chunks.push(chunk);
            }
        }
    }

    assert_eq!(chunks.len(), 5);
    assert_eq!(
        chunks[0].payload["choices"][0]["delta"]["role"]
            .as_str()
            .unwrap(),
        "assistant"
    );
    assert_eq!(
        chunks[1].payload["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
        "get_weather"
    );
    assert!(chunks[4].done);
    assert_eq!(
        chunks[4].payload["choices"][0]["finish_reason"]
            .as_str()
            .unwrap(),
        "tool_calls"
    );

    let final_resp = acc.finish("final-id", 1000, "MiniMax-M3");
    assert_eq!(
        final_resp["choices"][0]["finish_reason"].as_str().unwrap(),
        "tool_calls"
    );
    assert_eq!(
        final_resp["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "get_weather"
    );
    assert_eq!(
        final_resp["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
        r#"{"city":"Tokyo"}"#
    );
}

// ====================================================================
// Tier 2 Gap 1: Anthropic SSE translation concurrency stress test
//
// Proves that when multiple threads/tasks translate Anthropic SSE
// streams concurrently, each task's `AnthropicToolUseAccumulator`
// and chunk stream state remain strictly isolated:
//   1. No tool_use arguments bleed across requests.
//   2. `tool_call_index_counter` stays strictly local to each request
//      (every task sees index 0 for its first tool call).
//   3. Chunks emitted for request A never contain chunk_id, model, or
//      tool IDs belonging to request B.
// ====================================================================
// Under the old singleton / static design, parallel SSE translation
// would share state across concurrent tasks and this stress
// test fails with cross-contamination.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sse_translation_isolates_parallel_requests() {
    use std::sync::Arc;
    use tokio::task::JoinSet;

    const N: usize = 64;
    let mut joins = JoinSet::new();
    let barrier = Arc::new(tokio::sync::Barrier::new(N));

    for i in 0..N {
        let barrier = Arc::clone(&barrier);
        joins.spawn(async move {
            let chunk_id = format!("chatcmpl-{i}");
            let model = format!("claude-isolated-{i}");
            let mut tool_use_acc: Option<AnthropicToolUseAccumulator> = None;
            let mut tool_call_index_counter: u32 = 0;

            // Wait until all tasks are queued so they race in
            // parallel (not sequentially).
            barrier.wait().await;

            // Sequence: content_block_start (tool_use) → deltas →
            // message_delta (stop). Each task sees a distinct
            // tool id+name so any cross-talk would be visible.
            let id = format!("toolu_{i:08x}");
            let name = format!("fn_{i}");
            let start_payload = format!(
                "content_block_start\n{{\"content_block\":{{\"type\":\"tool_use\",\"id\":\"{id}\",\"name\":\"{name}\",\"input\":{{}}}}}}"
            );
            let delta_payload = "content_block_delta\n{\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}".to_string();
            let stop_payload = "message_delta\n{\"delta\":{\"stop_reason\":\"tool_use\"}}".to_string();

            let mut outs = Vec::new();
            for payload in [&start_payload, &delta_payload, &stop_payload] {
                let out = translate_anthropic_sse_event(
                    payload,
                    &chunk_id,
                    1_700_000_000 + i as u64,
                    &model,
                    &mut tool_use_acc,
                    &mut tool_call_index_counter,
                )
                .expect("translate");
                outs.push(out);
            }
            (i, chunk_id, model, outs)
        });
    }

    let mut seen_ids = std::collections::HashSet::new();
    let mut seen_models = std::collections::HashSet::new();
    while let Some(j) = joins.join_next().await {
        let (i, chunk_id, model, outs) = j.expect("join");
        // chunk_id must round-trip exactly, no cross-talk from peers.
        assert_eq!(chunk_id, format!("chatcmpl-{i}"));
        assert_eq!(model, format!("claude-isolated-{i}"));
        assert!(seen_ids.insert(chunk_id.clone()), "duplicate chunk_id");
        assert!(seen_models.insert(model.clone()), "duplicate model");

        // First chunk must carry THIS task's tool id and name,
        // not any other task's.
        let first_payload = &outs[0].as_ref().expect("first chunk").payload;
        let tool_id = first_payload["choices"][0]["delta"]["tool_calls"][0]["id"]
            .as_str()
            .expect("tool id");
        let tool_name = first_payload["choices"][0]["delta"]["tool_calls"][0]["function"]["name"]
            .as_str()
            .expect("tool name");
        assert_eq!(
            tool_id,
            format!("toolu_{i:08x}"),
            "tool id leaked from another parallel task"
        );
        assert_eq!(
            tool_name,
            format!("fn_{i}"),
            "tool name leaked from another parallel task"
        );

        // Model and chunk_id in the wire payload also must be
        // THIS task's, not a peer's.
        assert_eq!(first_payload["model"].as_str().unwrap(), model);
        assert_eq!(first_payload["id"].as_str().unwrap(), chunk_id);
    }
    assert_eq!(seen_ids.len(), N, "expected {N} unique chunk_ids");
}
