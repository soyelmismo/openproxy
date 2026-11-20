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
) -> (crate::pipeline::PipelineResult, Vec<bytes::Bytes>) {
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

/// `stream=false` fixture: the pipeline uses a `Discard` sink, so the
/// dispatcher must materialize the client response from the accumulated SSE
/// while keeping the recorded usage raw.
async fn run_discard_sse_fixture(
    provider_id: &str,
    trace_id: &str,
    response: &str,
    prefix: Option<&str>,
) -> (
    crate::pipeline::PipelineResult,
    std::sync::Arc<parking_lot::Mutex<rusqlite::Connection>>,
) {
    let base_url = spawn_single_sse_server(response.to_string()).await;
    let (_pool, conn_arc, _path) = fresh_pool();
    seed_stream_provider(&conn_arc, provider_id, &base_url);
    let mut dispatcher = build_dispatcher_for_stream_test(std::sync::Arc::clone(&conn_arc));
    // Keep a live receiver so `record()` admits the usage job instead of
    // failing on a closed worker; the test drains it below.
    let (job_tx, mut job_rx) = tokio::sync::mpsc::channel(16);
    dispatcher.tracker.background_tx = job_tx;

    let (_pid, model, target, combo) = make_stream_test_entities(provider_id, "g-2.5");
    let mut req =
        make_stream_pipeline_request("g-2.5", false, Some(crate::race_sink::StreamSink::Discard));
    if let Some(prefix) = prefix {
        *req.active_system_prefix.lock() = Some(prefix.to_string());
    }
    req.openai_request = std::sync::Arc::new(openproxy_types::OpenAIRequest {
        messages: vec![openproxy_types::OpenAIMessage {
            role: "user".to_string(),
            content: Some(serde_json::Value::String("hello".to_string())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::new(),
        }],
        ..(*req.openai_request).clone()
    });

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

    while let Ok(job) = job_rx.try_recv() {
        crate::worker::process_job(&conn_arc, job).expect("usage job must persist");
    }
    (result, conn_arc)
}

fn sse_with_final_usage(usage: serde_json::Value) -> String {
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n\
         data: {{\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"g-2.5\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"hello\"}},\"finish_reason\":null}}]}}\n\n\
         data: {{\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"g-2.5\",\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}],\"usage\":{usage}}}\n\n\
         data: [DONE]\n\n"
    )
}

fn recorded_prompt_and_completion(
    conn: &std::sync::Arc<parking_lot::Mutex<rusqlite::Connection>>,
) -> (i64, i64) {
    let conn = conn.lock();
    conn.query_row(
        "SELECT prompt_tokens, completion_tokens FROM usage ORDER BY id DESC LIMIT 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .expect("a usage row must be persisted")
}

#[tokio::test(flavor = "multi_thread")]
async fn discard_stream_applies_prefix_discount_to_final_response_only() {
    let prefix = "SYSTEM PREFIX ".repeat(64);
    let expected_prefix = {
        let msg = [openproxy_types::OpenAIMessage {
            role: "system".to_string(),
            content: Some(serde_json::Value::String(prefix.clone())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::new(),
        }];
        openproxy_compression::token_estimate::estimate_prompt_tokens(&msg, Some("g-2.5"))
    };
    assert!(
        expected_prefix > 0 && expected_prefix < 1000,
        "fixture prefix must be non-zero and smaller than the upstream prompt"
    );

    let response = sse_with_final_usage(serde_json::json!({
        "prompt_tokens": 1000,
        "completion_tokens": 10,
        "total_tokens": 1010
    }));
    let (result, conn_arc) = run_discard_sse_fixture(
        "discard-prefix",
        "t-discard-prefix",
        &response,
        Some(&prefix),
    )
    .await;

    assert!(
        result.error.is_none(),
        "expected success: {:?}",
        result.error
    );
    assert_eq!(result.status_code, 200);
    let final_response = result
        .final_response
        .expect("a Discard sink must materialize the client response");
    let usage = final_response.usage.expect("final response usage");
    assert_eq!(
        usage.prompt_tokens,
        1000 - expected_prefix,
        "the client copy must discount the injected prefix"
    );
    assert_eq!(usage.completion_tokens, 10);
    assert_eq!(
        usage.total_tokens,
        usage.prompt_tokens + usage.completion_tokens,
        "the client total must be recomputed after the discount"
    );

    // The record is the raw upstream observation, asserted independently of
    // the response copy above.
    let (recorded_prompt, recorded_completion) = recorded_prompt_and_completion(&conn_arc);
    assert_eq!(
        recorded_prompt, 1000,
        "the SQLite row must keep the undiscounted upstream prompt"
    );
    assert_eq!(recorded_completion, 10);
}

#[tokio::test(flavor = "multi_thread")]
async fn discard_stream_does_not_discount_an_estimated_prompt() {
    // Upstream omitted prompt_tokens, so the response prompt is an estimate
    // over the original request, which never contained the injected prefix.
    // Subtracting the prefix there would invent a discount that was not billed.
    let prefix = "SYSTEM PREFIX ".repeat(64);
    let response = sse_with_final_usage(serde_json::json!({
        "completion_tokens": 10,
        "total_tokens": 10
    }));
    let (result, conn_arc) = run_discard_sse_fixture(
        "discard-estimated-prompt",
        "t-discard-estimated-prompt",
        &response,
        Some(&prefix),
    )
    .await;

    assert!(
        result.error.is_none(),
        "expected success: {:?}",
        result.error
    );
    let final_response = result
        .final_response
        .expect("a Discard sink must materialize the client response");
    let usage = final_response.usage.expect("final response usage");
    assert!(
        usage.prompt_tokens > 0,
        "an estimated prompt must not be reduced to zero by a prefix it never contained"
    );
    let prefix_tokens = {
        let msg = [openproxy_types::OpenAIMessage {
            role: "system".to_string(),
            content: Some(serde_json::Value::String(prefix.clone())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::new(),
        }];
        openproxy_compression::token_estimate::estimate_prompt_tokens(&msg, Some("g-2.5"))
    };
    assert!(
        usage.prompt_tokens < prefix_tokens,
        "the fixture only proves the gate when the estimate is smaller than the prefix"
    );
    assert_eq!(usage.completion_tokens, 10);

    let (recorded_prompt, recorded_completion) = recorded_prompt_and_completion(&conn_arc);
    assert_eq!(
        recorded_prompt,
        i64::from(usage.prompt_tokens),
        "the recorded estimate must match the client copy when no discount applies"
    );
    assert_eq!(recorded_completion, 10);
}
