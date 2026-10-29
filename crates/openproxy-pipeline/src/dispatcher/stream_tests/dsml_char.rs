//! Character-by-character stress of the DSML path, plus a check that the
//! recorded body keeps its non-tool fields intact.

use super::{build_dispatcher_for_stream_test, fresh_pool};
use crate::dispatcher::types::StreamDispatchParams;
use openproxy_types::CancelReason;
use openproxy_types::combos::{Combo, ComboTarget, PriorityMode, Strategy};
use openproxy_types::providers::{AuthType, ProviderFormat, RateLimitScope};

const DSML_BODY: &str = "\n\n<｜｜DSML｜｜ calls>\n<｜｜DSML｜｜ invoke name=\"terminal\">\n<｜｜DSML｜｜ parameter name=\"command\" string=\"true\">ls -la</｜｜DSML｜｜ parameter>\n</｜｜DSML｜｜ invoke>\n</｜｜DSML｜｜ calls>";

fn chunk_line(delta: serde_json::Value) -> String {
    format!(
        "data: {}\n\n",
        serde_json::json!({
            "id": "chatcmpl-dsml",
            "object": "chat.completion.chunk",
            "created": 1793251173u64,
            "model": "deepseek-v4.1-flash:free",
            "choices": [{ "index": 0, "delta": delta, "finish_reason": serde_json::Value::Null }]
        })
    )
}

async fn run(frames: String) -> (Vec<String>, String) {
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
            let body = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{frames}"
            );
            let _ = socket.write_all(body.as_bytes()).await;
            let _ = socket.flush().await;
            let _ = socket.shutdown().await;
        }
    });

    let (_pool, conn_arc, _path) = fresh_pool();
    let pid = openproxy_types::ids::ProviderId::new("dsml-char");
    let base_url = format!("http://127.0.0.1:{}", addr.port());
    {
        let c = conn_arc.lock();
        openproxy_db::providers::create(
            &c,
            openproxy_db::providers::NewProvider {
                id: &pid,
                name: "dsml-char",
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
    let dispatcher = build_dispatcher_for_stream_test(std::sync::Arc::clone(&conn_arc));

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
        name: "dsml-char".into(),
        strategy: Strategy::Priority,
        priority_mode: PriorityMode::Strict,
        race_size: 1,
        created_at: "2024-01-01".into(),
        cooldown_mode: openproxy_types::config::CooldownMode::None,
        selection_window_secs: Some(3600),
        ..Default::default()
    };

    let (sink_tx, mut sink_rx) = tokio::sync::mpsc::channel(4096);
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
        trace_id: "t-dsml-char".to_string(),
        upstream_request: openproxy_adapters::upstream::UpstreamRequest::post_json(
            format!("{base_url}/v1/chat/completions"),
            bytes::Bytes::from(
                r#"{"model":"deepseek-v4.1-flash:free","messages":[],"stream":true}"#,
            ),
        ),
    };

    let result = dispatcher.dispatch_upstream_streaming(params).await;
    assert!(
        result.error.is_none(),
        "expected success: {:?}",
        result.error
    );

    let mut received = Vec::new();
    while let Ok(chunk) = sink_rx.try_recv() {
        received.push(String::from_utf8_lossy(&chunk).to_string());
    }
    let conn = conn_arc.lock();
    let recorded: String = conn
        .query_row(
            "SELECT COALESCE(response_body_json,'<null>') FROM usage ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap_or_else(|e| format!("<query error: {e}>"));
    (received, recorded)
}

#[tokio::test(flavor = "multi_thread")]
async fn dsml_char_by_char_emits_tool_calls() {
    let mut frames = String::new();
    frames.push_str(&chunk_line(
        serde_json::json!({ "role": "assistant", "content": "", "reasoning_content": "thinking..." }),
    ));
    for ch in DSML_BODY.chars() {
        frames.push_str(&chunk_line(
            serde_json::json!({ "content": ch.to_string() }),
        ));
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

    let (received, recorded) = run(frames).await;

    let all: String = received.concat();
    println!("\n=== CHAR-BY-CHAR: {} client frames ===", received.len());
    for (i, c) in received.iter().enumerate() {
        println!("[{i}] {}", c.trim_end());
    }
    println!("\n=== RECORDED ===\n{recorded}\n");

    assert!(
        all.contains("tool_calls"),
        "client must receive tool_calls:\n{all}"
    );
    assert!(
        !all.contains("DSML"),
        "no raw DSML may reach the client:\n{all}"
    );
}
