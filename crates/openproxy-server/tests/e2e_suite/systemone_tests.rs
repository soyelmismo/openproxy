#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::harness::TestHarness;
use axum::body::Body;
use axum::http::{Method, StatusCode};
use openproxy_core::admin::{self, AddTargetInput, CreateComboInput, CreateCustomModelInput};
use openproxy_core::usage;
use serde_json::json;
use std::time::Duration;

#[tokio::test]
async fn test_systemone_direct_call_success_and_records_usage() {
    let harness = TestHarness::new().await;

    // Register a systemone custom model
    {
        let w = harness.db_pool.writer();
        admin::create_custom_model(
            &w,
            CreateCustomModelInput {
                provider_id: harness.provider_id.as_str().into(),
                model_id: "mock-jev".into(),
                display_name: Some("Mock Jev".into()),
                target_format: "systemone".into(),
                ttl_seconds: 3600,
                model_type: Some("decision".into()),
            },
        )
        .expect("create mock-jev model");
    }

    let payload = json!({
        "model": "mock-jev",
        "state": "The user wants a fast and simple calculation.",
        "questions": {
            "intent": {
                "type": "choice",
                "instructions": "Determine intent",
                "options": ["fast_math", "deep_reasoning"]
            },
            "notes": {
                "type": "noul",
                "instructions": "Additional notes"
            }
        }
    });

    let (status, resp) = harness.client_systemone_call(payload).await;
    assert_eq!(status, StatusCode::OK, "direct systemone call should return 200: {resp}");

    assert!(resp.get("answers").is_some());
    let answers = resp["answers"].as_object().expect("answers map");
    assert!(answers.contains_key("intent"));
    assert_eq!(answers["intent"]["choice"], "fast_math");
    assert_eq!(answers["intent"]["probabilities"]["fast_math"], 0.98);
    assert_eq!(answers["notes"]["response"], "mock answer");

    let usage = resp["usage"].as_object().expect("usage object");
    assert_eq!(usage["input_tokens"], 15);
    assert_eq!(usage["output_tokens"], 5);
    assert_eq!(usage["total_tokens"], 20);

    // Verify usage row is recorded in database
    let r = harness.db_pool.reader();
    let rows = usage::recent_desc(&r, 10).expect("query recent usage");
    assert!(!rows.is_empty(), "a usage row should be recorded for systemone");
    let last = &rows[0];
    assert_eq!(last.status_code, 200);
    assert_eq!(last.upstream_model_id, "mock-jev");
    assert_eq!(last.prompt_tokens, Some(15));
    assert_eq!(last.completion_tokens, Some(5));
}

