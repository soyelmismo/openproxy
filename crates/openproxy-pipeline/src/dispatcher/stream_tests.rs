use super::UpstreamDispatcher;
use super::tests::fresh_pool;
use super::types::{DispatchContext, StreamDispatchParams};
use openproxy_adapters::UpstreamClient;
use openproxy_db::MasterKey;
use openproxy_types::CancelReason;
use openproxy_types::combos::{Combo, ComboTarget, PriorityMode, Strategy};
use openproxy_types::providers::{AuthType, ProviderFormat, RateLimitScope};

fn build_dispatcher_for_stream_test(
    conn_arc: std::sync::Arc<parking_lot::Mutex<rusqlite::Connection>>,
) -> UpstreamDispatcher {
    let repo = std::sync::Arc::new(crate::repository::SqlitePipelineRepository::new(
        std::sync::Arc::clone(&conn_arc),
    ));
    let tracker = crate::usage_tracker::UsageTracker {
        conn: std::sync::Arc::clone(&conn_arc),
        background_tx: tokio::sync::mpsc::channel(1).0,
        record_bodies_and_headers: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        selection_registry: std::sync::Arc::new(openproxy_types::SelectionRegistry::new()),
        cooldown_secs: 60,
        cooldown_max_secs: 3600,
        cooldown_factor: 2,
        repo: std::sync::Arc::clone(&repo)
            as std::sync::Arc<dyn crate::repository::PipelineRepository>,
    };
    let cfg = crate::PipelineConfig {
        defaults: crate::timeouts::Timeouts::from_config(
            &openproxy_types::config::TimeoutsConfig::default(),
        ),
        racing: openproxy_types::config::RacingConfig::default(),
        retries: openproxy_types::config::RetriesConfig::default(),
        max_attempts: 1,
        master_key: std::sync::Arc::new(MasterKey::generate().unwrap()),
        adapters: std::sync::Arc::new(Vec::new()),
        cooldown_secs: 60,
        cooldown_max_secs: 3600,
        cooldown_factor: 2,
        upstream_client: UpstreamClient::new(),
        oauth_provider_registry: None,
        compression_mode: openproxy_compression::CompressionMode::Off,
        idle_chunk_retryable: true,
        quota_protection: openproxy_types::config::QuotaProtectionConfig::default(),
        pii_config: openproxy_types::config::PiiConfig::default(),
        background_tx: tokio::sync::mpsc::channel(1).0,
    };
    UpstreamDispatcher::new(
        std::sync::Arc::clone(&conn_arc),
        cfg,
        tracker,
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn dispatch_upstream_streaming_errors_without_sink() {
    let (_pool, conn_arc, _path) = fresh_pool();
    let provider_id = "stream-test";
    let pid = openproxy_types::ids::ProviderId::new(provider_id);
    {
        let c = conn_arc.lock();
        openproxy_db::providers::create(
            &c,
            openproxy_db::providers::NewProvider {
                id: &pid,
                name: provider_id,
                base_url: "https://example.com",
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

    let target = ComboTarget {
        id: openproxy_types::ids::ComboTargetId(1),
        combo_id: openproxy_types::ids::ComboId(1),
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
    };

    let combo = Combo {
        id: openproxy_types::ids::ComboId(1),
        name: "stream-test".into(),
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
    };

    let (_tx, rx) = tokio::sync::watch::channel::<Option<CancelReason>>(None);
    let req = crate::PipelineRequest {
        request_id: openproxy_types::ids::RequestId::new(),
        trace_id: openproxy_types::ids::TraceId::new(),
        combo_id: openproxy_types::ids::ComboId(1),
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
        trace_id: "t-stream".to_string(),
        upstream_request: openproxy_adapters::upstream::UpstreamRequest::post_json(
            String::new(),
            bytes::Bytes::new(),
        ),
    };

    let result = dispatcher.dispatch_upstream_streaming(params).await;
    assert!(
        result.error.is_some(),
        "dispatch_upstream_streaming without stream_sink must produce an error"
    );
    match result.error.expect("just asserted is_some") {
        openproxy_types::error::CoreError::Internal(msg) => {
            assert!(
                msg.contains("stream_sink"),
                "Internal error must mention stream_sink, got: {msg}"
            );
        }
        other => panic!("expected CoreError::Internal, got {other:?}"),
    }
    assert_eq!(result.status_code, 500);

    let _dctx: DispatchContext<'_> = DispatchContext {
        attempt: 1,
        race_size: 1,
        started,
        model: &model,
        proxy_url: None,
        proxy_status: None,
    };
}

#[tokio::test(flavor = "multi_thread")]
async fn dispatch_upstream_streaming_emits_done_on_metadata_only_stream() {
    unsafe {
        std::env::set_var("OPENPROXY_ALLOW_PRIVATE_UPSTREAMS", "true");
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        if let Ok((mut socket, _)) = listener.accept().await {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = [0u8; 2048];
            let _ = socket.read(&mut buf).await;
            let response = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n\
                data: {\"id\":\"test\",\"object\":\"chat.completion.chunk\",\"created\":123,\"model\":\"g-2.5\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
            let _ = socket.shutdown().await;
        }
    });

    let (_pool, conn_arc, _path) = fresh_pool();
    let provider_id = "stream-done-test";
    let pid = openproxy_types::ids::ProviderId::new(provider_id);
    let base_url = format!("http://127.0.0.1:{}", addr.port());
    {
        let c = conn_arc.lock();
        openproxy_db::providers::create(
            &c,
            openproxy_db::providers::NewProvider {
                id: &pid,
                name: provider_id,
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
        model_id: openproxy_types::ids::ModelId::new("g-2.5"),
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
        account_id: None,
        model_row_id: None,
        sub_combo_id: None,
        cooldown_mode: None,
        cooldown_base_secs: None,
        cooldown_max_secs: None,
        cooldown_factor: None,
        rate_limit_scope: RateLimitScope::Account,
        thinking_effort: None,
    };
    let combo = Combo {
        id: openproxy_types::ids::ComboId(1),
        name: "stream-done-test".into(),
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
    };

    let (sink_tx, mut sink_rx) = tokio::sync::mpsc::channel(16);
    let (_tx, rx) = tokio::sync::watch::channel::<Option<CancelReason>>(None);
    let req = crate::PipelineRequest {
        request_id: openproxy_types::ids::RequestId::new(),
        trace_id: openproxy_types::ids::TraceId::new(),
        combo_id: openproxy_types::ids::ComboId(1),
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
        stream_sink: Some(crate::race_sink::StreamSink::Direct(sink_tx)),
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
        trace_id: "t-stream-done".to_string(),
        upstream_request: openproxy_adapters::upstream::UpstreamRequest::post_json(
            format!("{base_url}/v1/chat/completions"),
            bytes::Bytes::from(r#"{"model":"g-2.5","messages":[],"stream":true}"#),
        ),
    };

    let result = dispatcher.dispatch_upstream_streaming(params).await;
    assert!(result.error.is_none(), "expected success, got {:?}", result.error);
    assert_eq!(result.status_code, 200);

    let mut received = Vec::new();
    while let Ok(chunk) = sink_rx.try_recv() {
        received.push(chunk);
    }
    assert!(!received.is_empty(), "must receive chunks");
    let last_chunk = received.last().unwrap();
    assert_eq!(last_chunk, &crate::pipeline::SSE_DONE_BYTES[..], "last chunk must be [DONE]");
}

#[tokio::test(flavor = "multi_thread")]
async fn dispatch_upstream_streaming_emits_done_on_empty_content_stream() {
    unsafe {
        std::env::set_var("OPENPROXY_ALLOW_PRIVATE_UPSTREAMS", "true");
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        if let Ok((mut socket, _)) = listener.accept().await {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = [0u8; 2048];
            let _ = socket.read(&mut buf).await;
            // Upstream sends empty string delta with finish_reason: "stop", but NO [DONE] frame
            let response = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n\
                data: {\"id\":\"test\",\"object\":\"chat.completion.chunk\",\"created\":123,\"model\":\"g-2.5\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"\"},\"finish_reason\":\"stop\"}]}\n\n";
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
            let _ = socket.shutdown().await;
        }
    });

    let (_pool, conn_arc, _path) = fresh_pool();
    let provider_id = "stream-empty-content-test";
    let pid = openproxy_types::ids::ProviderId::new(provider_id);
    let base_url = format!("http://127.0.0.1:{}", addr.port());
    {
        let c = conn_arc.lock();
        openproxy_db::providers::create(
            &c,
            openproxy_db::providers::NewProvider {
                id: &pid,
                name: provider_id,
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
        model_id: openproxy_types::ids::ModelId::new("g-2.5"),
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
        account_id: None,
        model_row_id: None,
        sub_combo_id: None,
        cooldown_mode: None,
        cooldown_base_secs: None,
        cooldown_max_secs: None,
        cooldown_factor: None,
        rate_limit_scope: RateLimitScope::Account,
        thinking_effort: None,
    };
    let combo = Combo {
        id: openproxy_types::ids::ComboId(1),
        name: "stream-empty-content-test".into(),
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
    };

    let (sink_tx, mut sink_rx) = tokio::sync::mpsc::channel(16);
    let (_tx, rx) = tokio::sync::watch::channel::<Option<CancelReason>>(None);
    let req = crate::PipelineRequest {
        request_id: openproxy_types::ids::RequestId::new(),
        trace_id: openproxy_types::ids::TraceId::new(),
        combo_id: openproxy_types::ids::ComboId(1),
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
        stream_sink: Some(crate::race_sink::StreamSink::Direct(sink_tx)),
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
        trace_id: "t-stream-empty-content".to_string(),
        upstream_request: openproxy_adapters::upstream::UpstreamRequest::post_json(
            format!("{base_url}/v1/chat/completions"),
            bytes::Bytes::from(r#"{"model":"g-2.5","messages":[],"stream":true}"#),
        ),
    };

    let result = dispatcher.dispatch_upstream_streaming(params).await;
    assert!(result.error.is_none(), "expected success, got {:?}", result.error);
    assert_eq!(result.status_code, 200);

    let mut received = Vec::new();
    while let Ok(chunk) = sink_rx.try_recv() {
        received.push(chunk);
    }
    assert!(!received.is_empty(), "must receive chunks");
    let last_chunk = received.last().unwrap();
    assert_eq!(last_chunk, &crate::pipeline::SSE_DONE_BYTES[..], "last chunk must be [DONE]");
}

#[tokio::test(flavor = "multi_thread")]
async fn dispatch_upstream_streaming_errors_on_empty_eof_stream() {
    unsafe {
        std::env::set_var("OPENPROXY_ALLOW_PRIVATE_UPSTREAMS", "true");
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        if let Ok((mut socket, _)) = listener.accept().await {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = [0u8; 2048];
            let _ = socket.read(&mut buf).await;
            // Upstream sends 200 OK headers but closes immediately with zero body bytes
            let response = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
            let _ = socket.shutdown().await;
        }
    });

    let (_pool, conn_arc, _path) = fresh_pool();
    let provider_id = "stream-empty-eof-test";
    let pid = openproxy_types::ids::ProviderId::new(provider_id);
    let base_url = format!("http://127.0.0.1:{}", addr.port());
    {
        let c = conn_arc.lock();
        openproxy_db::providers::create(
            &c,
            openproxy_db::providers::NewProvider {
                id: &pid,
                name: provider_id,
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
        model_id: openproxy_types::ids::ModelId::new("g-2.5"),
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
        account_id: None,
        model_row_id: None,
        sub_combo_id: None,
        cooldown_mode: None,
        cooldown_base_secs: None,
        cooldown_max_secs: None,
        cooldown_factor: None,
        rate_limit_scope: RateLimitScope::Account,
        thinking_effort: None,
    };
    let combo = Combo {
        id: openproxy_types::ids::ComboId(1),
        name: "stream-empty-eof-test".into(),
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
    };

    let (sink_tx, mut sink_rx) = tokio::sync::mpsc::channel(16);
    let (_tx, rx) = tokio::sync::watch::channel::<Option<CancelReason>>(None);
    let req = crate::PipelineRequest {
        request_id: openproxy_types::ids::RequestId::new(),
        trace_id: openproxy_types::ids::TraceId::new(),
        combo_id: openproxy_types::ids::ComboId(1),
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
        stream_sink: Some(crate::race_sink::StreamSink::Direct(sink_tx)),
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
        trace_id: "t-stream-empty-eof".to_string(),
        upstream_request: openproxy_adapters::upstream::UpstreamRequest::post_json(
            format!("{base_url}/v1/chat/completions"),
            bytes::Bytes::from(r#"{"model":"g-2.5","messages":[],"stream":true}"#),
        ),
    };

    let result = dispatcher.dispatch_upstream_streaming(params).await;
    // Must fail as empty streaming response
    assert!(result.error.is_some(), "expected error for completely empty stream");
    assert_eq!(result.status_code, 502);

    let mut received = Vec::new();
    while let Ok(chunk) = sink_rx.try_recv() {
        received.push(chunk);
    }
    // Client must NOT receive [DONE] for failed empty stream
    assert!(received.is_empty() || received.iter().all(|c| c != &crate::pipeline::SSE_DONE_BYTES[..]));
}

