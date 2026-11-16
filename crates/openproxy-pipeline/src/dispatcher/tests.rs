//! Tests de wiring del dispatcher: invariantes cross-submódulo que solo
//! son observables con la composición completa de
//! `dispatcher::{fail, rotation, proxy, stream, unary, horde, mod}`.
//!
//! `super::*` resuelve contra el orquestador porque el archivo se declara
//! como `#[cfg(test)] mod tests;` desde `dispatcher/mod.rs`.

use super::stream_tests::{
    build_dispatcher_for_stream_test, make_stream_pipeline_request, make_stream_test_entities,
    seed_stream_provider,
};
use super::types::DispatchContext;

/// DB+pool pair compatible con `UpstreamDispatcher::new`.
pub(super) fn fresh_pool() -> (
    openproxy_db::DbPool,
    std::sync::Arc<parking_lot::Mutex<rusqlite::Connection>>,
    std::path::PathBuf,
) {
    let pool =
        openproxy_db::DbPool::test_pool_with_prefix("openproxy-wiring-test").expect("open pool");
    let extra = pool.open_connection().expect("open extra connection");
    let conn = std::sync::Arc::new(parking_lot::Mutex::new(extra));
    let path = pool.path().to_path_buf();
    (pool, conn, path)
}

#[tokio::test(flavor = "multi_thread")]
async fn handle_non_2xx_response_wires_is_hard_skip_for_validation_required() {
    let (_pool, conn_arc, _path) = fresh_pool();
    let provider_id = "wired-test";
    seed_stream_provider(&conn_arc, provider_id, "https://example.com");
    let (_pid, model, target, combo) = make_stream_test_entities(provider_id, "g-2.5");
    let dispatcher = build_dispatcher_for_stream_test(std::sync::Arc::clone(&conn_arc));

    let started = std::time::Instant::now();
    let dctx = DispatchContext {
        attempt: 1,
        race_size: 1,
        started,
        model: &model,
        proxy_url: None,
        proxy_status: None,
    };

    let mut req = make_stream_pipeline_request("g-2.5", false, None);
    std::sync::Arc::make_mut(&mut req.openai_request).messages =
        vec![openproxy_types::OpenAIMessage {
            role: "user".into(),
            content: Some(serde_json::Value::String("hi".into())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::new(),
        }];

    // 403 + VALIDATION_REQUIRED no debe penalizar la cuenta.
    let result = dispatcher
        .handle_non_2xx_response(
            403,
            None,
            r#"{"error":{"code":"VALIDATION_REQUIRED"}}"#.to_string(),
            req,
            &combo,
            &target,
            &model,
            &dctx,
            42,
            Some(10),
        )
        .await;

    let err = result
        .error
        .expect("non-2xx response must produce an error");
    assert!(
        err.is_hard_skip(),
        "audit #1: 403 VALIDATION_REQUIRED must yield is_hard_skip=true, got error={err:?}"
    );
    assert_eq!(
        err.upstream_error_class(),
        Some(openproxy_types::UpstreamErrorClass::ValidationRequired)
    );
}

#[test]
fn test_translate_non_streaming_body_commandcode_raw_chunk_and_full_response() {
    let mut pipeline_req =
        make_stream_pipeline_request("meta/muse-spark-1.3-contributor", false, None);
    pipeline_req.combo_id = openproxy_types::ComboId(0);

    let raw_chunk = serde_json::json!({
        "type": "text-delta",
        "text": "Hello world from CommandCode"
    });
    let res = super::unary::translate_non_streaming_body(
        openproxy_types::TargetFormat::CommandCodeGo,
        &raw_chunk,
        &pipeline_req,
    )
    .expect("must translate raw CommandCode chunk");
    assert_eq!(
        res.choices[0]
            .message
            .content
            .as_ref()
            .and_then(serde_json::Value::as_str),
        Some("Hello world from CommandCode")
    );

    let full_openai = serde_json::json!({
        "id": "chatcmpl_test",
        "object": "chat.completion",
        "created": 1234567,
        "model": "meta/muse-spark-1.3-contributor",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "Full response"
            },
            "finish_reason": "stop"
        }]
    });
    let res2 = super::unary::translate_non_streaming_body(
        openproxy_types::TargetFormat::CommandCodeGo,
        &full_openai,
        &pipeline_req,
    )
    .expect("must deserialize valid OpenAI shape");
    assert_eq!(
        res2.choices[0]
            .message
            .content
            .as_ref()
            .and_then(serde_json::Value::as_str),
        Some("Full response")
    );
}
