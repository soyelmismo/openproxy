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

    let line1 = b"event: content_block_delta";
    let res1 = super::processor::decode_and_record_line(&mut state, line1);
    assert_eq!(res1, Some("event: content_block_delta"));
    assert_eq!(
        state.current_event_type.as_deref(),
        Some("content_block_start")
    );

    let comment = b": ping";
    let res_comment = super::processor::decode_and_record_line(&mut state, comment);
    assert!(res_comment.is_none());
    assert_eq!(
        state.current_event_type.as_deref(),
        Some("content_block_start")
    );

    let empty = b"";
    let res_empty = super::processor::decode_and_record_line(&mut state, empty);
    assert!(res_empty.is_none());
    assert!(state.current_event_type.is_none());

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
        let payload_opt =
            crate::sse::parse_anthropic_sse_stream_line(line, &mut state.current_event_type)
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

    // message_start, two content_block_delta and message_delta.
    assert_eq!(
        collected_chunks.len(),
        4,
        "must collect exactly 4 translated chunks"
    );
    assert_eq!(
        collected_chunks[0].payload["choices"][0]["delta"]["role"]
            .as_str()
            .unwrap(),
        "assistant"
    );
    assert_eq!(
        collected_chunks[1].payload["choices"][0]["delta"]["content"]
            .as_str()
            .unwrap(),
        "Hello"
    );
    assert_eq!(
        collected_chunks[2].payload["choices"][0]["delta"]["content"]
            .as_str()
            .unwrap(),
        " world"
    );
    assert!(collected_chunks[3].done);
    assert_eq!(
        collected_chunks[3].payload["choices"][0]["finish_reason"]
            .as_str()
            .unwrap(),
        "stop"
    );
    assert_eq!(
        collected_chunks[3]
            .usage
            .as_ref()
            .unwrap()
            .completion_tokens,
        5
    );
    assert!(
        state.current_event_type.is_none(),
        "state must be clean after stream ends"
    );
}

fn setup_test_harness(
    sink: &crate::race_sink::StreamSink,
) -> (
    crate::Pipeline,
    openproxy_types::combos::Combo,
    openproxy_types::combos::ComboTarget,
    openproxy_types::models::Model,
    crate::PipelineRequest,
    crate::timeouts::Timeouts,
) {
    let (pool, conn_arc, _path) = crate::test_utils::fresh_pool();
    let master_key = std::sync::Arc::new(openproxy_db::secrets::MasterKey::generate().unwrap());
    let config = crate::test_utils::test_config(std::sync::Arc::clone(&master_key));
    let pipeline = crate::test_utils::test_pipeline_with_pool(pool, config);

    let (combo_id, _account_id) = {
        let conn = conn_arc.lock();
        crate::test_utils::seed_solo_combo_at_url(
            &conn,
            "prov-test",
            "https://example.com",
            &master_key,
        )
    };

    let combo = openproxy_types::combos::Combo {
        id: combo_id,
        name: "test-combo".to_string(),
        strategy: openproxy_types::combos::Strategy::Priority,
        race_size: 1,
        ..Default::default()
    };

    let target = openproxy_types::combos::ComboTarget {
        id: openproxy_types::ids::ComboTargetId(1),
        combo_id,
        provider_id: openproxy_types::ids::ProviderId::new("prov-test"),
        model_row_id: Some(openproxy_types::ids::ModelRowId(1)),
        priority_order: 1,
        ..Default::default()
    };

    let model = openproxy_types::models::Model {
        row_id: openproxy_types::ids::ModelRowId(1),
        provider_id: openproxy_types::ids::ProviderId::new("prov-test"),
        model_id: openproxy_types::ids::ModelId::new("m"),
        target_format: openproxy_types::TargetFormat::Openai,
        active: true,
        ..Default::default()
    };

    let (mut req, _rx) = crate::test_utils::make_request(combo_id);
    req.stream_sink = Some(sink.clone());

    let timeouts =
        crate::timeouts::Timeouts::from_config(&openproxy_types::config::TimeoutsConfig::default());

    (pipeline, combo, target, model, req, timeouts)
}

struct TestStreamHarness {
    pipeline: crate::Pipeline,
    combo: openproxy_types::combos::Combo,
    target: openproxy_types::combos::ComboTarget,
    model: openproxy_types::models::Model,
    req: crate::PipelineRequest,
    timeouts: crate::timeouts::Timeouts,
    started: std::time::Instant,
}

impl TestStreamHarness {
    fn new(sink: &crate::race_sink::StreamSink) -> Self {
        let (pipeline, combo, target, model, req, timeouts) = setup_test_harness(sink);
        Self {
            pipeline,
            combo,
            target,
            model,
            req,
            timeouts,
            started: std::time::Instant::now(),
        }
    }

