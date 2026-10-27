use super::normalizer::{ReasoningNormalizer, ToolCallAccumulator};
use crate::streaming::{StreamAction, StreamingChunkStage};

mod fast_paths;
mod framed;

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
    let sink = crate::race_sink::StreamSink::direct(tx);
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
    let sink = crate::race_sink::StreamSink::direct(tx);
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

#[cfg(test)]
mod reasoning_instrumentation_tests {
    use super::setup_test_harness;
    use crate::race_sink::StreamSink;
    use crate::streaming_state::processor::ChunkProcessor;
    use crate::streaming_state::{StreamContext, StreamingState};
    use openproxy_types::TargetFormat;

    /// Líneas SSE crudas, una por elemento, exactamente en la forma que
    /// entregan `SseParser::next_line` + `decode_and_record_line` al
    /// pipeline: `event: X` y `data: {...}` por separado, más el
    /// delimitador en blanco que emite Anthropic entre eventos.
    ///
    /// `record_reasoning_progress` es la función real del pipeline; aquí se
    /// la llama con la misma secuencia de líneas que recibiría en
    /// producción, no con una forma combinada inventada.
    fn run_lines(lines: &[&str]) -> StreamingState {
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let sink = StreamSink::direct(tx);
        let (pipeline, combo, target, model, req, timeouts) = setup_test_harness(&sink);
        let started = std::time::Instant::now();
        let ctx = StreamContext {
            req: &req,
            combo: &combo,
            target: &target,
            model: &model,
            target_format: TargetFormat::Anthropic,
            sink: &sink,
            trace_id: "trace-hidden",
            chunk_id: "chunk-hidden",
            model_name: "claude-test",
            started,
            attempt: 1,
            race_size: 1,
            created: 1000,
            connect_and_send_ms: 5,
            resolved_timeouts: &timeouts,
            proxy_url: None,
            proxy_status: None,
        };
        let mut state = StreamingState::new(false);
        {
            let mut processor = ChunkProcessor {
                state: &mut state,
                dispatcher: &pipeline.dispatcher,
            };
            for line in lines {
                processor.record_reasoning_progress(&ctx, line);
            }
        }
        state
    }

    /// Un evento `thinking_delta` con `thinking` vacío y estimación
    /// positiva: progreso oculto que el traductor convierte en liveness.
    const EV_HIDDEN: &str = "event: content_block_delta";
    const DATA_HIDDEN: &str = "data: {\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"\",\"estimated_tokens\":120}}";
    /// Razonamiento visible: `thinking` con texto, que sí se reenvía.
    const EV_VISIBLE: &str = "event: content_block_delta";
    const DATA_VISIBLE: &str = "data: {\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"razonando\",\"estimated_tokens\":120}}";
    /// Texto normal: grupo de control.
    const EV_TEXT: &str = "event: content_block_delta";
    const DATA_TEXT: &str =
        "data: {\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"respuesta\"}}";
    /// Keepalive de Anthropic: no es actividad de razonamiento.
    const EV_PING: &str = "event: ping";
    const DATA_PING: &str = "data: {\"type\":\"ping\"}";

    /// Evento completo en la forma real: evento, dato y delimitador en
    /// blanco que emite Anthropic después de cada evento.
    fn hidden_block(json: &str) -> [String; 3] {
        [
            EV_HIDDEN.to_string(),
            format!("data: {json}"),
            String::new(),
        ]
    }

    #[test]
    fn hidden_thinking_increments_counters_metadata_only() {
        let state = run_lines(&[
            EV_HIDDEN,
            DATA_HIDDEN,
            "",
            EV_HIDDEN,
            DATA_HIDDEN,
            "",
            EV_TEXT,
            DATA_TEXT,
            "",
        ]);
        assert_eq!(state.reasoning_progress.hidden_events, 2);
        assert_eq!(state.reasoning_progress.visible_reasoning_events, 0);
        assert_eq!(state.reasoning_progress.text_delta_events, 1);
        // Suma de estimaciones positivas válidas.
        assert_eq!(state.reasoning_progress.estimated_tokens_sum, 240);
        assert_eq!(state.reasoning_progress.first_block_index, Some(0));
        assert!(state.reasoning_progress.saw_reasoning());
    }

    #[test]
    fn visible_reasoning_is_tracked_separately() {
        let state = run_lines(&[EV_VISIBLE, DATA_VISIBLE, ""]);
        assert_eq!(state.reasoning_progress.hidden_events, 0);
        assert_eq!(state.reasoning_progress.visible_reasoning_events, 1);
        assert!(state.reasoning_progress.saw_reasoning());
    }

