use super::*;
use openproxy_types::OpenAIRequest;
use openproxy_types::combos::{Combo, ComboTarget};
use openproxy_types::ids::{ComboId, RequestId, TraceId};
use std::sync::Arc;

fn make_test_builder<'a>(
    tracker: &'a UsageTracker,
    combo: &'a Combo,
    target: &'a ComboTarget,
) -> UsageRecordBuilder<'a> {
    let (_tx, rx) = tokio::sync::watch::channel(None);
    let req = PipelineRequest {
        request_id: RequestId::new(),
        trace_id: TraceId::new(),
        combo_id: ComboId(1),
        openai_request: Arc::new(OpenAIRequest {
            model: "test-model".into(),
            messages: vec![],
            stream: false,
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
        compressed_messages: Arc::new(std::sync::OnceLock::new()),
        pii_session: Arc::new(parking_lot::Mutex::new(None)),
        compression_stats: Arc::new(parking_lot::Mutex::new(None)),
        proxy_override: None,
    };
    UsageRecordBuilder::new(tracker, req, combo, target)
}

fn make_test_combo_and_target() -> (Combo, ComboTarget) {
    let combo: Combo = serde_json::from_value(serde_json::json!({
        "id": 1,
        "name": "test",
        "strategy": "priority",
        "race_size": 1,
        "created_at": "2026-01-01"
    }))
    .unwrap();

    let target: ComboTarget = serde_json::from_value(serde_json::json!({
        "id": 1,
        "combo_id": 1,
        "provider_id": "test",
        "priority_order": 0
    }))
    .unwrap();

    (combo, target)
}

fn make_test_tracker() -> UsageTracker {
    let conn_arc = Arc::new(parking_lot::Mutex::new(
        openproxy_db::testing::open_in_memory(),
    ));
    let repo = Arc::new(crate::repository::SqlitePipelineRepository::new(
        Arc::clone(&conn_arc),
    ));
    UsageTracker {
        conn: Arc::clone(&conn_arc),
        background_tx: tokio::sync::mpsc::channel(1).0,
        record_bodies_and_headers: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        selection_registry: Arc::new(openproxy_types::SelectionRegistry::new()),
        cooldown_secs: 60,
        cooldown_max_secs: 3600,
        cooldown_factor: 2,
        repo: Arc::clone(&repo) as Arc<dyn crate::repository::PipelineRepository>,
        coordinator: None,
    }
}

async fn pending_jobs(tracker: &UsageTracker) -> Vec<crate::worker::BackgroundJob> {
    let conn = Arc::clone(&tracker.conn);
    tokio::task::spawn_blocking(move || {
        openproxy_db::usage_journal::pending(&conn.lock(), 128)
            .unwrap()
            .into_iter()
            .map(|(_, payload)| serde_json::from_str(&payload).unwrap())
            .collect()
    })
    .await
    .unwrap()
}

#[test]
fn compute_completion_tokens_reported_by_upstream() {
    let tracker = make_test_tracker();
    let (combo, target) = make_test_combo_and_target();

    let mut builder = make_test_builder(&tracker, &combo, &target);
    builder.completion_tokens = Some(42);
    assert_eq!(builder.compute_completion_tokens(), (Some(42), false));
}

#[test]
fn compute_completion_tokens_fallback_for_tool_calls() {
    let tracker = make_test_tracker();
    let (combo, target) = make_test_combo_and_target();

    let mut builder = make_test_builder(&tracker, &combo, &target);
    builder.completion_tokens = None;
    builder.response_body_json = Some(serde_json::json!({
        "choices": [{
            "finish_reason": "tool_calls",
            "index": 0,
            "message": {
                "content": null,
                "role": "assistant",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "delegate",
                        "arguments": "{\"tasks\":[{\"context\":\"Investigación read-only\"}]}"
                    }
                }]
            }
        }]
    }));

    let (tokens, estimated) = builder.compute_completion_tokens();
    assert!(estimated);
    assert!(tokens.is_some_and(|t| t >= 10));
}

