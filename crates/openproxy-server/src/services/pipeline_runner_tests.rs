//! End-to-end tests for the SSE bridge, FIFO ordering of data and terminal errors,
//! and wire cut behavior across supported protocols.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use bytes::Bytes;
use futures::StreamExt;
use http_body_util::BodyExt;
use tower::ServiceExt;

use openproxy_core::AppConfig;
use openproxy_core::admin::providers::{CreateProviderInput, create_provider};
use openproxy_core::admin::{CreateCustomModelInput, create_custom_model};
use openproxy_core::api_keys::CreateApiKeyInput;
use openproxy_db::DbPool;
use openproxy_db::combos::{AddTargetInput, add_target, create_combo};
use openproxy_db::secrets::MasterKey;
use openproxy_types::combos::Strategy;
use openproxy_types::ids::ComboId;
use openproxy_types::{EndpointKind, OpenAIRequest, TargetFormat};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::disconnect::CancelWatch;
use crate::router::build_router;
use crate::services::pipeline_runner::{PipelineRunner, PrepareRequestParams};
use crate::state::AppState;

const PARTIAL_CHUNK: &str = "data: {\"id\":\"chunk-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"partial-wire-content\"},\"finish_reason\":null}]}\n\n";

const SECOND_TARGET_CHUNK: &str = "data: {\"id\":\"chunk-second\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"replacement-chunk\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

/// Spawn a mock TCP server simulating upstream socket behaviors:
/// - If `cut_midstream` is true: writes partial valid SSE frame and abruptly closes socket.
/// - If `cut_midstream` is false: closes socket immediately before sending any HTTP response.
/// - Subsequent requests (if any) return a valid replacement stream.
async fn spawn_mock_upstream(
    cut_midstream: bool,
    hits_counter: Arc<AtomicUsize>,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream TCP listener");
    let addr = listener.local_addr().expect("local addr");

    let server_task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };

            let hits = Arc::clone(&hits_counter);
            tokio::spawn(async move {
                let mut req_buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let header_len = loop {
                    let n = match socket.read(&mut chunk).await {
                        Ok(n) if n > 0 => n,
                        _ => return,
                    };
                    req_buf.extend_from_slice(&chunk[..n]);
                    if let Some(pos) = req_buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break pos + 4;
                    }
                };

                let headers_str = String::from_utf8_lossy(&req_buf[..header_len]);
                let content_len = headers_str
                    .lines()
                    .filter_map(|l| l.split_once(':'))
                    .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, v)| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);

                while req_buf.len() < header_len + content_len {
                    let n = match socket.read(&mut chunk).await {
                        Ok(n) if n > 0 => n,
                        _ => return,
                    };
                    req_buf.extend_from_slice(&chunk[..n]);
                }

                let attempt = hits.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    if cut_midstream {
                        let header = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 999999\r\nconnection: close\r\n\r\n";
                        let _ = socket.write_all(header.as_bytes()).await;
                        let _ = socket.write_all(PARTIAL_CHUNK.as_bytes()).await;
                        let _ = socket.flush().await;
                        tokio::time::sleep(Duration::from_millis(30)).await;
                    }
                    // Abrupt closure: no DONE, no replacement
                    let _ = socket.shutdown().await;
                } else {
                    let body = SECOND_TARGET_CHUNK;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(resp.as_bytes()).await;
                    let _ = socket.flush().await;
                    let _ = socket.shutdown().await;
                }
            });
        }
    });

    (addr, server_task)
}

struct TestEnv {
    state: AppState,
    client_key: String,
    combo_id: ComboId,
    hits_p1: Arc<AtomicUsize>,
    hits_p2: Arc<AtomicUsize>,
    _servers: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        for task in &self._servers {
            task.abort();
        }
    }
}

