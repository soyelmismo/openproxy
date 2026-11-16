use super::UpstreamDispatcher;
use super::tests::fresh_pool;
use super::types::{DispatchContext, StreamDispatchParams};
use openproxy_adapters::UpstreamClient;
use openproxy_db::MasterKey;
use openproxy_types::CancelReason;
use openproxy_types::combos::{Combo, ComboTarget, PriorityMode, Strategy};
use openproxy_types::providers::{AuthType, ProviderFormat, RateLimitScope};

mod cancellation;
mod dsml_char;
mod dsml_e2e;

pub(super) fn build_dispatcher_for_stream_test(
    conn_arc: std::sync::Arc<parking_lot::Mutex<rusqlite::Connection>>,
) -> UpstreamDispatcher {
    build_dispatcher_for_stream_test_with_recording(conn_arc, false)
}

/// Same as [`build_dispatcher_for_stream_test`], but with body recording on,
/// so tests can assert what lands in `usage.response_body_json`.
pub(super) fn build_dispatcher_for_stream_test_with_recording(
    conn_arc: std::sync::Arc<parking_lot::Mutex<rusqlite::Connection>>,
    recording: bool,
) -> UpstreamDispatcher {
    let repo = std::sync::Arc::new(crate::repository::SqlitePipelineRepository::new(
        std::sync::Arc::clone(&conn_arc),
    ));
    let tracker = crate::usage_tracker::UsageTracker {
        conn: std::sync::Arc::clone(&conn_arc),
        background_tx: tokio::sync::mpsc::channel(1).0,
        record_bodies_and_headers: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
            recording,
        )),
        selection_registry: std::sync::Arc::new(openproxy_types::SelectionRegistry::new()),
        cooldown_secs: 60,
        cooldown_max_secs: 3600,
        cooldown_factor: 2,
        repo: std::sync::Arc::clone(&repo)
            as std::sync::Arc<dyn crate::repository::PipelineRepository>,
        coordinator: None,
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
        modality_routing: openproxy_types::config::ModalityRoutingConfig::default(),
        error_masking: openproxy_types::config::TargetErrorMaskingConfig::default(),
        background_tx: tokio::sync::mpsc::channel(1).0,
    };
    UpstreamDispatcher::new(
        std::sync::Arc::clone(&conn_arc),
        cfg,
        tracker,
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
}

pub(super) fn seed_stream_provider(
    conn_arc: &std::sync::Arc<parking_lot::Mutex<rusqlite::Connection>>,
    provider_id: &str,
    base_url: &str,
) -> openproxy_types::ids::ProviderId {
    let pid = openproxy_types::ids::ProviderId::new(provider_id);
    let c = conn_arc.lock();
    openproxy_db::providers::create(
        &c,
        openproxy_db::providers::NewProvider {
            id: &pid,
            name: provider_id,
            base_url,
            auth_type: AuthType::Bearer,
            format: ProviderFormat::Openai,
            extra_headers_json: None,
            auto_activate_keyword: None,
            rate_limit_scope: RateLimitScope::Account,
        },
    )
    .expect("seed provider");
    pid
}

pub(super) fn make_stream_test_entities(
    provider_id: &str,
    model_id: &str,
) -> (
    openproxy_types::ids::ProviderId,
    openproxy_types::models::Model,
    ComboTarget,
    Combo,
) {
    let pid = openproxy_types::ids::ProviderId::new(provider_id);
    let model = openproxy_types::models::Model {
        row_id: openproxy_types::ids::ModelRowId(1),
        provider_id: pid.clone(),
        model_id: openproxy_types::ids::ModelId::new(model_id),
        discovered_at: "2024-01-01".into(),
        model_type: "test".into(),
        target_format: openproxy_types::TargetFormat::Openai,
        active: true,
        ..Default::default()
    };
    let target = ComboTarget {
        id: openproxy_types::ids::ComboTargetId(1),
        combo_id: openproxy_types::ids::ComboId(1),
        provider_id: pid.clone(),
        priority_order: 1,
        weight: 1,
        active: true,
        rate_limit_scope: RateLimitScope::Account,
        ..Default::default()
    };
    let combo = Combo {
        id: openproxy_types::ids::ComboId(1),
        name: provider_id.into(),
        strategy: Strategy::Priority,
        priority_mode: PriorityMode::Strict,
        race_size: 1,
        created_at: "2024-01-01".into(),
        cooldown_mode: openproxy_types::config::CooldownMode::None,
        selection_window_secs: Some(3600),
        ..Default::default()
    };
    (pid, model, target, combo)
}

