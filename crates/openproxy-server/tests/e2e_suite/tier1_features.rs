#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::harness::TestHarness;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use openproxy_compression::{CompressionMode, apply_compression, would_compress};
use openproxy_types::OpenAIMessage;
use serde_json::{Value, json};

fn msg(role: &str, content: &str) -> OpenAIMessage {
    OpenAIMessage {
        role: role.into(),
        content: Some(json!(content)),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        extra: Default::default(),
    }
}

#[tokio::test]
async fn test_tier1_chat_completions_basic_success() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness
        .client_chat_call("mock-gpt-4", "Hello world", false)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["object"], "chat.completion");
    assert_eq!(resp["choices"][0]["message"]["content"], "Hello from mock!");
}

#[tokio::test]
async fn test_tier1_chat_completions_system_and_user_messages() {
    let harness = TestHarness::new().await;
    let payload = json!({"model": "mock-gpt-4", "messages": [{"role": "system", "content": "You are a helpful assistant."}, {"role": "user", "content": "Summarize this request."}], "stream": false});
    let (status, _, body) = harness.client_chat_raw(payload).await;
    assert_eq!(status, StatusCode::OK);
    let resp: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(resp["choices"][0]["finish_reason"], "stop");
    assert!(!harness.mock_handle.recorded_requests().is_empty());
}

#[tokio::test]
async fn test_tier1_chat_completions_custom_model_routing() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness
        .client_chat_call("mock-gpt-4", "Test custom model", false)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["model"], "mock-gpt-4");
}

#[tokio::test]
async fn test_tier1_chat_completions_response_format_json() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness
        .client_chat_call("mock-gpt-4", "Validate JSON schema", false)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        resp.get("id").is_some()
            && resp.get("created").is_some()
            && resp.get("choices").and_then(Value::as_array).is_some()
            && resp.get("usage").is_some()
    );
}

#[tokio::test]
async fn test_tier1_chat_completions_token_accounting() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness
        .client_chat_call("mock-gpt-4", "Check tokens", false)
        .await;
    assert_eq!(status, StatusCode::OK);
    let usage = &resp["usage"];
    assert!(
        usage["prompt_tokens"].as_u64().unwrap_or(0) > 0
            && usage["completion_tokens"].as_u64().unwrap_or(0) > 0
    );
    assert!(
        usage["total_tokens"].as_u64().unwrap_or(0) >= usage["prompt_tokens"].as_u64().unwrap()
    );
}

#[tokio::test]
async fn test_tier1_models_list_openai_envelope() {
    let harness = TestHarness::new().await;
    let req = harness.client_request(Method::GET, "/v1/models", Body::empty());
    let (status, resp) = harness.oneshot_json(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["object"], "list");
    let data = resp["data"].as_array().expect("data array");
    assert!(
        data.iter()
            .any(|m| m["id"].as_str().is_some_and(|s| s.contains("mock-gpt-4")))
    );
}

#[tokio::test]
async fn test_tier1_models_list_anthropic_format() {
    let harness = TestHarness::new().await;
    let mut req = harness.client_request(Method::GET, "/v1/models", Body::empty());
    req.headers_mut().insert(
        header::HeaderName::from_static("anthropic-version"),
        header::HeaderValue::from_static("2023-06-01"),
    );
    let (status, resp) = harness.oneshot_json(req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp.get("has_more").is_some());
    assert!(
        resp["data"]
            .as_array()
            .expect("data array")
            .iter()
            .any(|m| m["type"] == "model")
    );
}

