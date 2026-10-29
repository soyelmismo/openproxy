//! End-to-end reproduction of the DeepSeek DSML case:
//! upstream emits a `<｜｜DSML｜｜ calls>` block inside `delta.content`, the
//! client declared no `tools`, and we assert what the client receives and
//! what the accumulator records.

use super::{build_dispatcher_for_stream_test_with_recording, fresh_pool};
use crate::dispatcher::types::StreamDispatchParams;
use openproxy_types::CancelReason;
use openproxy_types::combos::{Combo, ComboTarget, PriorityMode, Strategy};
use openproxy_types::providers::{AuthType, ProviderFormat, RateLimitScope};

const DSML_BODY: &str = "\n\n<｜｜DSML｜｜ calls>\n<｜｜DSML｜｜ invoke name=\"terminal\">\n<｜｜DSML｜｜ parameter name=\"command\" string=\"true\">ls -la</｜｜DSML｜｜ parameter>\n</｜｜DSML｜｜ invoke>\n</｜｜DSML｜｜ calls>";

fn sse_body(content_json: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{content_json}"
    )
}

async fn run_case(upstream_frames: String) -> (Vec<String>, String) {
    unsafe {
        std::env::set_var("OPENPROXY_ALLOW_PRIVATE_UPSTREAMS", "true");
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        if let Ok((mut socket, _)) = listener.accept().await {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await;
            let response = sse_body(&upstream_frames);
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
            let _ = socket.shutdown().await;
        }
    });

    let (_pool, conn_arc, path) = fresh_pool();
    let pid = openproxy_types::ids::ProviderId::new("dsml-e2e");
    let base_url = format!("http://127.0.0.1:{}", addr.port());
    {
        let c = conn_arc.lock();
        openproxy_db::providers::create(
            &c,
            openproxy_db::providers::NewProvider {
                id: &pid,
                name: "dsml-e2e",
                base_url: &base_url,
                auth_type: AuthType::Bearer,
                format: ProviderFormat::Openai,
                extra_headers_json: None,
                auto_activate_keyword: None,
                rate_limit_scope: RateLimitScope::Account,
            },
        )
        .expect("seed provider");
    }
    let dispatcher = build_dispatcher_for_stream_test_with_recording(
        std::sync::Arc::clone(&conn_arc),
        true,
    );

    let model = openproxy_types::models::Model {
        row_id: openproxy_types::ids::ModelRowId(1),
        provider_id: pid.clone(),
        model_id: openproxy_types::ids::ModelId::new("deepseek-v4.1-flash:free"),
        target_format: openproxy_types::TargetFormat::Openai,
        active: true,
        ..Default::default()
    };
    let target = ComboTarget {
        id: openproxy_types::ids::ComboTargetId(1),
        combo_id: openproxy_types::ids::ComboId(1),
        provider_id: pid.clone(),
        active: true,
        priority_order: 1,
        weight: 1,
        rate_limit_scope: RateLimitScope::Account,
        ..Default::default()
    };
    let combo = Combo {
        id: openproxy_types::ids::ComboId(1),
        name: "dsml-e2e".into(),
        strategy: Strategy::Priority,
        priority_mode: PriorityMode::Strict,
        race_size: 1,
        created_at: "2024-01-01".into(),
        cooldown_mode: openproxy_types::config::CooldownMode::None,
        selection_window_secs: Some(3600),
        ..Default::default()
    };

    let (sink_tx, mut sink_rx) = tokio::sync::mpsc::channel(64);
    let (_tx, rx) = tokio::sync::watch::channel::<Option<CancelReason>>(None);
    let req = crate::PipelineRequest {
        request_id: openproxy_types::ids::RequestId::new(),
        trace_id: openproxy_types::ids::TraceId::new(),
        combo_id: openproxy_types::ids::ComboId(1),
        openai_request: std::sync::Arc::new(openproxy_types::OpenAIRequest {
            model: "deepseek-v4.1-flash:free".into(),
            messages: vec![],
            stream: true,
            temperature: None,
            max_tokens: None,
            top_p: None,
            stop: None,
            // The real failing request declared NO tools.
            tools: None,
            tool_choice: None,
            top_k: None,
            user: None,
            extra: serde_json::Map::new(),
        }),
        client_disconnected: rx,
        stream_sink: Some(crate::race_sink::StreamSink::direct(sink_tx)),
        api_key_id: None,
        combo_override: None,
        targets_override: None,
        request_headers: std::collections::BTreeMap::new(),
        request_body_json: None,
        race_cancelled: false,
        race_cancel: None,
        endpoint_kind: openproxy_types::endpoint::EndpointKind::Chat,
        compressed_messages: std::sync::Arc::new(std::sync::OnceLock::new()),
        pii_session: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        compression_stats: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        proxy_override: None,
        pinned_adapters: Default::default(),
    };

    let started = std::time::Instant::now();
    let params = StreamDispatchParams {
        target: &target,
        combo: &combo,
        req,
        model: &model,
        target_format: openproxy_types::TargetFormat::Openai,
        resolved_timeouts: &crate::timeouts::Timeouts::from_config(
            &openproxy_types::config::TimeoutsConfig::default(),
        ),
        started,
        attempt: 1,
        race_size: 1,
        trace_id: "t-dsml-e2e".to_string(),
        upstream_request: openproxy_adapters::upstream::UpstreamRequest::post_json(
            format!("{base_url}/v1/chat/completions"),
            bytes::Bytes::from(r#"{"model":"deepseek-v4.1-flash:free","messages":[],"stream":true}"#),
        ),
    };

    let result = dispatcher.dispatch_upstream_streaming(params).await;
    assert!(result.error.is_none(), "expected success: {:?}", result.error);

    let mut received = Vec::new();
    while let Ok(chunk) = sink_rx.try_recv() {
        received.push(String::from_utf8_lossy(&chunk).to_string());
    }

    let conn = conn_arc.lock();
    let journal_depth: i64 = conn
        .query_row("SELECT count(*) FROM usage_journal", [], |r| r.get(0))
        .unwrap_or(-1);
    println!("journal depth = {journal_depth}");
    let recorded: String = conn
        .query_row(
            "SELECT COALESCE(payload,'<null>') FROM usage_journal ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap_or_else(|e| format!("<query error: {e}>"));
    let _ = path;
    (received, recorded)
}

fn extract_recorded_response_body(journal_payload: &str) -> Option<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(journal_payload).ok()?;
    // The journal stores the serialized BackgroundJob; the usage input rides
    // under RecordAttempt/RecordUsage.
    let usage = v
        .get("RecordAttempt")
        .and_then(|r| r.get("usage_input"))
        .or_else(|| v.get("RecordUsage"))?;
    usage.get("response_body_json").cloned()
}