#[tokio::test]
async fn test_systemone_missing_auth_returns_unauthorized() {
    let harness = TestHarness::new().await;

    let payload = json!({
        "model": "mock-jev",
        "state": "test prompt",
        "questions": {
            "choice": {
                "type": "choice",
                "instructions": "Make choice",
                "options": ["a", "b"]
            }
        }
    });

    // Request with no Authorization header
    let req = axum::http::Request::builder()
        .method(Method::POST)
        .uri("/v1/systemone")
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(payload.to_string()))
        .expect("build request");

    let (status, _) = harness.oneshot_json(req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_systemone_upstream_error_handling() {
    let harness = TestHarness::new().await;

    {
        let w = harness.db_pool.writer();
        admin::create_custom_model(
            &w,
            CreateCustomModelInput {
                provider_id: harness.provider_id.as_str().into(),
                model_id: "mock-jev-err".into(),
                display_name: Some("Mock Jev Err".into()),
                target_format: "systemone".into(),
                ttl_seconds: 3600,
                model_type: Some("decision".into()),
            },
        )
        .expect("create mock-jev-err model");
    }

    harness
        .mock_handle
        .set_error(StatusCode::BAD_GATEWAY, Some("{\"error\": \"mock gateway error\"}"));

    let payload = json!({
        "model": "mock-jev-err",
        "state": "error test",
        "questions": {
            "cat": {
                "type": "choice",
                "instructions": "Classify",
                "options": ["x", "y"]
            }
        }
    });

    let (status, _) = harness.client_systemone_call(payload).await;
    assert!(status.is_server_error() || status.is_client_error(), "should return error status");

    harness.mock_handle.clear_error();
}

#[tokio::test]
async fn test_combo_decision_routing_simple_vs_complex() {
    let harness = TestHarness::new().await;

    // Create decision model, fast model, and deep model
    {
        let w = harness.db_pool.writer();
        admin::create_custom_model(
            &w,
            CreateCustomModelInput {
                provider_id: harness.provider_id.as_str().into(),
                model_id: "mock-decision-model".into(),
                display_name: Some("Decision Jev".into()),
                target_format: "systemone".into(),
                ttl_seconds: 3600,
                model_type: Some("decision".into()),
            },
        )
        .expect("create decision model");

        let fast_row_id = admin::create_custom_model(
            &w,
            CreateCustomModelInput {
                provider_id: harness.provider_id.as_str().into(),
                model_id: "mock-fast".into(),
                display_name: Some("Fast Model".into()),
                target_format: "openai".into(),
                ttl_seconds: 3600,
                model_type: Some("chat".into()),
            },
        )
        .expect("create mock-fast");

        let deep_row_id = admin::create_custom_model(
            &w,
            CreateCustomModelInput {
                provider_id: harness.provider_id.as_str().into(),
                model_id: "mock-deep".into(),
                display_name: Some("Deep Model".into()),
                target_format: "openai".into(),
                ttl_seconds: 3600,
                model_type: Some("chat".into()),
            },
        )
        .expect("create mock-deep");

        let combo_id = admin::create_combo(
            &w,
            &CreateComboInput {
                name: "smart-combo".into(),
                strategy: "priority".into(),
                race_size: None,
                priority_mode: Some("decision".into()),
                decision_model: Some(format!("{}/mock-decision-model", harness.provider_id)),
                decision_timeout_ms: Some(500),
                cooldown_mode: None,
                cooldown_base_secs: None,
                cooldown_max_secs: None,
                cooldown_factor: None,
                lkgp_exploration_rate: None,
                selection_window_secs: None,
            },
        )
        .expect("create combo");

        admin::add_target_to_combo(
            &w,
            combo_id,
            AddTargetInput {
                provider_id: harness.provider_id.as_str().into(),
                account_id: None,
                model_row_id: Some(fast_row_id),
                sub_combo_id: None,
                priority_order: 1,
                description: Some("Fast simple questions and casual conversation".into()),
            },
        )
        .expect("add fast target");

        admin::add_target_to_combo(
            &w,
            combo_id,
            AddTargetInput {
                provider_id: harness.provider_id.as_str().into(),
                account_id: None,
                model_row_id: Some(deep_row_id),
                sub_combo_id: None,
                priority_order: 2,
                description: Some("Complex deep coding in Rust and advanced architecture".into()),
            },
        )
        .expect("add deep target");
    }

    harness.mock_handle.clear_recorded_requests();

    // 1) Test simple prompt -> routes to mock-fast
    let (status, resp) = harness
        .client_chat_call("smart-combo", "Hello, how is your day?", false)
        .await;
    assert_eq!(status, StatusCode::OK, "chat call to combo should succeed: {resp}");
    assert_eq!(resp["object"], "chat.completion");

    let rec = harness.mock_handle.recorded_requests();
    let chat_reqs: Vec<_> = rec.iter().filter(|r| r.path.contains("chat/completions")).collect();
    assert!(!chat_reqs.is_empty(), "must record chat upstream request");
    let model_dispatched = chat_reqs.last().unwrap().body["model"].as_str().unwrap();
    assert_eq!(model_dispatched, "mock-fast", "simple prompt must route to mock-fast");

    harness.mock_handle.clear_recorded_requests();

    // 2) Test complex prompt -> routes to mock-deep
    let (status, resp) = harness
        .client_chat_call("smart-combo", "Write a complex async parser in Rust", false)
        .await;
    assert_eq!(status, StatusCode::OK, "chat call to combo should succeed: {resp}");
    assert_eq!(resp["object"], "chat.completion");

    let rec2 = harness.mock_handle.recorded_requests();
    let chat_reqs2: Vec<_> = rec2.iter().filter(|r| r.path.contains("chat/completions")).collect();
    assert!(!chat_reqs2.is_empty(), "must record chat upstream request");
    let model_dispatched2 = chat_reqs2.last().unwrap().body["model"].as_str().unwrap();
    assert_eq!(model_dispatched2, "mock-deep", "complex prompt must route to mock-deep");
}

#[tokio::test]
async fn test_combo_decision_fallback_on_timeout() {
    let harness = TestHarness::new().await;

    // Create combo with tight timeout
    {
        let w = harness.db_pool.writer();
        admin::create_custom_model(
            &w,
            CreateCustomModelInput {
                provider_id: harness.provider_id.as_str().into(),
                model_id: "mock-slow-jev".into(),
                display_name: Some("Slow Jev".into()),
                target_format: "systemone".into(),
                ttl_seconds: 3600,
                model_type: Some("decision".into()),
            },
        )
        .expect("create slow decision model");

        let default_row_id = admin::create_custom_model(
            &w,
            CreateCustomModelInput {
                provider_id: harness.provider_id.as_str().into(),
                model_id: "mock-default-model".into(),
                display_name: Some("Default Model".into()),
                target_format: "openai".into(),
                ttl_seconds: 3600,
                model_type: Some("chat".into()),
            },
        )
        .expect("create default model");

        let combo_id = admin::create_combo(
            &w,
            &CreateComboInput {
                name: "timeout-combo".into(),
                strategy: "priority".into(),
                race_size: None,
                priority_mode: Some("decision".into()),
                decision_model: Some(format!("{}/mock-slow-jev", harness.provider_id)),
                decision_timeout_ms: Some(50),
                cooldown_mode: None,
                cooldown_base_secs: None,
                cooldown_max_secs: None,
                cooldown_factor: None,
                lkgp_exploration_rate: None,
                selection_window_secs: None,
            },
        )
        .expect("create combo");

        admin::add_target_to_combo(
            &w,
            combo_id,
            AddTargetInput {
                provider_id: harness.provider_id.as_str().into(),
                account_id: None,
                model_row_id: Some(default_row_id),
                sub_combo_id: None,
                priority_order: 1,
                description: Some("Default target".into()),
            },
        )
        .expect("add target");
    }

    // Set mock delay of 250ms (exceeding decision_timeout_ms of 50ms)
    harness.mock_handle.set_delay(Duration::from_millis(250));

    let (status, resp) = harness
        .client_chat_call("timeout-combo", "Fallback check prompt", false)
        .await;

    // Must still succeed via graceful fallback to default target
    assert_eq!(status, StatusCode::OK, "fallback should succeed: {resp}");
    assert_eq!(resp["object"], "chat.completion");

    harness.mock_handle.clear_delay();
}

#[tokio::test]
async fn test_admin_model_tester_decision_model() {
    let harness = TestHarness::new().await;

    let model_row_id = {
        let w = harness.db_pool.writer();
        admin::create_custom_model(
            &w,
            CreateCustomModelInput {
                provider_id: harness.provider_id.as_str().into(),
                model_id: "jev-test-model".into(),
                display_name: Some("Jev Test Model".into()),
                target_format: "systemone".into(),
                ttl_seconds: 3600,
                model_type: Some("decision".into()),
            },
        )
        .expect("create jev model")
    };

    harness.mock_handle.clear_recorded_requests();

    // Call POST /admin/api/models/:id/test (dashboard test button backend)
    let (status, resp) = harness
        .admin_post(&format!("/admin/api/models/{model_row_id}/test"), json!({}))
        .await;

    assert_eq!(status, StatusCode::OK, "test endpoint should return 200: {resp}");
    assert_eq!(resp["status"], 200);

    // Verify debug_payload contains the real System One request and response
    let debug = &resp["debug_payload"];
    assert!(debug["request_url"].as_str().unwrap().contains("/systemone"));
    assert_eq!(debug["request_body"]["model"], "jev-test-model");
    assert!(debug["request_body"]["questions"]["status"].is_object());
    assert_eq!(debug["response_body"]["answers"]["status"]["choice"], "ok");

    // Verify upstream mock recorded the systemone request
    let recorded = harness.mock_handle.recorded_requests();
    let sys_req = recorded
        .iter()
        .find(|r| r.path.contains("systemone"))
        .expect("must record /systemone request");
    assert_eq!(sys_req.body["model"], "jev-test-model");
}

#[tokio::test]
async fn test_systemone_routing_supports_provider_prefixed_and_combo_models() {
    let harness = TestHarness::new().await;

    let _model_row_id = {
        let w = harness.db_pool.writer();
        let mid = admin::create_custom_model(
            &w,
            CreateCustomModelInput {
                provider_id: harness.provider_id.as_str().into(),
                model_id: "jev-routed".into(),
                display_name: Some("Jev Routed".into()),
                target_format: "systemone".into(),
                ttl_seconds: 3600,
                model_type: Some("decision".into()),
            },
        )
        .expect("create jev-routed model");

        let combo_id = admin::create_combo(
            &w,
            &CreateComboInput {
                name: "decision-combo".into(),
                strategy: "priority".into(),
                race_size: None,
                priority_mode: None,
                decision_model: None,
                decision_timeout_ms: None,
                cooldown_mode: None,
                cooldown_base_secs: None,
                cooldown_max_secs: None,
                cooldown_factor: None,
                lkgp_exploration_rate: None,
                selection_window_secs: None,
            },
        )
        .expect("create decision-combo");

        admin::add_target_to_combo(
            &w,
            combo_id,
            AddTargetInput {
                provider_id: harness.provider_id.as_str().into(),
                account_id: None,
                model_row_id: Some(mid),
                sub_combo_id: None,
                priority_order: 1,
                description: None,
            },
        )
        .expect("add target to combo");

        mid
    };

    let sample_payload = |model_name: &str| {
        json!({
            "model": model_name,
            "state": "System is operational and performing tasks.",
            "questions": {
                "health": {
                    "type": "choice",
                    "instructions": "Verify health status",
                    "criteria": {
                        "ok": "System is operational",
                        "error": "System has failed"
                    }
                }
            }
        })
    };

    // 1) Test provider/model resolution
    let provider_model = format!("{}/jev-routed", harness.provider_id);
    let (status1, resp1) = harness.client_systemone_call(sample_payload(&provider_model)).await;
    assert_eq!(status1, StatusCode::OK, "provider/model systemone call should return 200: {resp1}");
    assert_eq!(resp1["answers"]["health"]["choice"], "ok");

    // 2) Test combo resolution
    let (status2, resp2) = harness.client_systemone_call(sample_payload("decision-combo")).await;
    assert_eq!(status2, StatusCode::OK, "combo systemone call should return 200: {resp2}");
    assert_eq!(resp2["answers"]["health"]["choice"], "ok");

    // Verify usage records row for combo dispatch
    let r = harness.db_pool.reader();
    let rows = usage::recent_desc(&r, 10).expect("fetch usage");
    let combo_row = rows
        .iter()
        .find(|r| r.upstream_model_id == "jev-routed")
        .expect("must record usage row for jev-routed");
    assert_eq!(combo_row.status_code, 200);
    assert_eq!(combo_row.endpoint_kind, openproxy_types::EndpointKind::SystemOne);
}

