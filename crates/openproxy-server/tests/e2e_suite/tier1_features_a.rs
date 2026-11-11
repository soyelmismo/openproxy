use crate::harness::TestHarness;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use serde_json::{Value, json};

#[test]
fn test_tier1_chat_completions_basic_success() {
    crate::run_with_big_stack(test_tier1_chat_completions_basic_success_body);
}

async fn test_tier1_chat_completions_basic_success_body() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness
        .client_chat_call("mock-gpt-4", "Hello world", false)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["object"], "chat.completion");
    assert_eq!(resp["choices"][0]["message"]["content"], "Hello from mock!");
}

#[test]
fn test_tier1_chat_completions_system_and_user_messages() {
    crate::run_with_big_stack(test_tier1_chat_completions_system_and_user_messages_body);
}

async fn test_tier1_chat_completions_system_and_user_messages_body() {
    let harness = TestHarness::new().await;
    let payload = json!({"model": "mock-gpt-4", "messages": [{"role": "system", "content": "You are a helpful assistant."}, {"role": "user", "content": "Summarize this request."}], "stream": false});
    let (status, _, body) = harness.client_chat_raw(payload).await;
    assert_eq!(status, StatusCode::OK);
    let resp: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(resp["choices"][0]["finish_reason"], "stop");
    assert!(!harness.mock_handle.recorded_requests().is_empty());
}

#[test]
fn test_tier1_chat_completions_custom_model_routing() {
    crate::run_with_big_stack(test_tier1_chat_completions_custom_model_routing_body);
}

async fn test_tier1_chat_completions_custom_model_routing_body() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness
        .client_chat_call("mock-gpt-4", "Test custom model", false)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["model"], "mock-gpt-4");
}

#[test]
fn test_tier1_chat_completions_response_format_json() {
    crate::run_with_big_stack(test_tier1_chat_completions_response_format_json_body);
}

async fn test_tier1_chat_completions_response_format_json_body() {
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

#[test]
fn test_tier1_chat_completions_token_accounting() {
    crate::run_with_big_stack(test_tier1_chat_completions_token_accounting_body);
}

async fn test_tier1_chat_completions_token_accounting_body() {
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

#[test]
fn test_tier1_models_list_openai_envelope() {
    crate::run_with_big_stack(test_tier1_models_list_openai_envelope_body);
}

async fn test_tier1_models_list_openai_envelope_body() {
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

#[test]
fn test_tier1_models_list_anthropic_format() {
    crate::run_with_big_stack(test_tier1_models_list_anthropic_format_body);
}

async fn test_tier1_models_list_anthropic_format_body() {
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

#[test]
fn test_tier1_models_list_includes_combos() {
    crate::run_with_big_stack(test_tier1_models_list_includes_combos_body);
}

async fn test_tier1_models_list_includes_combos_body() {
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

#[test]
fn test_tier1_models_list_key_scoped_filtering() {
    crate::run_with_big_stack(test_tier1_models_list_key_scoped_filtering_body);
}

async fn test_tier1_models_list_key_scoped_filtering_body() {
    let harness = TestHarness::new().await;
    // `allowed_models` is additive: it grants access to the named models on
    // top of the default-permit catalog, never hides the rest. The key must
    // therefore still see the whole catalog, including its whitelisted model.
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
    let ids: Vec<&str> = resp["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    assert!(
        ids.iter().any(|id| id.contains("mock-gpt-4")),
        "whitelisted model must be visible: {ids:?}"
    );
    // Filtering still applies via blacklists: a key that blacklists
    // mock-embed must not see it.
    let (status, bl_key) = harness
        .admin_post(
            "/admin/api/keys",
            json!({"label": "bl-key", "scopes": ["chat"], "blacklisted_models": ["mock-embed"]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let plaintext = bl_key["plaintext"].as_str().unwrap();
    let req = Request::builder()
        .method(Method::GET)
        .uri("/v1/models")
        .header(header::AUTHORIZATION, format!("Bearer {plaintext}"))
        .body(Body::empty())
        .unwrap();
    let (status, resp) = harness.oneshot_json(req).await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = resp["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    assert!(
        !ids.iter().any(|id| id.contains("mock-embed")),
        "blacklisted model must be filtered: {ids:?}"
    );
    assert!(
        ids.iter().any(|id| id.contains("mock-gpt-4")),
        "non-blacklisted model stays visible: {ids:?}"
    );
}

#[test]
fn test_tier1_models_list_metadata_fields() {
    crate::run_with_big_stack(test_tier1_models_list_metadata_fields_body);
}

async fn test_tier1_models_list_metadata_fields_body() {
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

#[test]
fn test_tier1_embeddings_single_input_success() {
    crate::run_with_big_stack(test_tier1_embeddings_single_input_success_body);
}

async fn test_tier1_embeddings_single_input_success_body() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness
        .client_embeddings_call("mock-embed", json!("semantic search string"))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["object"], "list");
    assert_eq!(resp["data"].as_array().unwrap().len(), 1);
}

#[test]
fn test_tier1_embeddings_multiple_inputs() {
    crate::run_with_big_stack(test_tier1_embeddings_multiple_inputs_body);
}

async fn test_tier1_embeddings_multiple_inputs_body() {
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

#[test]
fn test_tier1_embeddings_empty_input_validation() {
    crate::run_with_big_stack(test_tier1_embeddings_empty_input_validation_body);
}

async fn test_tier1_embeddings_empty_input_validation_body() {
    let harness = TestHarness::new().await;
    let (status, _) = harness
        .client_embeddings_call("mock-embed", json!([]))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[test]
fn test_tier1_embeddings_vector_structure() {
    crate::run_with_big_stack(test_tier1_embeddings_vector_structure_body);
}

async fn test_tier1_embeddings_vector_structure_body() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness
        .client_embeddings_call("mock-embed", json!("vector test"))
        .await;
    assert_eq!(status, StatusCode::OK);
    let vec = resp["data"][0]["embedding"].as_array().unwrap();
    assert!(!vec.is_empty() && vec[0].is_number());
}

#[test]
fn test_tier1_embeddings_usage_accounting() {
    crate::run_with_big_stack(test_tier1_embeddings_usage_accounting_body);
}

async fn test_tier1_embeddings_usage_accounting_body() {
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
