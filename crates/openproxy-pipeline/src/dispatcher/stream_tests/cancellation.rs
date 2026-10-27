use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn fail_stream_client_disconnected_normalizes_to_cancelled_499_with_partial_content() {
    let (_pool, conn_arc, _path) = fresh_pool();
    let dispatcher = build_dispatcher_for_stream_test(std::sync::Arc::clone(&conn_arc));
    let pid = openproxy_types::ids::ProviderId::new("stream-test");
    let model = openproxy_types::models::Model {
        row_id: openproxy_types::ids::ModelRowId(1),
        provider_id: pid.clone(),
        model_id: openproxy_types::ids::ModelId::new("g-2.5"),
        display_name: None,
        discovered_at: "2024-01-01".into(),
        expires_at: None,
        timeout_overrides_json: None,
        last_test_at: None,
        context_length: None,
        max_output_tokens: None,
        capabilities_json: None,
        family: None,
        model_type: "test".into(),
        input_modalities_json: None,
        output_modalities_json: None,
        last_test_status: None,
        target_format: openproxy_types::TargetFormat::Openai,
        active: true,
        custom: false,
        ..Default::default()
    };
    let combo = Combo {
        id: openproxy_types::ComboId(1),
        name: "test-combo".into(),
        strategy: Strategy::Priority,
        priority_mode: PriorityMode::Strict,
        race_size: 1,
        created_at: "2024-01-01".into(),
        context_window: None,
        cooldown_mode: openproxy_types::config::CooldownMode::None,
        cooldown_base_secs: None,
        cooldown_max_secs: None,
        cooldown_factor: None,
        lkgp_exploration_rate: None,
        selection_window_secs: Some(3600),
        preventive_rate_limit: false,
        ..Default::default()
    };
    let target = ComboTarget {
        id: openproxy_types::ComboTargetId(1),
        combo_id: openproxy_types::ComboId(1),
        provider_id: pid.clone(),
        account_id: None,
        model_row_id: None,
        sub_combo_id: None,
        priority_order: 1,
        weight: 1,
        active: true,
        cooldown_mode: None,
        cooldown_base_secs: None,
        cooldown_max_secs: None,
        cooldown_factor: None,
        rate_limit_scope: RateLimitScope::Account,
        thinking_effort: None,
        ..Default::default()
    };
    let (_tx, rx) = tokio::sync::watch::channel(None);
    let req = crate::PipelineRequest {
        request_id: openproxy_types::ids::RequestId::new(),
        trace_id: openproxy_types::TraceId::new(),
        combo_id: openproxy_types::ComboId(1),
        openai_request: std::sync::Arc::new(openproxy_types::OpenAIRequest {
            model: "g-2.5".into(),
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
        stream_sink: None,
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
    let pid = openproxy_types::ids::ProviderId::new("stream-test");
    let model = openproxy_types::models::Model {
        row_id: openproxy_types::ids::ModelRowId(1),
        provider_id: pid.clone(),
        model_id: openproxy_types::ids::ModelId::new("g-2.5"),
        display_name: None,
        discovered_at: "2024-01-01".into(),
        expires_at: None,
        timeout_overrides_json: None,
        last_test_at: None,
        context_length: None,
        max_output_tokens: None,
        capabilities_json: None,
        family: None,
        model_type: "test".into(),
        input_modalities_json: None,
        output_modalities_json: None,
        last_test_status: None,
        target_format: openproxy_types::TargetFormat::Openai,
        active: true,
        custom: false,
        ..Default::default()
    };
    let combo = Combo {
        id: openproxy_types::ComboId(1),
        name: "test-combo".into(),
        strategy: Strategy::Priority,
        priority_mode: PriorityMode::Strict,
        race_size: 1,
        created_at: "2024-01-01".into(),
        context_window: None,
        cooldown_mode: openproxy_types::config::CooldownMode::None,
        cooldown_base_secs: None,
        cooldown_max_secs: None,
        cooldown_factor: None,
        lkgp_exploration_rate: None,
        selection_window_secs: Some(3600),
        preventive_rate_limit: false,
        ..Default::default()
    };
    let target = ComboTarget {
        id: openproxy_types::ComboTargetId(1),
        combo_id: openproxy_types::ComboId(1),
        provider_id: pid.clone(),
        account_id: None,
        model_row_id: None,
        sub_combo_id: None,
        priority_order: 1,
        weight: 1,
        active: true,
        cooldown_mode: None,
        cooldown_base_secs: None,
        cooldown_max_secs: None,
        cooldown_factor: None,
        rate_limit_scope: RateLimitScope::Account,
        thinking_effort: None,
        ..Default::default()
    };
    let (_tx, rx) = tokio::sync::watch::channel(None);
    let req = crate::PipelineRequest {
        request_id: openproxy_types::ids::RequestId::new(),
        trace_id: openproxy_types::TraceId::new(),
        combo_id: openproxy_types::ComboId(1),
        openai_request: std::sync::Arc::new(openproxy_types::OpenAIRequest {
            model: "g-2.5".into(),
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
        stream_sink: None,
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
    let pid = openproxy_types::ids::ProviderId::new("stream-test");
    let model = openproxy_types::models::Model {
        row_id: openproxy_types::ids::ModelRowId(1),
        provider_id: pid.clone(),
        model_id: openproxy_types::ids::ModelId::new("g-2.5"),
        display_name: None,
        discovered_at: "2024-01-01".into(),
        expires_at: None,
        timeout_overrides_json: None,
        last_test_at: None,
        context_length: None,
        max_output_tokens: None,
        capabilities_json: None,
        family: None,
        model_type: "test".into(),
        input_modalities_json: None,
        output_modalities_json: None,
        last_test_status: None,
        target_format: openproxy_types::TargetFormat::Openai,
        active: true,
        custom: false,
        ..Default::default()
    };
    let combo = Combo {
        id: openproxy_types::ComboId(1),
        name: "test-combo".into(),
        strategy: Strategy::Priority,
        priority_mode: PriorityMode::Strict,
        race_size: 1,
        created_at: "2024-01-01".into(),
        context_window: None,
        cooldown_mode: openproxy_types::config::CooldownMode::None,
        cooldown_base_secs: None,
        cooldown_max_secs: None,
        cooldown_factor: None,
        lkgp_exploration_rate: None,
        selection_window_secs: Some(3600),
        preventive_rate_limit: false,
        ..Default::default()
    };
    let target = ComboTarget {
        id: openproxy_types::ComboTargetId(1),
        combo_id: openproxy_types::ComboId(1),
        provider_id: pid.clone(),
        account_id: None,
        model_row_id: None,
        sub_combo_id: None,
        priority_order: 1,
        weight: 1,
        active: true,
        cooldown_mode: None,
        cooldown_base_secs: None,
        cooldown_max_secs: None,
        cooldown_factor: None,
        rate_limit_scope: RateLimitScope::Account,
        thinking_effort: None,
        ..Default::default()
    };
    let (_tx, rx) = tokio::sync::watch::channel(None);
    let req = crate::PipelineRequest {
        request_id: openproxy_types::ids::RequestId::new(),
        trace_id: openproxy_types::TraceId::new(),
        combo_id: openproxy_types::ComboId(1),
        openai_request: std::sync::Arc::new(openproxy_types::OpenAIRequest {
            model: "g-2.5".into(),
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
        stream_sink: None,
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