#[test]
fn compute_completion_tokens_zero_falls_back_to_tool_calls() {
    let tracker = make_test_tracker();
    let (combo, target) = make_test_combo_and_target();

    let mut builder = make_test_builder(&tracker, &combo, &target);
    builder.completion_tokens = Some(0);
    builder.response_body_json = Some(serde_json::json!({
        "choices": [{
            "finish_reason": "tool_calls",
            "index": 0,
            "message": {
                "content": null,
                "role": "assistant",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "search",
                        "arguments": "{\"query\": \"rust\"}"
                    }
                }]
            }
        }]
    }));

    let (tokens, estimated) = builder.compute_completion_tokens();
    assert!(estimated);
    assert!(tokens.is_some_and(|t| t >= 6));
}

#[tokio::test]
async fn test_concurrent_compression_stats_isolation() {
    let conn_arc = Arc::new(parking_lot::Mutex::new(
        rusqlite::Connection::open_in_memory().unwrap(),
    ));
    let cfg = crate::PipelineConfig {
        defaults: crate::timeouts::Timeouts::from_config(
            &openproxy_types::config::TimeoutsConfig::default(),
        ),
        racing: openproxy_types::config::RacingConfig::default(),
        retries: openproxy_types::config::RetriesConfig::default(),
        max_attempts: 1,
        master_key: Arc::new(openproxy_db::secrets::MasterKey::generate().unwrap()),
        adapters: Arc::new(vec![]),
        cooldown_secs: 60,
        cooldown_max_secs: 3600,
        cooldown_factor: 2,
        upstream_client: openproxy_adapters::upstream::UpstreamClient::new(),
        oauth_provider_registry: None,
        compression_mode: openproxy_compression::CompressionMode::Lite,
        idle_chunk_retryable: false,
        quota_protection: openproxy_types::config::QuotaProtectionConfig::default(),
        pii_config: openproxy_types::config::PiiConfig::default(),
        background_tx: tokio::sync::mpsc::channel(1).0,
    };
    let pipeline = crate::Pipeline::new(conn_arc, cfg);

    // Request 1: compressible (> 1000 chars with multiple redundant spaces)
    let compressible_content = "hello   world   this   is   a   test   ".repeat(50);
    let (_tx1, rx1) = tokio::sync::watch::channel(None);
    let req1 = PipelineRequest {
        request_id: RequestId::new(),
        trace_id: TraceId::new(),
        combo_id: ComboId(1),
        openai_request: Arc::new(OpenAIRequest {
            model: "model-compressible".into(),
            messages: vec![openproxy_types::OpenAIMessage {
                role: "user".into(),
                content: Some(serde_json::Value::String(compressible_content)),
                name: None,
                tool_calls: None,
                tool_call_id: None,
                extra: serde_json::Map::new(),
            }],
            stream: false,
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
        client_disconnected: rx1,
        stream_sink: None,
        api_key_id: None,
        combo_override: None,
        targets_override: None,
        request_headers: std::collections::BTreeMap::new(),
        request_body_json: None,
        race_cancelled: false,
        race_cancel: None,
        endpoint_kind: openproxy_types::endpoint::EndpointKind::Chat,
        compressed_messages: Arc::new(std::sync::OnceLock::new()),
        pii_session: Arc::new(parking_lot::Mutex::new(None)),
        compression_stats: Arc::new(parking_lot::Mutex::new(None)),
        proxy_override: None,
    };

    // Request 2: uncompressible (short message < 1000 chars)
    let (_tx2, rx2) = tokio::sync::watch::channel(None);
    let req2 = PipelineRequest {
        request_id: RequestId::new(),
        trace_id: TraceId::new(),
        combo_id: ComboId(1),
        openai_request: Arc::new(OpenAIRequest {
            model: "model-uncompressed".into(),
            messages: vec![openproxy_types::OpenAIMessage {
                role: "user".into(),
                content: Some(serde_json::Value::String("short text".into())),
                name: None,
                tool_calls: None,
                tool_call_id: None,
                extra: serde_json::Map::new(),
            }],
            stream: false,
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
        client_disconnected: rx2,
        stream_sink: None,
        api_key_id: None,
        combo_override: None,
        targets_override: None,
        request_headers: std::collections::BTreeMap::new(),
        request_body_json: None,
        race_cancelled: false,
        race_cancel: None,
        endpoint_kind: openproxy_types::endpoint::EndpointKind::Chat,
        compressed_messages: Arc::new(std::sync::OnceLock::new()),
        pii_session: Arc::new(parking_lot::Mutex::new(None)),
        compression_stats: Arc::new(parking_lot::Mutex::new(None)),
        proxy_override: None,
    };

    let ctx1 = crate::context::PipelineContext::new(req1.clone(), pipeline.clone());
    let ctx2 = crate::context::PipelineContext::new(req2.clone(), pipeline.clone());

    let task1 = tokio::spawn(async move {
        crate::stages::target::prepare_messages_for_formatting(&ctx1);
    });
    let task2 = tokio::spawn(async move {
        crate::stages::target::prepare_messages_for_formatting(&ctx2);
    });

    let (res1, res2) = tokio::join!(task1, task2);
    res1.unwrap();
    res2.unwrap();

    let stats1_guard = req1.compression_stats.lock();
    assert!(stats1_guard.is_some(), "req1 should have compression stats");
    let stats1 = stats1_guard.as_ref().unwrap();
    assert!(
        stats1.savings_pct_opt().is_some_and(|pct| pct > 0.0),
        "req1 must record positive compression savings pct"
    );
    assert!(
        stats1.techniques_csv().is_some(),
        "req1 must record applied techniques"
    );

    let stats2_guard = req2.compression_stats.lock();
    assert!(stats2_guard.is_some(), "req2 should have compression stats");
    let stats2 = stats2_guard.as_ref().unwrap();
    assert_eq!(
        stats2.savings_pct_opt(),
        None,
        "req2 must NOT have savings pct (zero cross-contamination)"
    );
    assert_eq!(
        stats2.techniques_csv(),
        None,
        "req2 must NOT have techniques (zero cross-contamination)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_high_concurrency_compression_stats_isolation() {
    let conn_arc = Arc::new(parking_lot::Mutex::new(
        rusqlite::Connection::open_in_memory().unwrap(),
    ));
    let cfg = crate::PipelineConfig {
        defaults: crate::timeouts::Timeouts::from_config(
            &openproxy_types::config::TimeoutsConfig::default(),
        ),
        racing: openproxy_types::config::RacingConfig::default(),
        retries: openproxy_types::config::RetriesConfig::default(),
        max_attempts: 1,
        master_key: Arc::new(openproxy_db::secrets::MasterKey::generate().unwrap()),
        adapters: Arc::new(vec![]),
        cooldown_secs: 60,
        cooldown_max_secs: 3600,
        cooldown_factor: 2,
        upstream_client: openproxy_adapters::upstream::UpstreamClient::new(),
        oauth_provider_registry: None,
        compression_mode: openproxy_compression::CompressionMode::Lite,
        idle_chunk_retryable: false,
        quota_protection: openproxy_types::config::QuotaProtectionConfig::default(),
        pii_config: openproxy_types::config::PiiConfig::default(),
        background_tx: tokio::sync::mpsc::channel(1).0,
    };
    let pipeline = crate::Pipeline::new(conn_arc, cfg);

    let num_requests = 100;
    let mut tasks = Vec::new();
    let mut reqs = Vec::new();

    for i in 0..num_requests {
        let is_compressible = i % 2 == 0;
        let content = if is_compressible {
            "hello   world   this   is   a   test   \n".repeat(50)
        } else {
            "short text".to_string()
        };

        let (_tx, rx) = tokio::sync::watch::channel(None);
        let req = PipelineRequest {
            request_id: RequestId::new(),
            trace_id: TraceId::new(),
            combo_id: ComboId(1),
            openai_request: Arc::new(OpenAIRequest {
                model: format!("model-{i}"),
                messages: vec![openproxy_types::OpenAIMessage {
                    role: "user".into(),
                    content: Some(serde_json::Value::String(content)),
                    name: None,
                    tool_calls: None,
                    tool_call_id: None,
                    extra: serde_json::Map::new(),
                }],
                stream: false,
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
            compressed_messages: Arc::new(std::sync::OnceLock::new()),
            pii_session: Arc::new(parking_lot::Mutex::new(None)),
            compression_stats: Arc::new(parking_lot::Mutex::new(None)),
            proxy_override: None,
        };

        let ctx = crate::context::PipelineContext::new(req.clone(), pipeline.clone());
        reqs.push((is_compressible, req));

        tasks.push(tokio::spawn(async move {
            crate::stages::target::prepare_messages_for_formatting(&ctx);
        }));
    }

    for t in tasks {
        t.await.unwrap();
    }

    for (i, (is_compressible, req)) in reqs.iter().enumerate() {
        let guard = req.compression_stats.lock();
        assert!(guard.is_some(), "request {i} must have stats recorded");
        let stats = guard.as_ref().unwrap();
        if *is_compressible {
            assert!(
                stats.savings_pct_opt().is_some_and(|p| p > 0.0),
                "req {i} (compressible) must have positive savings"
            );
            assert!(
                stats.techniques_csv().is_some(),
                "req {i} (compressible) must have techniques"
            );
        } else {
            assert_eq!(
                stats.savings_pct_opt(),
                None,
                "req {i} (uncompressible) must have NO savings"
            );
            assert_eq!(
                stats.techniques_csv(),
                None,
                "req {i} (uncompressible) must have NO techniques"
            );
        }
    }
}

#[tokio::test]
async fn test_compression_stats_empty_requests() {
    let conn_arc = Arc::new(parking_lot::Mutex::new(
        rusqlite::Connection::open_in_memory().unwrap(),
    ));
    let cfg = crate::PipelineConfig {
        defaults: crate::timeouts::Timeouts::from_config(
            &openproxy_types::config::TimeoutsConfig::default(),
        ),
        racing: openproxy_types::config::RacingConfig::default(),
        retries: openproxy_types::config::RetriesConfig::default(),
        max_attempts: 1,
        master_key: Arc::new(openproxy_db::secrets::MasterKey::generate().unwrap()),
        adapters: Arc::new(vec![]),
        cooldown_secs: 60,
        cooldown_max_secs: 3600,
        cooldown_factor: 2,
        upstream_client: openproxy_adapters::upstream::UpstreamClient::new(),
        oauth_provider_registry: None,
        compression_mode: openproxy_compression::CompressionMode::Lite,
        idle_chunk_retryable: false,
        quota_protection: openproxy_types::config::QuotaProtectionConfig::default(),
        pii_config: openproxy_types::config::PiiConfig::default(),
        background_tx: tokio::sync::mpsc::channel(1).0,
    };
    let pipeline = crate::Pipeline::new(conn_arc, cfg);
    let (_tx, rx) = tokio::sync::watch::channel(None);
    let req = PipelineRequest {
        request_id: RequestId::new(),
        trace_id: TraceId::new(),
        combo_id: ComboId(1),
        openai_request: Arc::new(OpenAIRequest {
            model: "test-model".into(),
            messages: vec![],
            stream: false,
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
        compressed_messages: Arc::new(std::sync::OnceLock::new()),
        pii_session: Arc::new(parking_lot::Mutex::new(None)),
        compression_stats: Arc::new(parking_lot::Mutex::new(None)),
        proxy_override: None,
    };

    let ctx = crate::context::PipelineContext::new(req.clone(), pipeline);
    let prepared = crate::stages::target::prepare_messages_for_formatting(&ctx);
    assert!(prepared.is_empty(), "prepared messages must be empty");

    let guard = req.compression_stats.lock();
    assert!(
        guard.is_some(),
        "empty request must initialize CompressionStats::empty"
    );
    let stats = guard.as_ref().unwrap();
    assert_eq!(stats.savings_pct_opt(), None);
    assert_eq!(stats.techniques_csv(), None);
}

#[tokio::test]
async fn test_compression_stats_failed_requests_and_cloned_retries() {
    let conn_arc = Arc::new(parking_lot::Mutex::new(
        rusqlite::Connection::open_in_memory().unwrap(),
    ));
    let cfg = crate::PipelineConfig {
        defaults: crate::timeouts::Timeouts::from_config(
            &openproxy_types::config::TimeoutsConfig::default(),
        ),
        racing: openproxy_types::config::RacingConfig::default(),
        retries: openproxy_types::config::RetriesConfig::default(),
        max_attempts: 1,
        master_key: Arc::new(openproxy_db::secrets::MasterKey::generate().unwrap()),
        adapters: Arc::new(vec![]),
        cooldown_secs: 60,
        cooldown_max_secs: 3600,
        cooldown_factor: 2,
        upstream_client: openproxy_adapters::upstream::UpstreamClient::new(),
        oauth_provider_registry: None,
        compression_mode: openproxy_compression::CompressionMode::Lite,
        idle_chunk_retryable: false,
        quota_protection: openproxy_types::config::QuotaProtectionConfig::default(),
        pii_config: openproxy_types::config::PiiConfig::default(),
        background_tx: tokio::sync::mpsc::channel(1).0,
    };
    let pipeline = crate::Pipeline::new(conn_arc, cfg);
    let (combo, target) = make_test_combo_and_target();
    let mut tracker = make_test_tracker();
    let (background_tx, mut background_rx) = tokio::sync::mpsc::channel(2);
    tracker.background_tx = background_tx;

    let (_tx, rx) = tokio::sync::watch::channel(None);
    let compressible_content = "hello   world   this   is   a   test   ".repeat(50);
    let req = PipelineRequest {
        request_id: RequestId::new(),
        trace_id: TraceId::new(),
        combo_id: ComboId(1),
        openai_request: Arc::new(OpenAIRequest {
            model: "test-model".into(),
            messages: vec![openproxy_types::OpenAIMessage {
                role: "user".into(),
                content: Some(serde_json::Value::String(compressible_content)),
                name: None,
                tool_calls: None,
                tool_call_id: None,
                extra: serde_json::Map::new(),
            }],
            stream: false,
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
        compressed_messages: Arc::new(std::sync::OnceLock::new()),
        pii_session: Arc::new(parking_lot::Mutex::new(None)),
        compression_stats: Arc::new(parking_lot::Mutex::new(None)),
        proxy_override: None,
    };

    // Attempt 1: Prepares messages, encounters failure
    let ctx1 = crate::context::PipelineContext::new(req.clone(), pipeline.clone());
    let _ = crate::stages::target::prepare_messages_for_formatting(&ctx1);
    {
        let guard = req.compression_stats.lock();
        assert!(guard.is_some(), "attempt 1 must populate compression_stats");
        assert!(guard.as_ref().unwrap().savings_pct_opt().is_some());
    }

    // Record failure on Attempt 1
    let mut builder1 = UsageRecordBuilder::new(&tracker, req.clone(), &combo, &target);
    let err = CoreError::UpstreamTimeout {
        phase: "connect".to_string(),
        ms: 100,
    };
    builder1 = builder1.err(&err);
    let res1 = builder1.record().await;
    assert!(res1.is_ok(), "recording failure must succeed");

    // Attempt 2 (Retry on next target with cloned request)
    let cloned_req = req;
    let ctx2 = crate::context::PipelineContext::new(cloned_req.clone(), pipeline);
    let _ = crate::stages::target::prepare_messages_for_formatting(&ctx2);

    // Verify stats were preserved across retry without re-allocating or zeroing
    {
        let stats_lock = Arc::clone(&cloned_req.compression_stats);
        let guard2 = stats_lock.lock();
        assert!(
            guard2.is_some(),
            "cloned request retains compression stats across retries"
        );
        assert!(
            guard2.as_ref().unwrap().savings_pct_opt().is_some(),
            "savings pct must remain positive on retry"
        );
    }

    // Record success on Attempt 2
    let mut target2 = target.clone();
    target2.id = openproxy_types::ids::ComboTargetId(2);
    let mut builder2 = UsageRecordBuilder::new(&tracker, cloned_req, &combo, &target2);
    builder2.prompt_tokens = Some(50);
    builder2.completion_tokens = Some(10);
    let res2 = builder2.record().await;
    assert!(res2.is_ok(), "recording success on retry must succeed");
    assert!(background_rx.try_recv().is_ok());
    assert!(background_rx.try_recv().is_ok());
}

#[tokio::test]
async fn recording_waits_for_capacity_and_preserves_order() {
    let mut tracker = make_test_tracker();
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    tracker.background_tx = sender;
    let (combo, target) = make_test_combo_and_target();
    let tuple = make_test_builder(&tracker, &combo, &target)
        .record()
        .await
        .unwrap()
        .unwrap();
    let mark = tracker.mark_client_response(Some(tuple));
    tokio::pin!(mark);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), &mut mark)
            .await
            .is_err()
    );
    assert!(matches!(
        receiver.recv().await,
        Some(crate::worker::BackgroundJob::JournalWake)
    ));
    mark.await;
    assert!(matches!(
        receiver.recv().await,
        Some(crate::worker::BackgroundJob::JournalWake)
    ));
    let jobs = pending_jobs(&tracker).await;
    assert_eq!(jobs.len(), 2);
    assert!(matches!(
        jobs.first(),
        Some(crate::worker::BackgroundJob::RecordAttempt { .. })
    ));
    assert!(matches!(
        jobs.get(1),
        Some(crate::worker::BackgroundJob::MarkClientResponse { .. })
    ));
}

#[tokio::test]
async fn closed_usage_worker_is_reported_to_the_builder() {
    let tracker = make_test_tracker();
    let (combo, target) = make_test_combo_and_target();
    assert!(
        make_test_builder(&tracker, &combo, &target)
            .record()
            .await
            .is_err()
    );
}

#[tokio::test]
async fn selection_reputation_is_updated_once_per_attempt() {
    let mut tracker = make_test_tracker();
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    tracker.background_tx = sender;
    let (combo, target) = make_test_combo_and_target();
    make_test_builder(&tracker, &combo, &target)
        .record()
        .await
        .unwrap();
    let job = receiver.recv().await.unwrap();
    assert!(matches!(job, crate::worker::BackgroundJob::JournalWake));
    let conn = Arc::clone(&tracker.conn);
    tokio::task::spawn_blocking(move || crate::worker::process_job(&conn, job))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        tracker
            .selection_registry
            .target_metrics(target.id)
            .success_count,
        1
    );
}

#[tokio::test]
async fn usage_worker_drains_accepted_jobs_and_marks_winner() {
    let mut tracker = make_test_tracker();
    let conn = Arc::clone(&tracker.conn);
    let (sender, receiver) = tokio::sync::mpsc::channel(4);
    let worker = crate::worker::spawn_worker(Arc::clone(&conn), receiver);
    tracker.background_tx = sender;
    let (combo, target) = make_test_combo_and_target();
    let tuple = make_test_builder(&tracker, &combo, &target)
        .record()
        .await
        .unwrap();
    tracker.mark_client_response(tuple).await;
    for _ in 0..96 {
        make_test_builder(&tracker, &combo, &target)
            .record()
            .await
            .unwrap();
    }
    worker.shutdown().await.unwrap();
    assert!(tracker.background_tx.is_closed());
    let (count, winners): (i64, i64) = tokio::task::spawn_blocking(move || {
        conn.lock()
            .query_row(
                "SELECT count(*), sum(client_response) FROM usage",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
    })
    .await
    .unwrap();
    assert_eq!((count, winners), (97, 1));
    assert!(pending_jobs(&tracker).await.is_empty());
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn usage_worker_surfaces_persistence_failures_at_shutdown() {
    let mut tracker = make_test_tracker();
    let conn = Arc::clone(&tracker.conn);
    tokio::task::spawn_blocking(move || {
        conn.lock()
            .execute_batch(
                "CREATE TRIGGER reject_usage BEFORE INSERT ON usage
             BEGIN SELECT RAISE(FAIL, 'usage persistence rejected'); END;",
            )
            .unwrap();
    })
    .await
    .unwrap();
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    let worker = crate::worker::spawn_worker(Arc::clone(&tracker.conn), receiver);
    tracker.background_tx = sender;
    let (combo, target) = make_test_combo_and_target();
    make_test_builder(&tracker, &combo, &target)
        .record()
        .await
        .unwrap();
    assert!(worker.shutdown().await.is_err());
    assert_eq!(pending_jobs(&tracker).await.len(), 1);
}
