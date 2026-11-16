use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn fail_stream_client_disconnected_normalizes_to_cancelled_499_with_partial_content() {
    let (_pool, conn_arc, _path) = fresh_pool();
    let dispatcher = build_dispatcher_for_stream_test(std::sync::Arc::clone(&conn_arc));
    let (_pid, model, target, combo) = make_stream_test_entities("stream-test", "g-2.5");
    let req = make_stream_pipeline_request("g-2.5", true, None);

    let mut acc = crate::sse_accumulator::ResponseAccumulator::new();
    acc.append_openai_raw(
        r#"data: {"choices":[{"delta":{"content":"partial response before disconnect"}}]}"#,
    );

    let fctx = crate::dispatcher::types::StreamFailureContext {
        proxy_url: None,
        proxy_status: None,
        req,
        combo: &combo,
        target: &target,
        attempt: 1,
        race_size: 1,
        started: std::time::Instant::now(),
        model: &model,
        connect_ms: 10,
        ttft_ms: Some(15),
        trace_id: "trace-dc-test".into(),
        acc: Some(&mut acc),
        chunk_id: "chunk-1",
        created: 1234567,
        model_name: "g-2.5",
    };

    let res = dispatcher.fail_stream_client_disconnected(fctx).await;
    assert_eq!(
        res.status_code, 499,
        "real client disconnect must yield 499"
    );
    assert!(
        matches!(
            res.error,
            Some(openproxy_types::CoreError::Cancelled(
                openproxy_types::CancelReason::ClientDisconnected
            ))
        ),
        "expected CoreError::Cancelled(ClientDisconnected), got {:?}",
        res.error
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn fail_on_sink_send_error_normalizes_to_cancelled_499() {
    let (_pool, conn_arc, _path) = fresh_pool();
    let dispatcher = build_dispatcher_for_stream_test(std::sync::Arc::clone(&conn_arc));
    let (_pid, model, target, combo) = make_stream_test_entities("stream-test", "g-2.5");
    let req = make_stream_pipeline_request("g-2.5", true, None);

    let mut acc = crate::sse_accumulator::ResponseAccumulator::new();
    acc.append_openai_raw(
        r#"data: {"choices":[{"delta":{"content":"partial response before sink dropped"}}]}"#,
    );

    let fctx = crate::dispatcher::types::StreamFailureContext {
        proxy_url: None,
        proxy_status: None,
        req,
        combo: &combo,
        target: &target,
        attempt: 1,
        race_size: 1,
        started: std::time::Instant::now(),
        model: &model,
        connect_ms: 10,
        ttft_ms: Some(15),
        trace_id: "trace-sink-test".into(),
        acc: Some(&mut acc),
        chunk_id: "chunk-1",
        created: 1234567,
        model_name: "g-2.5",
    };

    let res = dispatcher
        .fail_on_sink_send_error(crate::race_sink::StreamSinkError::Closed, fctx)
        .await;
    assert_eq!(res.status_code, 499);
    assert!(
        matches!(
            res.error,
            Some(openproxy_types::CoreError::Cancelled(
                openproxy_types::CancelReason::ClientDisconnected
            ))
        ),
        "expected CoreError::Cancelled(ClientDisconnected), got {:?}",
        res.error
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn fail_stream_prioritizes_inline_upstream_error_over_disconnect() {
    let (_pool, conn_arc, _path) = fresh_pool();
    let dispatcher = build_dispatcher_for_stream_test(std::sync::Arc::clone(&conn_arc));
    let (_pid, model, target, combo) = make_stream_test_entities("stream-test", "g-2.5");
    let req = make_stream_pipeline_request("g-2.5", true, None);

    let mut acc = crate::sse_accumulator::ResponseAccumulator::new();
    acc.append_raw_line(r#"data: {"error":{"code":500,"message":"upstream server blew up"}}"#);

    let fctx = crate::dispatcher::types::StreamFailureContext {
        proxy_url: None,
        proxy_status: None,
        req,
        combo: &combo,
        target: &target,
        attempt: 1,
        race_size: 1,
        started: std::time::Instant::now(),
        model: &model,
        connect_ms: 10,
        ttft_ms: Some(15),
        trace_id: "trace-inline-test".into(),
        acc: Some(&mut acc),
        chunk_id: "chunk-1",
        created: 1234567,
        model_name: "g-2.5",
    };

    let res = dispatcher.fail_stream_client_disconnected(fctx).await;
    assert_eq!(
        res.status_code, 500,
        "inline upstream error must take precedence over client disconnect"
    );
    assert!(
        matches!(
            res.error,
            Some(openproxy_types::CoreError::UpstreamError { status: 500, .. })
        ),
        "expected CoreError::UpstreamError, got {:?}",
        res.error
    );
}