pub(super) fn make_stream_pipeline_request(
    model_id: &str,
    stream: bool,
    stream_sink: Option<crate::race_sink::StreamSink>,
) -> crate::PipelineRequest {
    let (_tx, rx) = tokio::sync::watch::channel::<Option<CancelReason>>(None);
    crate::PipelineRequest {
        request_id: openproxy_types::ids::RequestId::new(),
        trace_id: openproxy_types::ids::TraceId::new(),
        combo_id: openproxy_types::ids::ComboId(1),
        openai_request: std::sync::Arc::new(openproxy_types::OpenAIRequest {
            model: model_id.into(),
            messages: vec![],
            stream,
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
        stream_sink,
        api_key_id: None,
        combo_override: None,
        targets_override: None,
        request_headers: std::collections::BTreeMap::new(),
        request_body_json: None,
        race_cancelled: false,
        race_cancel: None,
        endpoint_kind: openproxy_types::endpoint::EndpointKind::Chat,
        compressed_messages: std::sync::Arc::new(std::sync::OnceLock::new()),
        active_system_prefix: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        pii_session: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        compression_stats: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        proxy_override: None,
        pinned_adapters: Default::default(),
    }
}

pub(super) async fn spawn_single_sse_server(response: String) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((mut socket, _)) = listener.accept().await {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await;
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
            let _ = socket.shutdown().await;
        }
    });
    format!("http://127.0.0.1:{}", addr.port())
}

async fn run_sse_stream_fixture(
    provider_id: &str,
    trace_id: &str,
    response: &str,
) -> (
    crate::pipeline::PipelineResult,
    Vec<bytes::Bytes>,
) {
    let base_url = spawn_single_sse_server(response.to_string()).await;
    let (_pool, conn_arc, _path) = fresh_pool();
    seed_stream_provider(&conn_arc, provider_id, &base_url);
    let dispatcher = build_dispatcher_for_stream_test(std::sync::Arc::clone(&conn_arc));
    let (_pid, model, target, combo) = make_stream_test_entities(provider_id, "g-2.5");
    let (sink_tx, mut sink_rx) = tokio::sync::mpsc::channel(16);
    let req = make_stream_pipeline_request(
        "g-2.5",
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
            bytes::Bytes::from(r#"{"model":"g-2.5","messages":[],"stream":true}"#),
        ),
    };
    let result = dispatcher.dispatch_upstream_streaming(params).await;
    let mut received = Vec::new();
    while let Ok(chunk) = sink_rx.try_recv() {
        received.push(chunk);
    }
    (result, received)
}

#[tokio::test(flavor = "multi_thread")]
async fn dispatch_upstream_streaming_errors_without_sink() {
    let (_pool, conn_arc, _path) = fresh_pool();
    let provider_id = "stream-test";
    seed_stream_provider(&conn_arc, provider_id, "https://example.com");
    let dispatcher = build_dispatcher_for_stream_test(std::sync::Arc::clone(&conn_arc));
    let (_pid, model, target, combo) = make_stream_test_entities(provider_id, "g-2.5");
    let req = make_stream_pipeline_request("g-2.5", true, None);

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
    let response = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n\
        data: {\"id\":\"test\",\"object\":\"chat.completion.chunk\",\"created\":123,\"model\":\"g-2.5\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
    let (result, received) =
        run_sse_stream_fixture("stream-done-test", "t-stream-done", response).await;
    assert!(
        result.error.is_none(),
        "expected success, got {:?}",
        result.error
    );
    assert_eq!(result.status_code, 200);
    assert!(!received.is_empty(), "must receive chunks");
    assert_eq!(
        received.last().unwrap(),
        &crate::pipeline::SSE_DONE_BYTES[..],
        "last chunk must be [DONE]"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn dispatch_upstream_streaming_emits_done_on_empty_content_stream() {
    let response = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n\
        data: {\"id\":\"test\",\"object\":\"chat.completion.chunk\",\"created\":123,\"model\":\"g-2.5\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"\"},\"finish_reason\":\"stop\"}]}\n\n";
    let (result, received) = run_sse_stream_fixture(
        "stream-empty-content-test",
        "t-stream-empty-content",
        response,
    )
    .await;
    assert!(
        result.error.is_none(),
        "expected success, got {:?}",
        result.error
    );
    assert_eq!(result.status_code, 200);
    assert!(!received.is_empty(), "must receive chunks");
    assert_eq!(
        received.last().unwrap(),
        &crate::pipeline::SSE_DONE_BYTES[..],
        "last chunk must be [DONE]"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn dispatch_upstream_streaming_errors_on_empty_eof_stream() {
    let response =
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
    let (result, received) =
        run_sse_stream_fixture("stream-empty-eof-test", "t-stream-empty-eof", response).await;
    assert!(
        result.error.is_some(),
        "expected error for completely empty stream"
    );
    assert_eq!(result.status_code, 502);
    assert!(
        received.is_empty()
            || received
                .iter()
                .all(|c| c != &crate::pipeline::SSE_DONE_BYTES[..])
    );
}