    fn context<'a>(&'a self, sink: &'a crate::race_sink::StreamSink) -> super::StreamContext<'a> {
        super::StreamContext {
            req: &self.req,
            combo: &self.combo,
            target: &self.target,
            model: &self.model,
            target_format: openproxy_types::TargetFormat::Openai,
            sink,
            trace_id: "test-trace",
            chunk_id: "test-chunk",
            model_name: "m",
            started: self.started,
            attempt: 1,
            race_size: 1,
            created: 1000,
            connect_and_send_ms: 5,
            resolved_timeouts: &self.timeouts,
            proxy_url: None,
            proxy_status: None,
        }
    }
}

#[tokio::test]
async fn test_finish_chat_stream_terminal_done_received() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let sink = crate::race_sink::StreamSink::Direct(tx);
    let harness = TestStreamHarness::new(&sink);
    let ctx = harness.context(&sink);
    let mut state = super::StreamingState::new(false);
    assert!(!state.done_sent);

    let mut processor = super::processor::ChunkProcessor {
        state: &mut state,
        dispatcher: &harness.pipeline.dispatcher,
    };

    let res = processor.finish_chat_stream(&ctx).await;
    assert!(matches!(res, Ok(crate::streaming::ChunkEvent::Done)));
    assert!(processor.state.done_sent);

    let frame = rx.recv().await.expect("must receive terminal frame");
    assert_eq!(frame, crate::SSE_DONE_BYTES);
    assert!(
        rx.try_recv().is_err(),
        "must receive exactly 1 terminal frame"
    );
}

#[tokio::test]
async fn test_finish_chat_stream_flushes_pii_prior_to_terminal_done() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let sink = crate::race_sink::StreamSink::Direct(tx);
    let harness = TestStreamHarness::new(&sink);
    let ctx = harness.context(&sink);
    let mut state = super::StreamingState::new(false);

    let mut mapping = std::collections::HashMap::new();
    mapping.insert("[EMAIL_1]".to_string(), "alice@example.com".to_string());
    let mut pii = crate::pii::PiiRestorationStage::from_mapping(mapping);
    let partial_chunk = r#"{"choices":[{"delta":{"content":"prefix [EMAIL_"}}]}"#;
    pii.process_chunk(partial_chunk);
    state.pii_stage = Some(pii);

    let mut processor = super::processor::ChunkProcessor {
        state: &mut state,
        dispatcher: &harness.pipeline.dispatcher,
    };

    let res = processor.finish_chat_stream(&ctx).await;
    assert!(matches!(res, Ok(crate::streaming::ChunkEvent::Done)));
    assert!(processor.state.done_sent);

    let frame1 = rx.recv().await.expect("must receive flushed PII frame");
    let frame1_str = std::str::from_utf8(&frame1).expect("valid utf8");
    assert!(frame1_str.starts_with("data: "));
    assert!(frame1_str.ends_with("\n\n"));
    assert!(frame1_str.contains("[EMAIL_"));

    let frame2 = rx.recv().await.expect("must receive terminal frame");
    assert_eq!(frame2, crate::SSE_DONE_BYTES);
    assert!(rx.try_recv().is_err(), "must receive exactly 2 frames");
}

#[tokio::test]
async fn test_finish_chat_stream_sink_lost_preserves_done_sent_false() {
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    let (race_sink, _tokens) = crate::race_sink::RaceSink::new(tx, 2);
    let winner = race_sink.handle(0);
    winner
        .send(bytes::Bytes::from_static(b"winner-first"))
        .await
        .expect("winner claim");

    let loser = race_sink.handle(1);
    let sink = crate::race_sink::StreamSink::Race(loser);

    let harness = TestStreamHarness::new(&sink);
    let ctx = harness.context(&sink);
    let mut state = super::StreamingState::new(false);
    assert!(!state.done_sent);

    let mut processor = super::processor::ChunkProcessor {
        state: &mut state,
        dispatcher: &harness.pipeline.dispatcher,
    };

    let res = processor.finish_chat_stream(&ctx).await;
    let Ok(crate::streaming::ChunkEvent::Return(boxed_res)) = res else {
        panic!("expected ChunkEvent::Return on StreamSinkError::Lost");
    };
    assert!(matches!(
        boxed_res.error,
        Some(openproxy_types::error::CoreError::RaceLost)
    ));
    assert!(
        !processor.state.done_sent,
        "done_sent must remain false when sink returned Lost"
    );
}