async fn setup_test_env(cut_p1_midstream: bool) -> TestEnv {
    // SSRF bypass comes from the test build feature/environment, not unsafe
    // process-global environment mutation from a concurrent async test.

    let hits_p1 = Arc::new(AtomicUsize::new(0));
    let hits_p2 = Arc::new(AtomicUsize::new(0));

    let (addr1, task1) = spawn_mock_upstream(cut_p1_midstream, Arc::clone(&hits_p1)).await;
    let (addr2, task2) = spawn_mock_upstream(false, Arc::clone(&hits_p2)).await;

    let pool = Arc::new(
        DbPool::test_pool_with_prefix("bridge-e2e-wire").expect("create test sqlite pool"),
    );
    let master_key = Arc::new(MasterKey::generate().expect("generate master key"));
    let mut config = AppConfig::default();
    config.retries.max_attempts = 2;
    config.retries.backoff_base_ms = 0;
    config.racing.max_race_size = 1;

    let adapters = Arc::new(parking_lot::RwLock::new(Arc::new(Vec::new())));
    let state = AppState::for_test(config, Arc::clone(&pool), Arc::clone(&master_key), adapters);

    state.discovery_scheduler().cancel();
    state.supervisor().shutdown();
    state.discovery_scheduler().shutdown_and_wait().await;
    state.supervisor().shutdown_and_wait().await;

    let (combo_id, client_key) = pool
        .spawn_write({
            let master_key = Arc::clone(&master_key);
            move |conn| {
                let combo_id = create_combo(conn, "bridge-combo", Strategy::Priority, 1)?;

                for (name, addr, priority) in [("wire-p1", addr1, 1), ("wire-p2", addr2, 2)] {
                    let p_id = create_provider(
                        conn,
                        CreateProviderInput {
                            id: name.into(),
                            name: name.into(),
                            base_url: format!("http://{addr}/v1"),
                            auth_type: "bearer".into(),
                            format: "openai".into(),
                            extra_headers_json: None,
                            rate_limit_scope: None,
                            stream_mode: None,
                            prompt_cache_ttl: None,
                        },
                    )?;

                    openproxy_core::admin::create_account(
                        conn,
                        &master_key,
                        openproxy_core::admin::CreateAccountInput {
                            provider_id: name.into(),
                            api_key: Some("sk-test-mock-key".into()),
                            label: Some(format!("{name}-acc")),
                            priority: Some(10),
                            extra_config_json: None,
                        },
                    )?;

                    let m_id = create_custom_model(
                        conn,
                        CreateCustomModelInput {
                            provider_id: name.into(),
                            model_id: "test-model".into(),
                            display_name: Some(format!("{name} model")),
                            target_format: "openai".into(),
                            ttl_seconds: 3600,
                            model_type: Some("chat".into()),
                        },
                    )?;

                    add_target(
                        conn,
                        AddTargetInput {
                            combo_id,
                            provider_id: p_id,
                            account_id: None,
                            model_row_id: Some(m_id),
                            sub_combo_id: None,
                            priority_order: priority,
                            description: None,
                            selected_accounts: None,
                        },
                    )?;
                }

                let (_rec, plaintext) = openproxy_core::api_keys::create(
                    conn,
                    CreateApiKeyInput {
                        label: Some("client-key".into()),
                        scopes: vec!["chat".into()],
                        allowed_models: None,
                        allowed_combos: None,
                        blacklisted_providers: None,
                        blacklisted_models: None,
                        expires_at: None,
                    },
                    "test_setup",
                )?;

                Ok((combo_id, plaintext))
            }
        })
        .await
        .expect("seed database");

    state
        .rebuild_adapters()
        .await
        .expect("rebuild custom adapters");

    TestEnv {
        state,
        client_key,
        combo_id,
        hits_p1,
        hits_p2,
        _servers: vec![task1, task2],
    }
}

/// Helper: run bridge directly using PipelineRunner's real build_pipeline and spawn_streaming_bridge.
async fn run_runner_bridge_with_cut(
    cut_midstream: bool,
    target_format: TargetFormat,
) -> (String, usize, usize) {
    let env = setup_test_env(cut_midstream).await;
    let pipeline = PipelineRunner::build_pipeline(&env.state);

    let cancel = CancelWatch::new();
    let openai_req = Arc::new(OpenAIRequest {
        model: "bridge-combo".into(),
        stream: true,
        ..Default::default()
    });

    let prepared = PipelineRunner::prepare_request(PrepareRequestParams {
        state: &env.state,
        headers: &HeaderMap::new(),
        cancel,
        openai_req,
        raw_request_body: Bytes::from_static(b"{\"model\":\"bridge-combo\",\"stream\":true}"),
        api_key_id: None,
        combo_id: env.combo_id,
        combo_override: None,
        targets_override: None,
        endpoint_kind: EndpointKind::Chat,
    });

    let mut stream = PipelineRunner::spawn_streaming_bridge(
        pipeline,
        prepared.req,
        prepared.done_tx,
        prepared.stream_rx,
        target_format,
    );

    let mut collected = Vec::new();
    while let Some(chunk) = stream.next().await {
        collected.extend_from_slice(&chunk);
    }

    let wire = String::from_utf8_lossy(&collected).to_string();
    let hits1 = env.hits_p1.load(Ordering::SeqCst);
    let hits2 = env.hits_p2.load(Ordering::SeqCst);
    (wire, hits1, hits2)
}

