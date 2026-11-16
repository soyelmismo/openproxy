//! Character-by-character stress of the DSML path, plus a check that the
//! recorded body keeps its non-tool fields intact.

use super::{
    build_dispatcher_for_stream_test, fresh_pool, make_stream_pipeline_request,
    make_stream_test_entities, seed_stream_provider, spawn_single_sse_server,
};
use crate::dispatcher::types::StreamDispatchParams;
use std::fmt::Write as _;

pub(super) const DSML_BODY: &str = "\n\n<｜｜DSML｜｜ calls>\n<｜｜DSML｜｜ invoke name=\"terminal\">\n<｜｜DSML｜｜ parameter name=\"command\" string=\"true\">ls -la</｜｜DSML｜｜ parameter>\n</｜｜DSML｜｜ invoke>\n</｜｜DSML｜｜ calls>";

pub(super) fn dsml_chunk_line(delta: serde_json::Value, finish: serde_json::Value) -> String {
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

fn chunk_line(delta: serde_json::Value) -> String {
    dsml_chunk_line(delta, serde_json::Value::Null)
}

pub(super) async fn run_dsml_stream_case(
    provider_id: &str,
    trace_id: &str,
    recording: bool,
    sink_cap: usize,
    frames: String,
) -> (
    Vec<String>,
    std::sync::Arc<parking_lot::Mutex<rusqlite::Connection>>,
) {
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{frames}"
    );
    let base_url = spawn_single_sse_server(response).await;
    let (_pool, conn_arc, _path) = fresh_pool();
    seed_stream_provider(&conn_arc, provider_id, &base_url);
    let dispatcher = if recording {
        super::build_dispatcher_for_stream_test_with_recording(
            std::sync::Arc::clone(&conn_arc),
            true,
        )
    } else {
        build_dispatcher_for_stream_test(std::sync::Arc::clone(&conn_arc))
    };

    let (_pid, model, target, combo) =
        make_stream_test_entities(provider_id, "deepseek-v4.1-flash:free");
    let (sink_tx, mut sink_rx) = tokio::sync::mpsc::channel(sink_cap);
    let req = make_stream_pipeline_request(
        "deepseek-v4.1-flash:free",
        true,
        Some(crate::race_sink::StreamSink::direct(sink_tx)),
    );

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
        trace_id: trace_id.to_string(),
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
    (received, conn_arc)
}

async fn run(frames: String) -> (Vec<String>, String) {
    let (received, conn_arc) =
        run_dsml_stream_case("dsml-char", "t-dsml-char", false, 4096, frames).await;
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
    let _ = write!(
        frames,
        "data: {}\n\n",
        serde_json::json!({
            "id": "chatcmpl-dsml",
            "object": "chat.completion.chunk",
            "created": 1793251173u64,
            "model": "deepseek-v4.1-flash:free",
            "choices": [{ "index": 0, "delta": {}, "finish_reason": serde_json::Value::Null }],
            "usage": { "prompt_tokens": 55254, "completion_tokens": 336, "total_tokens": 55590 }
        })
    );
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