fn chunk_line(delta: serde_json::Value, finish: serde_json::Value) -> String {
    format!(
        "data: {}\n\n",
        serde_json::json!({
            "id": "chatcmpl-dsml",
            "object": "chat.completion.chunk",
            "created": 1793251173u64,
            "model": "deepseek-v4.1-flash:free",
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }]
        })
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn dsml_single_chunk_emits_tool_calls() {
    let mut frames = String::new();
    frames.push_str(&chunk_line(
        serde_json::json!({ "role": "assistant", "content": "", "reasoning_content": "thinking..." }),
        serde_json::Value::Null,
    ));
    frames.push_str(&chunk_line(
        serde_json::json!({ "content": DSML_BODY }),
        serde_json::Value::Null,
    ));
    frames.push_str(&format!(
        "data: {}\n\n",
        serde_json::json!({
            "id": "chatcmpl-dsml",
            "object": "chat.completion.chunk",
            "created": 1793251173u64,
            "model": "deepseek-v4.1-flash:free",
            "choices": [{ "index": 0, "delta": {}, "finish_reason": serde_json::Value::Null }],
            "usage": { "prompt_tokens": 55254, "completion_tokens": 336, "total_tokens": 55590 }
        })
    ));
    frames.push_str("data: [DONE]\n\n");

    let (received, recorded) = run_case(frames).await;

    println!("\n=== CLIENT CHUNKS ({}) ===", received.len());
    for (i, c) in received.iter().enumerate() {
        println!("[{i}] {}", c.trim_end());
    }
    println!("\n=== RECORDED response_body_json ===\n{recorded}\n");

    // The journal drains in a background worker that this harness does not
    // spawn, so the row may legitimately be absent. The recorded-body shape is
    // covered directly in `recorded_body_tests`.
    if let Some(recorded_body) = extract_recorded_response_body(&recorded) {
        let rec_choice = &recorded_body["choices"][0];
        assert_eq!(
            rec_choice["finish_reason"], "tool_calls",
            "recorded body must carry tool_calls finish_reason:\n{recorded_body}"
        );
        assert_eq!(
            rec_choice["message"]["tool_calls"][0]["function"]["name"], "terminal",
            "recorded body must carry the parsed tool call:\n{recorded_body}"
        );
        assert!(
            rec_choice["message"]["content"].is_null(),
            "recorded content must be emptied of the DSML block:\n{recorded_body}"
        );
    }

    let all: String = received.concat();
    assert!(
        all.contains("tool_calls"),
        "client must receive tool_calls, got:\n{all}"
    );
    assert!(
        !all.contains("DSML"),
        "raw DSML must never reach the client, got:\n{all}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn dsml_split_chunks_emit_tool_calls() {
    // Same block, but the upstream streams it in pieces like a real token
    // stream does.
    let mut frames = String::new();
    frames.push_str(&chunk_line(
        serde_json::json!({ "role": "assistant", "content": "", "reasoning_content": "thinking..." }),
        serde_json::Value::Null,
    ));
    let mut chars = DSML_BODY.chars().peekable();
    let mut buf = String::new();
    while let Some(c) = chars.next() {
        buf.push(c);
        if buf.chars().count() >= 12 || chars.peek().is_none() {
            frames.push_str(&chunk_line(
                serde_json::json!({ "content": std::mem::take(&mut buf) }),
                serde_json::Value::Null,
            ));
        }
    }
    frames.push_str(&format!(
        "data: {}\n\n",
        serde_json::json!({
            "id": "chatcmpl-dsml",
            "object": "chat.completion.chunk",
            "created": 1793251173u64,
            "model": "deepseek-v4.1-flash:free",
            "choices": [{ "index": 0, "delta": {}, "finish_reason": serde_json::Value::Null }],
            "usage": { "prompt_tokens": 55254, "completion_tokens": 336, "total_tokens": 55590 }
        })
    ));
    frames.push_str("data: [DONE]\n\n");

    let (received, recorded) = run_case(frames).await;

    println!("\n=== SPLIT: CLIENT CHUNKS ({}) ===", received.len());
    for (i, c) in received.iter().enumerate() {
        println!("[{i}] {}", c.trim_end());
    }
    println!("\n=== SPLIT: RECORDED ===\n{recorded}\n");

    let all: String = received.concat();
    assert!(
        all.contains("tool_calls"),
        "client must receive tool_calls, got:\n{all}"
    );
    assert!(
        !all.contains("DSML"),
        "raw DSML must never reach the client, got:\n{all}"
    );
}