#[tokio::test]
async fn test_bridge_fifo_order_openai_format() {
    // Upstream sends 1 valid chunk, then disconnects.
    // Invariants:
    // 1. Partial chunk arrives first.
    // 2. Terminal error frame arrives in the exact SAME stream after the partial chunk.
    // 3. No replacement chunk (retry halted because stream committed).
    // 4. No [DONE] terminator.
    // 5. hits_p1 == 1, hits_p2 == 0 despite retry limit = 2 and p2 available.
    let (wire, hits1, hits2) = run_runner_bridge_with_cut(true, TargetFormat::Openai).await;

    assert_eq!(hits1, 1, "first target must be hit exactly once");
    assert_eq!(
        hits2, 0,
        "second target must NOT be attempted after partial output was committed"
    );

    let partial_pos = wire.find("partial-wire-content");
    let error_pos = wire.find("\"error\":");

    assert!(
        partial_pos.is_some(),
        "wire must contain the initial partial data frame: {wire}"
    );
    assert!(
        error_pos.is_some(),
        "wire must contain terminal error frame: {wire}"
    );
    assert!(
        partial_pos.unwrap() < error_pos.unwrap(),
        "FIFO invariant violated: error frame preceded data frame: {wire}"
    );

    assert!(
        !wire.contains("replacement-chunk"),
        "replacement chunk from second target must NEVER appear: {wire}"
    );
    assert!(
        !wire.contains("[DONE]"),
        "[DONE] frame must NEVER be emitted on upstream cut: {wire}"
    );
}

#[tokio::test]
async fn test_bridge_fifo_order_anthropic_format() {
    let (wire, hits1, hits2) = run_runner_bridge_with_cut(true, TargetFormat::Anthropic).await;

    assert_eq!(hits1, 1, "first target must be hit exactly once");
    assert_eq!(
        hits2, 0,
        "second target must NOT be called after partial output"
    );

    let partial_pos = wire.find("partial-wire-content");
    let error_pos = wire.find("event: error");

    assert!(partial_pos.is_some(), "must contain partial data: {wire}");
    assert!(
        error_pos.is_some(),
        "must contain Anthropic error event: {wire}"
    );
    assert!(
        partial_pos.unwrap() < error_pos.unwrap(),
        "FIFO invariant violated: error preceded partial data in Anthropic format: {wire}"
    );

    assert!(!wire.contains("replacement-chunk"));
    assert!(!wire.contains("[DONE]"));
}

#[tokio::test]
async fn test_bridge_fifo_order_responses_format() {
    let (wire, hits1, hits2) = run_runner_bridge_with_cut(true, TargetFormat::Responses).await;

    assert_eq!(hits1, 1, "first target must be hit once");
    assert_eq!(hits2, 0, "second target must NOT be called");

    let partial_pos = wire.find("partial-wire-content");
    let error_pos = wire.find("event: response.failed");

    assert!(partial_pos.is_some(), "must contain partial data: {wire}");
    assert!(
        error_pos.is_some(),
        "must contain Responses response.failed event: {wire}"
    );
    assert!(
        partial_pos.unwrap() < error_pos.unwrap(),
        "FIFO invariant violated: response.failed preceded partial data: {wire}"
    );

    assert!(!wire.contains("replacement-chunk"));
    assert!(!wire.contains("[DONE]"));
}

#[tokio::test]
async fn test_bridge_failure_before_output_retries_safely() {
    // When socket closes before any byte is committed to the client stream sink,
    // pipeline MUST allow a fresh attempt instead of committing an error.
    let (wire, hits1, hits2) = run_runner_bridge_with_cut(false, TargetFormat::Openai).await;

    assert_eq!(hits1, 2, "first target must retry before any output");
    assert_eq!(hits2, 0, "successful same-target retry needs no fallback");

    assert!(
        wire.contains("replacement-chunk"),
        "successful replacement output from second target must be streamed: {wire}"
    );
    assert!(
        wire.contains("[DONE]"),
        "successful stream must terminate with [DONE]: {wire}"
    );
    assert!(
        !wire.contains("\"error\":"),
        "no error frame should be emitted when retry succeeds: {wire}"
    );
}

