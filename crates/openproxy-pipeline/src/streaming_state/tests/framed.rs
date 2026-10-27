use super::*;
use bytes::Bytes;

#[tokio::test]
async fn passthrough_keeps_storage_but_normalization_and_pii_still_mutate() {
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    let sink = crate::race_sink::StreamSink::direct(tx);
    let harness = TestStreamHarness::new(&sink);
    let mut state = super::super::StreamingState::new(true);
    let mut processor = super::super::ChunkProcessor {
        state: &mut state,
        dispatcher: &harness.pipeline.dispatcher,
    };
    let payload = r#"{"choices":[{"delta":{"content":"hola 🌍"}}]}"#;
    for delimiter in ["\n\n", "\r\n\r\n"] {
        let wire = Bytes::from(format!("data: {payload}{delimiter}"));
        let frame = processor.prepare_content_chunk_bytes(payload, &wire);
        assert_eq!(frame, wire);
        assert_eq!(frame.as_ptr(), wire.as_ptr());
    }
    let wire = Bytes::from(format!("data: {payload}\n"));
    let frame = processor.prepare_content_chunk_bytes(payload, &wire);
    assert_eq!(frame, crate::sse::build_sse_frame(payload));

    let thinking = r#"{"choices":[{"delta":{"content":"<think>razón</think>respuesta"}}]}"#;
    let wire = Bytes::from(format!("data: {thinking}\n\n"));
    let frame = processor.prepare_content_chunk_bytes(thinking, &wire);
    let frame_text = std::str::from_utf8(&frame).unwrap();
    assert!(frame_text.contains("\"reasoning_content\":\"razón\""));
    assert!(frame_text.contains("\"content\":\"respuesta\""));
    assert_ne!(frame.as_ptr(), wire.as_ptr());

    processor.state.pii_stage = Some(crate::pii::PiiRestorationStage::from_mapping(
        [("[EMAIL_1]".into(), "alice@example.com".into())].into(),
    ));
    let payload = r#"{"choices":[{"delta":{"content":"[EMAIL_1]"}}]}"#;
    let wire = Bytes::from(format!("data: {payload}\n\n"));
    let frame = processor.prepare_content_chunk_bytes(payload, &wire);
    assert!(
        std::str::from_utf8(&frame)
            .unwrap()
            .contains("alice@example.com")
    );
    assert!(!std::str::from_utf8(&frame).unwrap().contains("[EMAIL_1]"));
    assert_ne!(frame.as_ptr(), wire.as_ptr());
    assert!(processor.state.acc.is_some());
}

#[test]
fn transport_splits_preserve_recording_and_decoded_payloads() {
    let wire =
        "data: {\"choices\":[{\"delta\":{\"content\":\"🌍ñ\"}}]}\r\n\r\ndata: [DONE]\r\n\r\n";
    let mut reference = super::super::StreamingState::new(true);
    let expected: Vec<_> = wire
        .lines()
        .filter_map(|line| {
            super::super::processor::decode_and_record_line(&mut reference, line.as_bytes())
                .map(str::to_owned)
        })
        .collect();
    for step in 1..=wire.len() {
        let mut parser = crate::sse::SseParser::new(4096);
        let mut state = super::super::StreamingState::new(true);
        let mut actual = Vec::new();
        for chunk in wire.as_bytes().chunks(step) {
            parser.push(chunk).unwrap();
            while let Some(frame) = parser.next_openai_frame() {
                if let Some(line) =
                    super::super::processor::decode_and_record_line(&mut state, &frame)
                {
                    actual.push(line.to_owned());
                }
            }
        }
        assert_eq!(actual, expected, "step={step}");
        assert_eq!(
            state.acc.as_ref().unwrap().raw_response_body(),
            reference.acc.as_ref().unwrap().raw_response_body(),
            "recording differs at step={step}"
        );
        assert!(parser.is_empty());
    }
}