    #[test]
    fn negative_estimates_do_not_count_as_hidden() {
        let zero = hidden_block(
            r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"","estimated_tokens":0}}"#,
        );
        let negative = hidden_block(
            r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"","estimated_tokens":-3}}"#,
        );
        let none = hidden_block(r#"{"index":0,"delta":{"type":"thinking_delta","thinking":""}}"#);
        let state = run_lines(&[
            &zero[0],
            &zero[1],
            &zero[2],
            &negative[0],
            &negative[1],
            &negative[2],
            &none[0],
            &none[1],
            &none[2],
        ]);
        assert_eq!(state.reasoning_progress.hidden_events, 0);
        assert_eq!(state.reasoning_progress.estimated_tokens_sum, 0);
        assert!(!state.reasoning_progress.saw_reasoning());
    }

    #[test]
    fn malformed_and_unknown_lines_are_ignored() {
        // JSON malformado bajo el evento correcto: el probe falla en
        // silencio sin entrar en pánico.
        let malformed = [
            EV_HIDDEN.to_string(),
            "data: {\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":}}".to_string(),
            String::new(),
        ];
        // Evento distinto de `content_block_delta`: el traductor tampoco lo
        // clasificaría como delta de contenido.
        let message_start = [
            "event: message_start".to_string(),
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"role\":\"assistant\",\"content\":[],\"model\":\"c\",\"stop_reason\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}".to_string(),
            String::new(),
        ];
        // `input_json_delta`: no aporta al probe.
        let input_json = [
            EV_HIDDEN.to_string(),
            "data: {\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}"
                .to_string(),
            String::new(),
        ];
        let state = run_lines(&[
            &malformed[0],
            &malformed[1],
            &malformed[2],
            &message_start[0],
            &message_start[1],
            &message_start[2],
            &input_json[0],
            &input_json[1],
            &input_json[2],
            "",
            EV_PING,
            DATA_PING,
            "",
        ]);
        assert_eq!(state.reasoning_progress.hidden_events, 0);
        assert_eq!(state.reasoning_progress.visible_reasoning_events, 0);
        assert_eq!(state.reasoning_progress.text_delta_events, 0);
        assert!(!state.reasoning_progress.saw_reasoning());
        // El ping se cuenta aparte y jamás como razonamiento.
        assert_eq!(state.reasoning_progress.ping_events, 1);
    }

    #[test]
    fn data_line_after_delimiter_is_not_double_counted() {
        // El traductor resuelve un `data:` suelto tras el delimitador como
        // `unknown` y lo descarta: el probe debe comportarse igual, o el
        // sobreconteo inflaría los contadores con eventos que nunca se
        // tradujeron.
        let state = run_lines(&[EV_HIDDEN, DATA_HIDDEN, "", DATA_HIDDEN, ""]);
        assert_eq!(
            state.reasoning_progress.hidden_events, 1,
            "el data suelto tras el delimitador no debe contarse"
        );
        assert_eq!(state.reasoning_progress.estimated_tokens_sum, 120);
    }

    #[test]
    fn first_activity_log_emits_once() {
        // El flag de "primer info" se activa con la primera actividad
        // oculta y no se vuelve a emitir en las siguientes.
        let state = run_lines(&[
            EV_HIDDEN,
            DATA_HIDDEN,
            "",
            EV_HIDDEN,
            DATA_HIDDEN,
            "",
            EV_HIDDEN,
            DATA_HIDDEN,
            "",
        ]);
        assert!(state.reasoning_first_logged);
        assert_eq!(state.reasoning_progress.hidden_events, 3);
    }

    #[test]
    fn no_hidden_means_no_first_log() {
        let state = run_lines(&[EV_TEXT, DATA_TEXT, "", EV_VISIBLE, DATA_VISIBLE, ""]);
        assert!(
            !state.reasoning_first_logged,
            "el info de primera actividad oculta no debe emitirse sin progreso oculto"
        );
    }

    #[test]
    fn no_text_leaks_into_counters() {
        // Un secreto en el campo `thinking` visible solo debe influir en la
        // longitud, nunca aparecer en el probe ni en los contadores.
        let secret = hidden_block(
            r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"TOPSECRET123","estimated_tokens":9}}"#,
        );
        let state = run_lines(&[&secret[0], &secret[1], &secret[2]]);
        let dbg = format!("{:?}", state.reasoning_progress);
        assert!(!dbg.contains("TOPSECRET"));
    }

    #[test]
    fn liveness_regression_hidden_thinking_is_not_forwarded() {
        // Regresión de liveness: el traductor debe seguir convirtiendo el
        // progreso oculto en liveness (payload nulo, sin contenido) para
        // que el body stream se mantenga vivo sin reenviar nada. La
        // instrumentación se ejecuta antes y no debe alterar el chunk.
        let hidden = [
            EV_HIDDEN.to_string(),
            DATA_HIDDEN.to_string(),
            String::new(),
        ];
        let state = run_lines(&[&hidden[0], &hidden[1], &hidden[2]]);
        assert_eq!(state.reasoning_progress.hidden_events, 1);

        // El traductor real, alimentado con las mismas líneas.
        let mut t_state = StreamingState::new(false);
        let mut chunks = Vec::new();
        for line in [EV_HIDDEN, DATA_HIDDEN, ""] {
            // `translated` es sibling de `tests`, no de este módulo anidado:
            // `super` apuntaría a `streaming_state::tests`.
            if let Some(chunk) = crate::streaming_state::translated::parse_translated_sse_line(
                &mut t_state,
                TargetFormat::Anthropic,
                line,
                "chunk-hidden",
                1000,
                "claude-test",
            )
            .expect("parse must not fail")
            {
                chunks.push(chunk);
            }
        }
        assert_eq!(chunks.len(), 1, "solo el delta genera un chunk");
        let chunk = &chunks[0];
        assert!(
            chunk.is_liveness(),
            "el progreso oculto debe seguir siendo liveness"
        );
        assert!(!chunk.has_content, "no debe marcarse como contenido");
        assert!(chunk.usage.is_none(), "la estimación no es usage facturado");
        assert!(
            chunk.delta_reasoning.is_none(),
            "no debe filtrar razonamiento"
        );
    }

    #[test]
    fn instrumentation_never_feeds_official_usage_or_ttft() {
        // El progreso oculto es una estimación del vendor, no uso
        // facturado: ni `usage` ni `ttft_ms` deben tocarse, y el stop
        // reason debe seguir vacío hasta que llegue `message_delta`.
        let state = run_lines(&[EV_HIDDEN, DATA_HIDDEN, "", EV_HIDDEN, DATA_HIDDEN, ""]);
        assert!(state.usage.is_none(), "el progreso no debe crear usage");
        assert!(
            state.stop_reason.is_none(),
            "el progreso no debe fijar stop_reason"
        );
    }

    #[test]
    fn close_and_error_paths_keep_counters() {
        // Cierre normal y eventos posteriores: los contadores sobreviven al
        // cierre del stream y no se reinician.
        let done = [
            "event: message_delta".to_string(),
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":777}}".to_string(),
            String::new(),
        ];
        let state = run_lines(&[
            EV_HIDDEN,
            DATA_HIDDEN,
            "",
            &done[0],
            &done[1],
            &done[2],
            "event: message_stop",
            "data: {\"type\":\"message_stop\"}",
            "",
        ]);
        assert_eq!(state.reasoning_progress.hidden_events, 1);
        assert_eq!(state.reasoning_progress.estimated_tokens_sum, 120);
        assert!(
            state.reasoning_progress.first_activity_ms.is_some(),
            "la primera actividad debe quedar sellada"
        );
    }

    #[tokio::test]
    async fn handle_translated_done_attaches_merged_usage_to_client_payload() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let sink = StreamSink::direct(tx);
        let (pipeline, combo, target, model, req, timeouts) = setup_test_harness(&sink);
        let started = std::time::Instant::now();
        let ctx = StreamContext {
            req: &req,
            combo: &combo,
            target: &target,
            model: &model,
            target_format: TargetFormat::Anthropic,
            sink: &sink,
            trace_id: "test-trace",
            chunk_id: "test-chunk",
            model_name: "claude-3-5-sonnet",
            started,
            attempt: 1,
            race_size: 1,
            created: 1000,
            connect_and_send_ms: 5,
            resolved_timeouts: &timeouts,
            proxy_url: None,
            proxy_status: None,
        };
        let mut state = StreamingState::new(false);
        state.usage = Some(openproxy_types::message::OpenAIUsage {
            prompt_tokens: 120,
            completion_tokens: 45,
            total_tokens: 165,
            prompt_tokens_details: None,
        });

        let mut processor = ChunkProcessor {
            state: &mut state,
            dispatcher: &pipeline.dispatcher,
        };

        let chunk = crate::sse::UpstreamSseChunk {
            raw_payload: None,
            payload: serde_json::json!({
                "id": "chatcmpl-test",
                "object": "chat.completion.chunk",
                "created": 1000,
                "model": "claude-3-5-sonnet",
                "choices": [{
                    "index": 0,
                    "delta": {},
                    "finish_reason": "stop"
                }]
            }),
            done: true,
            usage: None,
            stop_reason: Some("stop".to_string()),
            delta_reasoning: None,
            delta_tool_calls: Vec::new(),
            has_content: false,
        };

        let res = processor.handle_translated_done(&ctx, chunk).await;
        assert!(matches!(res, Ok(crate::streaming::ChunkEvent::Done)));

        let frame1 = rx.recv().await.expect("must receive terminal frame");
        let frame1_str = String::from_utf8_lossy(&frame1);
        assert!(frame1_str.starts_with("data: "));
        let json_part = frame1_str.trim_start_matches("data: ").trim();
        let val: serde_json::Value = serde_json::from_str(json_part).expect("valid json frame");
        assert_eq!(val["usage"]["prompt_tokens"], 120);
        assert_eq!(val["usage"]["completion_tokens"], 45);
        assert_eq!(val["usage"]["total_tokens"], 165);

        let frame2 = rx.recv().await.expect("must receive [DONE] sentinel");
        assert_eq!(frame2.as_ref(), crate::SSE_DONE_BYTES.as_ref());
    }
}