#[tokio::test]
async fn test_app_router_e2e_wire_cut_openai() {
    // Full in-process router invocation via HTTP POST /v1/chat/completions
    let env = setup_test_env(true).await;
    let router = build_router(env.state.clone());

    let payload = serde_json::json!({
        "model": "bridge-combo",
        "stream": true,
        "messages": [{"role": "user", "content": "hello"}]
    });

    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {}", env.client_key))
        .header("content-type", "application/json")
        .body(Body::from(payload.to_string()))
        .unwrap();

    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let collected = resp.into_body().collect().await.unwrap().to_bytes();
    let wire = String::from_utf8_lossy(&collected).to_string();

    let hits1 = env.hits_p1.load(Ordering::SeqCst);
    let hits2 = env.hits_p2.load(Ordering::SeqCst);

    assert_eq!(hits1, 1);
    assert_eq!(hits2, 0);

    let partial_pos = wire.find("partial-wire-content");
    let error_pos = wire.find("\"error\":");

    assert!(partial_pos.is_some(), "payload: {wire}");
    assert!(error_pos.is_some(), "payload: {wire}");
    assert!(partial_pos.unwrap() < error_pos.unwrap());
    assert!(!wire.contains("replacement-chunk"));
    assert!(!wire.contains("[DONE]"));
}

#[tokio::test]
async fn test_app_router_e2e_wire_cut_anthropic() {
    // Full in-process router invocation via HTTP POST /v1/messages
    let env = setup_test_env(true).await;
    let router = build_router(env.state.clone());

    let payload = serde_json::json!({
        "model": "bridge-combo",
        "stream": true,
        "max_tokens": 100,
        "messages": [{"role": "user", "content": "hello"}]
    });

    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/messages")
        .header("x-api-key", &env.client_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .body(Body::from(payload.to_string()))
        .unwrap();

    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let collected = resp.into_body().collect().await.unwrap().to_bytes();
    let wire = String::from_utf8_lossy(&collected).to_string();

    let hits1 = env.hits_p1.load(Ordering::SeqCst);
    let hits2 = env.hits_p2.load(Ordering::SeqCst);

    assert_eq!(hits1, 1);
    assert_eq!(hits2, 0);

    let partial_pos = wire.find("partial-wire-content");
    let error_pos = wire.find("event: error");

    assert!(partial_pos.is_some(), "wire: {wire}");
    assert!(error_pos.is_some(), "wire: {wire}");
    assert!(partial_pos.unwrap() < error_pos.unwrap());
    assert!(!wire.contains("replacement-chunk"));
    assert!(!wire.contains("[DONE]"));
}

#[tokio::test]
async fn test_app_router_e2e_wire_cut_responses() {
    // Full in-process router invocation via HTTP POST /v1/responses
    let env = setup_test_env(true).await;
    let router = build_router(env.state.clone());

    let payload = serde_json::json!({
        "model": "bridge-combo",
        "stream": true,
        "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]}]
    });

    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/responses")
        .header("authorization", format!("Bearer {}", env.client_key))
        .header("content-type", "application/json")
        .body(Body::from(payload.to_string()))
        .unwrap();

    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let collected = resp.into_body().collect().await.unwrap().to_bytes();
    let wire = String::from_utf8_lossy(&collected).to_string();

    let hits1 = env.hits_p1.load(Ordering::SeqCst);
    let hits2 = env.hits_p2.load(Ordering::SeqCst);

    assert_eq!(hits1, 1);
    assert_eq!(hits2, 0);

    let partial_pos = wire.find("partial-wire-content");
    let error_pos = wire.find("event: response.failed");

    assert!(partial_pos.is_some(), "wire: {wire}");
    assert!(error_pos.is_some(), "wire: {wire}");
    assert!(partial_pos.unwrap() < error_pos.unwrap());
    assert!(!wire.contains("replacement-chunk"));
    assert!(!wire.contains("[DONE]"));
}