#[tokio::test]
async fn test_tier1_models_list_includes_combos() {
    let harness = TestHarness::new().await;
    let (status, _) = harness
        .admin_post(
            "/admin/api/combos",
            json!({"name": "tier1-smart-combo", "strategy": "priority"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let req = harness.client_request(Method::GET, "/v1/models", Body::empty());
    let (status, resp) = harness.oneshot_json(req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        resp["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == "combo:tier1-smart-combo" && m["owned_by"] == "combo")
    );
}

#[tokio::test]
async fn test_tier1_models_list_key_scoped_filtering() {
    let harness = TestHarness::new().await;
    let (status, new_key) = harness
        .admin_post(
            "/admin/api/keys",
            json!({"label": "scoped-key", "scopes": ["chat"], "allowed_models": ["mock-gpt-4"]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let plaintext = new_key["plaintext"].as_str().unwrap();
    let req = Request::builder()
        .method(Method::GET)
        .uri("/v1/models")
        .header(header::AUTHORIZATION, format!("Bearer {plaintext}"))
        .body(Body::empty())
        .unwrap();
    let (status, resp) = harness.oneshot_json(req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        resp["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["id"].as_str().is_some_and(|s| s.contains("mock-gpt-4")))
    );
}

#[tokio::test]
async fn test_tier1_models_list_metadata_fields() {
    let harness = TestHarness::new().await;
    let req = harness.client_request(Method::GET, "/v1/models", Body::empty());
    let (status, resp) = harness.oneshot_json(req).await;
    assert_eq!(status, StatusCode::OK);
    let model = resp["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"].as_str().is_some_and(|s| s.contains("mock-gpt-4")))
        .unwrap();
    assert_eq!(model["object"], "model");
    assert!(model.get("created").is_some());
}

#[tokio::test]
async fn test_tier1_embeddings_single_input_success() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness
        .client_embeddings_call("mock-embed", json!("semantic search string"))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["object"], "list");
    assert_eq!(resp["data"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn test_tier1_embeddings_multiple_inputs() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness
        .client_embeddings_call(
            "mock-embed",
            json!(["text snippet one", "text snippet two", "text snippet three"]),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["data"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn test_tier1_embeddings_empty_input_validation() {
    let harness = TestHarness::new().await;
    let (status, _) = harness
        .client_embeddings_call("mock-embed", json!([]))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_tier1_embeddings_vector_structure() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness
        .client_embeddings_call("mock-embed", json!("vector test"))
        .await;
    assert_eq!(status, StatusCode::OK);
    let vec = resp["data"][0]["embedding"].as_array().unwrap();
    assert!(!vec.is_empty() && vec[0].is_number());
}

#[tokio::test]
async fn test_tier1_embeddings_usage_accounting() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness
        .client_embeddings_call("mock-embed", json!("check usage"))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        resp["usage"]["prompt_tokens"].as_u64().unwrap_or(0) > 0
            && resp["usage"]["total_tokens"].as_u64().unwrap_or(0) > 0
    );
}

#[tokio::test]
async fn test_tier1_admin_providers_list() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness.admin_get("/admin/api/providers").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        resp.as_array()
            .expect("providers array")
            .iter()
            .any(|p| p["id"] == "mock-prov")
    );
}

#[tokio::test]
async fn test_tier1_admin_providers_create() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness.admin_post("/admin/api/providers", json!({"id": "tier1-new-prov", "name": "New Provider", "base_url": "http://127.0.0.1:9999/v1", "auth_type": "bearer", "format": "openai"})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["id"], "tier1-new-prov");
}

#[tokio::test]
async fn test_tier1_admin_providers_get_by_id() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness.admin_get("/admin/api/providers/mock-prov").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["id"], "mock-prov");
}

#[tokio::test]
async fn test_tier1_admin_providers_update() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness
        .admin_patch(
            "/admin/api/providers/mock-prov",
            json!({"name": "Updated Mock Name"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["id"], "mock-prov");
    let (_, fetched) = harness.admin_get("/admin/api/providers/mock-prov").await;
    assert_eq!(fetched["name"], "Updated Mock Name");
}

#[tokio::test]
async fn test_tier1_admin_providers_delete() {
    let harness = TestHarness::new().await;
    let _ = harness.admin_post("/admin/api/providers", json!({"id": "prov-to-delete", "name": "Deletable Provider", "base_url": "http://127.0.0.1:8888/v1", "auth_type": "bearer", "format": "openai"})).await;
    let (status, _) = harness
        .admin_delete("/admin/api/providers/prov-to-delete")
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status_after, _) = harness
        .admin_get("/admin/api/providers/prov-to-delete")
        .await;
    assert_eq!(status_after, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_tier1_admin_models_list() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness.admin_get("/admin/api/models").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        resp.as_array()
            .unwrap()
            .iter()
            .any(|m| m["model_id"] == "mock-gpt-4")
    );
}

#[tokio::test]
async fn test_tier1_admin_models_create_custom() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness.admin_post("/admin/api/models/custom", json!({"provider_id": "mock-prov", "model_id": "mock-custom-tier1", "display_name": "Custom Model Tier 1", "target_format": "openai", "ttl_seconds": 3600})).await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp.get("row_id").or_else(|| resp.get("id")).is_some());
}

#[tokio::test]
async fn test_tier1_admin_models_toggle_active() {
    let harness = TestHarness::new().await;
    let (status, models) = harness.admin_get("/admin/api/models").await;
    assert_eq!(status, StatusCode::OK);
    let row_id = models[0]
        .get("row_id")
        .or_else(|| models[0].get("id"))
        .unwrap()
        .as_i64()
        .unwrap();
    let (status_toggle, resp) = harness
        .admin_post(
            &format!("/admin/api/models/{row_id}/toggle"),
            json!({"active": false}),
        )
        .await;
    assert_eq!(status_toggle, StatusCode::OK);
    assert_eq!(resp["active"], false);
}

#[tokio::test]
async fn test_tier1_admin_models_bulk_toggle() {
    let harness = TestHarness::new().await;
    let (status_bulk, resp) = harness
        .admin_post(
            "/admin/api/models/bulk-toggle",
            json!({"provider_id": "mock-prov", "active": true}),
        )
        .await;
    assert_eq!(status_bulk, StatusCode::OK);
    assert!(resp.get("updated").is_some());
}

#[tokio::test]
async fn test_tier1_admin_models_refresh_trigger() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness
        .admin_post("/admin/api/providers/mock-prov/refresh", json!({}))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp.get("models_refreshed").is_some());
}

