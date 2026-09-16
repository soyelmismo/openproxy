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
        rusqlite::Connection::open_in_memory().unwrap(),
    ));
    let repo = Arc::new(crate::repository::SqlitePipelineRepository::new(
        Arc::clone(&conn_arc),
    ));
    UsageTracker {
        conn: Arc::clone(&conn_arc),
        background_tx: tokio::sync::mpsc::channel(1).0,
        record_bodies_and_headers: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        compression_stats_cell: Arc::new(parking_lot::RwLock::new(None)),
        selection_registry: Arc::new(openproxy_types::SelectionRegistry::new()),
        cooldown_secs: 60,
        cooldown_max_secs: 3600,
        cooldown_factor: 2,
        repo: Arc::clone(&repo) as Arc<dyn crate::repository::PipelineRepository>,
    }
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