#[tokio::test]
async fn test_tier1_admin_combos_list() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness.admin_get("/admin/api/combos").await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp.is_array());
}

#[tokio::test]
async fn test_tier1_admin_combos_create() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness
        .admin_post(
            "/admin/api/combos",
            json!({"name": "combo-tier1-roundrobin", "strategy": "round_robin"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp.get("id").is_some());
}

#[tokio::test]
async fn test_tier1_admin_combos_add_targets() {
    let harness = TestHarness::new().await;
    let (_, combo) = harness
        .admin_post(
            "/admin/api/combos",
            json!({"name": "combo-with-target", "strategy": "priority"}),
        )
        .await;
    let combo_id = combo["id"].as_i64().unwrap();
    let (_, models) = harness.admin_get("/admin/api/models").await;
    let model_row_id = models[0]
        .get("row_id")
        .or_else(|| models[0].get("id"))
        .unwrap()
        .as_i64()
        .unwrap();
    let (status, resp) = harness
        .admin_post(
            &format!("/admin/api/combos/{combo_id}/targets"),
            json!({"provider_id": "mock-prov", "model_row_id": model_row_id, "priority_order": 1}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp.get("id").is_some());
}

#[tokio::test]
async fn test_tier1_admin_combos_reorder_targets() {
    let harness = TestHarness::new().await;
    let (_, combo) = harness
        .admin_post(
            "/admin/api/combos",
            json!({"name": "combo-reorder", "strategy": "priority"}),
        )
        .await;
    let combo_id = combo["id"].as_i64().unwrap();
    let (_, models) = harness.admin_get("/admin/api/models").await;
    let m1 = models[0]
        .get("row_id")
        .or_else(|| models[0].get("id"))
        .unwrap()
        .as_i64()
        .unwrap();
    let m2 = models.get(1).map_or(m1, |m| {
        m.get("row_id")
            .or_else(|| m.get("id"))
            .unwrap()
            .as_i64()
            .unwrap()
    });
    let (_, t1) = harness
        .admin_post(
            &format!("/admin/api/combos/{combo_id}/targets"),
            json!({"provider_id": "mock-prov", "model_row_id": m1, "priority_order": 1}),
        )
        .await;
    let (_, t2) = harness
        .admin_post(
            &format!("/admin/api/combos/{combo_id}/targets"),
            json!({"provider_id": "mock-prov", "model_row_id": m2, "priority_order": 2}),
        )
        .await;
    let (status, resp) = harness
        .admin_post(
            &format!("/admin/api/combos/{combo_id}/targets/reorder"),
            json!({"target_ids": vec![t2["id"].as_i64().unwrap(), t1["id"].as_i64().unwrap()]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp.get("reordered").is_some());
}

#[tokio::test]
async fn test_tier1_admin_combos_delete() {
    let harness = TestHarness::new().await;
    let (_, combo) = harness
        .admin_post(
            "/admin/api/combos",
            json!({"name": "combo-to-delete", "strategy": "priority"}),
        )
        .await;
    let combo_id = combo["id"].as_i64().unwrap();
    let (status, _) = harness
        .admin_delete(&format!("/admin/api/combos/{combo_id}"))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status_after, _) = harness
        .admin_get(&format!("/admin/api/combos/{combo_id}"))
        .await;
    assert_eq!(status_after, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_tier1_admin_usage_summary() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness.admin_get("/admin/api/usage/summary").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        resp.get("unique_requests")
            .or_else(|| resp.get("total_rows"))
            .is_some()
    );
}

#[tokio::test]
async fn test_tier1_admin_usage_by_model() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness.admin_get("/admin/api/usage/by-model").await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp.is_array());
}

#[tokio::test]
async fn test_tier1_admin_usage_by_provider() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness.admin_get("/admin/api/usage/by-provider").await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp.is_array());
}

#[tokio::test]
async fn test_tier1_admin_usage_recent_activity() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness.admin_get("/admin/api/usage/recent").await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp.is_array());
}

#[tokio::test]
async fn test_tier1_admin_usage_latency() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness.admin_get("/admin/api/usage/latency").await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp.get("p50").is_some() || resp.is_object());
}

#[tokio::test]
async fn test_tier1_admin_keys_list() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness.admin_get("/admin/api/keys").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!resp.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn test_tier1_admin_keys_create() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness
        .admin_post(
            "/admin/api/keys",
            json!({"label": "tier1-new-key", "scopes": ["chat"]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp.get("plaintext").is_some());
    assert_eq!(resp["key"]["label"], "tier1-new-key");
}

#[tokio::test]
async fn test_tier1_admin_keys_get_by_id() {
    let harness = TestHarness::new().await;
    let id = harness.client_key_id.0;
    let (status, resp) = harness.admin_get(&format!("/admin/api/keys/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["id"], id);
}

#[tokio::test]
async fn test_tier1_admin_keys_update_label() {
    let harness = TestHarness::new().await;
    let id = harness.client_key_id.0;
    let (status, resp) = harness
        .admin_patch(
            &format!("/admin/api/keys/{id}"),
            json!({"label": "renamed-client-key"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["id"], id);
    let (_, key_info) = harness.admin_get(&format!("/admin/api/keys/{id}")).await;
    assert_eq!(key_info["label"], "renamed-client-key");
}

#[tokio::test]
async fn test_tier1_admin_keys_revoke() {
    let harness = TestHarness::new().await;
    let (_, new_key) = harness
        .admin_post(
            "/admin/api/keys",
            json!({"label": "revokable-key", "scopes": ["chat"]}),
        )
        .await;
    let id = new_key["key"]["id"].as_i64().unwrap();
    let (status, resp) = harness
        .admin_post(&format!("/admin/api/keys/{id}/revoke"), json!({}))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["revoked"], true);
}

#[tokio::test]
async fn test_tier1_streaming_sse_frame_format() {
    let harness = TestHarness::new().await;
    let payload = json!({"model": "mock-gpt-4", "messages": [{"role": "user", "content": "Stream frame test"}], "stream": true});
    let (status, _, body) = harness.client_chat_raw(payload).await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("data: "));
}

#[tokio::test]
async fn test_tier1_streaming_token_chunks() {
    let harness = TestHarness::new().await;
    let payload = json!({"model": "mock-gpt-4", "messages": [{"role": "user", "content": "Tokens stream test"}], "stream": true});
    let (status, _, body) = harness.client_chat_raw(payload).await;
    assert_eq!(status, StatusCode::OK);
    let body_str = String::from_utf8_lossy(&body);
    assert!(body_str.contains("Hello") && body_str.contains("from mock!"));
}

#[tokio::test]
async fn test_tier1_streaming_done_terminator() {
    let harness = TestHarness::new().await;
    let payload = json!({"model": "mock-gpt-4", "messages": [{"role": "user", "content": "Done terminator test"}], "stream": true});
    let (status, _, body) = harness.client_chat_raw(payload).await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("data: [DONE]"));
}

#[tokio::test]
async fn test_tier1_streaming_content_type() {
    let harness = TestHarness::new().await;
    let payload = json!({"model": "mock-gpt-4", "messages": [{"role": "user", "content": "Content type test"}], "stream": true});
    let (status, headers, _) = harness.client_chat_raw(payload).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("text/event-stream")
    );
}

#[tokio::test]
async fn test_tier1_streaming_multi_chunk_assembly() {
    let harness = TestHarness::new().await;
    let payload = json!({"model": "mock-gpt-4", "messages": [{"role": "user", "content": "Multi chunk assemble"}], "stream": true});
    let (status, _, body) = harness.client_chat_raw(payload).await;
    assert_eq!(status, StatusCode::OK);
    let mut assembled = String::new();
    for line in String::from_utf8_lossy(&body).lines() {
        if let Some(data) = line.strip_prefix("data: ") {
            if data.trim() == "[DONE]" {
                break;
            }
            if let Ok(chunk) = serde_json::from_str::<Value>(data)
                && let Some(content) = chunk["choices"][0]["delta"]["content"].as_str()
            {
                assembled.push_str(content);
            }
        }
    }
    assert_eq!(assembled, "Hello from mock!");
}

#[test]
fn test_tier1_compression_lite_repeated_content() {
    let text = "You are an AI assistant.\n\n\n\n\n\nAlways provide helpful answers.";
    let mut msgs = vec![
        msg("system", text),
        msg("system", text),
        msg(
            "user",
            "Please help me with this problem.\n\n\n\n\nThank you!",
        ),
    ];
    let stats = apply_compression(&mut msgs, CompressionMode::Lite);
    assert!(!stats.techniques.is_empty() || stats.compressed_chars < stats.original_chars);
}

#[test]
fn test_tier1_compression_rtk_git_status() {
    let git_status = "On branch main\nYour branch is up to date with 'origin/main'.\n\nChanges not staged for commit:\n  (use \"git add <file>...\" to update what will be committed)\n  (use \"git restore <file>...\" to discard changes in working directory)\n\tmodified:   src/lib.rs\n\tmodified:   Cargo.toml\n\nno changes added to commit (use \"git add\" to commit)\n".repeat(5);
    let mut msgs = vec![msg("user", &git_status)];
    assert!(would_compress(&msgs, CompressionMode::Rtk));
    let stats = apply_compression(&mut msgs, CompressionMode::Rtk);
    assert!(!stats.techniques.is_empty() || stats.compressed_chars < stats.original_chars);
}

#[test]
fn test_tier1_compression_rtk_diff_output() {
    let diff_text = "diff --git a/src/main.rs b/src/main.rs\nindex 83a0022..b320d41 100644\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -10,6 +10,8 @@ fn main() {\n-    println!(\"old text\");\n+    println!(\"new text\");\n".repeat(10);
    let mut msgs = vec![msg("user", &diff_text)];
    assert!(would_compress(&msgs, CompressionMode::Rtk));
    let stats = apply_compression(&mut msgs, CompressionMode::Rtk);
    assert!(!stats.techniques.is_empty() || stats.compressed_chars < stats.original_chars);
}

#[test]
fn test_tier1_compression_litertk_combined() {
    let mixed_payload = format!(
        "{}\n{}",
        "repeated whitespace     and spaces     \n".repeat(20),
        "diff --git a/test b/test\n--- a/test\n+++ b/test\n@@ -1 +1 @@\n-old\n+new\n".repeat(10)
    );
    let mut msgs = vec![msg("user", &mixed_payload)];
    assert!(would_compress(&msgs, CompressionMode::LiteRtk));
    let stats = apply_compression(&mut msgs, CompressionMode::LiteRtk);
    assert!(!stats.techniques.is_empty() || stats.compressed_chars < stats.original_chars);
}

#[test]
fn test_tier1_compression_threshold_skip() {
    let msgs = vec![msg("user", "Hello short text")];
    assert!(
        !would_compress(&msgs, CompressionMode::Lite)
            && !would_compress(&msgs, CompressionMode::Rtk)
    );
}
