use super::harness::TestHarness;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use openproxy_core::admin;
use serde_json::json;
use std::time::Duration;

#[tokio::test]
async fn test_tier4_scenario_multiturn_conversation() {
    let harness = TestHarness::new().await;
    let turn1 = json!({"model": "mock-gpt-4", "messages": [{"role": "user", "content": "My project is named OpenProxy Gateway."}], "stream": false});
    let (status1, resp1) = harness
        .oneshot_json(harness.client_request(
            Method::POST,
            "/v1/chat/completions",
            Body::from(turn1.to_string()),
        ))
        .await;
    assert_eq!(status1, StatusCode::OK);
    let msg1 = resp1["choices"][0]["message"]["content"]
        .as_str()
        .unwrap()
        .to_string();

    let turn2 = json!({"model": "mock-gpt-4", "messages": [{"role": "user", "content": "My project is named OpenProxy Gateway."}, {"role": "assistant", "content": msg1}, {"role": "user", "content": "What did I say my project was named?"}], "stream": false});
    let (status2, resp2) = harness
        .oneshot_json(harness.client_request(
            Method::POST,
            "/v1/chat/completions",
            Body::from(turn2.to_string()),
        ))
        .await;
    assert_eq!(status2, StatusCode::OK);
    assert_eq!(resp2["choices"][0]["finish_reason"], "stop");

    let recorded = harness.mock_handle.recorded_requests();
    let sent = recorded.last().unwrap().body["messages"]
        .as_array()
        .unwrap();
    assert_eq!(sent.len(), 3);
    assert!(
        sent[0]["role"] == "user" && sent[1]["role"] == "assistant" && sent[2]["role"] == "user"
    );
}

#[tokio::test]
async fn test_tier4_scenario_dynamic_model_discovery_sync() {
    let harness = TestHarness::new().await;
    harness.mock_handle.replace_catalog(vec![
        "discovery-model-alpha".into(),
        "discovery-model-beta".into(),
    ]);
    let (s1, r1) = harness
        .admin_post("/admin/api/providers/mock-prov/refresh", json!({}))
        .await;
    assert_eq!(s1, StatusCode::OK);
    assert!(r1.get("models_refreshed").is_some());

    let (sm, cat) = harness.admin_get("/admin/api/models").await;
    assert_eq!(sm, StatusCode::OK);
    let arr = cat.as_array().unwrap();
    assert!(
        arr.iter().any(|m| m["model_id"] == "discovery-model-alpha")
            && arr.iter().any(|m| m["model_id"] == "discovery-model-beta")
    );

    harness.mock_handle.replace_catalog(vec![
        "discovery-model-beta".into(),
        "discovery-model-gamma".into(),
    ]);
    assert_eq!(
        harness
            .admin_post("/admin/api/providers/mock-prov/refresh", json!({}))
            .await
            .0,
        StatusCode::OK
    );
    let (_, cat2) = harness.admin_get("/admin/api/models").await;
    assert!(
        cat2.as_array()
            .unwrap()
            .iter()
            .any(|m| m["model_id"] == "discovery-model-gamma")
    );
}

#[tokio::test]
async fn test_tier4_scenario_multi_client_analytics_accumulation() {
    let harness = TestHarness::new().await;
    let (_, key_a) = harness
        .admin_post(
            "/admin/api/keys",
            json!({"label": "Client Alpha", "scopes": ["chat"]}),
        )
        .await;
    let token_a = key_a["plaintext"].as_str().unwrap();
    let (_, key_b) = harness
        .admin_post(
            "/admin/api/keys",
            json!({"label": "Client Beta", "scopes": ["chat"]}),
        )
        .await;
    let token_b = key_b["plaintext"].as_str().unwrap();

    for i in 1..=2 {
        let req = Request::builder().method(Method::POST).uri("/v1/chat/completions").header(header::AUTHORIZATION, format!("Bearer {token_a}")).header(header::CONTENT_TYPE, "application/json").body(Body::from(json!({"model": "mock-gpt-4", "messages": [{"role": "user", "content": format!("Alpha msg {i}")}]}).to_string())).unwrap();
        assert_eq!(harness.oneshot(req).await.0, StatusCode::OK);
    }
    let req_b = Request::builder()
        .method(Method::POST)
        .uri("/v1/chat/completions")
        .header(header::AUTHORIZATION, format!("Bearer {token_b}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({"model": "mock-gpt-4", "messages": [{"role": "user", "content": "Beta msg 1"}]})
                .to_string(),
        ))
        .unwrap();
    assert_eq!(harness.oneshot(req_b).await.0, StatusCode::OK);

    let (sum_st, summary) = harness.admin_get("/admin/api/usage/summary").await;
    assert_eq!(sum_st, StatusCode::OK);
    assert!(summary.is_object());
    let (m_st, by_model) = harness.admin_get("/admin/api/usage/by-model").await;
    assert_eq!(m_st, StatusCode::OK);
    assert!(by_model.is_array());
}

#[tokio::test]
async fn test_tier4_scenario_concurrent_race_winner_selection() {
    let harness = TestHarness::new().await;
    let (_, combo) = harness
        .admin_post(
            "/admin/api/combos",
            json!({"name": "race-scenario-combo", "strategy": "priority", "race_size": 2}),
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
    let _ = harness
        .admin_post(
            &format!("/admin/api/combos/{combo_id}/targets"),
            json!({"provider_id": "mock-prov", "model_row_id": m1, "priority_order": 1}),
        )
        .await;

    let start = std::time::Instant::now();
    let (status, resp) = harness
        .client_chat_call("race-scenario-combo", "Race test message", false)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["object"], "chat.completion");
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn test_tier4_scenario_full_admin_lifecycle() {
    let harness = TestHarness::new().await;
    let prov_id = "lifecycle-provider";
    let (st1, p_res) = harness.admin_post("/admin/api/providers", json!({"id": prov_id, "name": "Lifecycle Provider", "base_url": format!("http://{}/v1", harness.mock_addr), "auth_type": "bearer", "format": "openai"})).await;
    assert_eq!(st1, StatusCode::OK);
    assert_eq!(p_res["id"], prov_id);

    {
        let w = harness.db_pool.writer();
        admin::create_account(
            &w,
            &harness.master_key,
            admin::CreateAccountInput {
                provider_id: prov_id.into(),
                api_key: Some("sk-lifecycle-acc".into()),
                label: Some("lifecycle-account".into()),
                priority: Some(10),
                extra_config_json: None,
            },
        )
        .unwrap();
    }
    harness.app_state.rebuild_adapters().await.unwrap();

    let (st2, m_res) = harness.admin_post("/admin/api/models/custom", json!({"provider_id": prov_id, "model_id": "lifecycle-gpt", "display_name": "Lifecycle GPT", "target_format": "openai", "ttl_seconds": 3600})).await;
    assert_eq!(st2, StatusCode::OK);
    let model_row_id = m_res
        .get("row_id")
        .or_else(|| m_res.get("id"))
        .unwrap()
        .as_i64()
        .unwrap();

    let (st3, c_res) = harness
        .admin_post(
            "/admin/api/combos",
            json!({"name": "lifecycle-combo", "strategy": "priority"}),
        )
        .await;
    assert_eq!(st3, StatusCode::OK);
    let combo_id = c_res["id"].as_i64().unwrap();
    assert_eq!(
        harness
            .admin_post(
                &format!("/admin/api/combos/{combo_id}/targets"),
                json!({"provider_id": prov_id, "model_row_id": model_row_id, "priority_order": 1})
            )
            .await
            .0,
        StatusCode::OK
    );

    let (st4, k_res) = harness.admin_post("/admin/api/keys", json!({"label": "Customer Team Key", "scopes": ["chat"], "allowed_models": ["lifecycle-combo", "lifecycle-gpt"]})).await;
    assert_eq!(st4, StatusCode::OK);
    let customer_key = k_res["plaintext"].as_str().unwrap();
    let customer_key_id = k_res["key"]["id"].as_i64().unwrap();

    let cust_req = Request::builder().method(Method::POST).uri("/v1/chat/completions").header(header::AUTHORIZATION, format!("Bearer {customer_key}")).header(header::CONTENT_TYPE, "application/json").body(Body::from(json!({"model": "lifecycle-combo", "messages": [{"role": "user", "content": "Lifecycle test prompt"}]}).to_string())).unwrap();
    let (cust_st, cust_resp) = harness.oneshot_json(cust_req).await;
    assert_eq!(cust_st, StatusCode::OK);
    assert_eq!(cust_resp["choices"][0]["finish_reason"], "stop");

    let (u_st, u_resp) = harness.admin_get("/admin/api/usage/recent").await;
    assert_eq!(u_st, StatusCode::OK);
    assert!(u_resp.is_array());

    assert_eq!(
        harness
            .admin_post(
                &format!("/admin/api/keys/{customer_key_id}/revoke"),
                json!({})
            )
            .await
            .0,
        StatusCode::OK
    );

    let cust_req_blocked = Request::builder().method(Method::POST).uri("/v1/chat/completions").header(header::AUTHORIZATION, format!("Bearer {customer_key}")).header(header::CONTENT_TYPE, "application/json").body(Body::from(json!({"model": "lifecycle-combo", "messages": [{"role": "user", "content": "Attempt after revocation"}]}).to_string())).unwrap();
    assert_eq!(
        harness.oneshot(cust_req_blocked).await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn test_tier4_scenario_responses_api_streaming_and_non_streaming() {
    let harness = TestHarness::new().await;

    // 1. Streaming Responses request
    let stream_payload = json!({
        "model": "mock-gpt-4",
        "input": [
            {"role": "user", "content": "Hello via Responses API stream"}
        ],
        "stream": true
    });
    let stream_req = harness.client_request(
        Method::POST,
        "/v1/responses",
        Body::from(stream_payload.to_string()),
    );
    let (status, headers, body_bytes) = harness.oneshot(stream_req).await;
    assert_eq!(status, StatusCode::OK);
    let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    assert!(ct.contains("text/event-stream"));

    let body_str = String::from_utf8_lossy(&body_bytes);
    assert!(body_str.contains("event: response.created"), "missing response.created in:\n{body_str}");
    assert!(body_str.contains("event: response.output_item.added"), "missing output_item.added in:\n{body_str}");
    assert!(body_str.contains("event: response.output_text.delta"), "missing output_text.delta in:\n{body_str}");
    assert!(body_str.contains("event: response.output_item.done"), "missing output_item.done in:\n{body_str}");
    assert!(body_str.contains("event: response.completed"), "missing response.completed in:\n{body_str}");
    assert!(body_str.contains("\"status\":\"completed\""), "missing status completed in:\n{body_str}");
    assert!(body_str.contains("data: [DONE]"), "missing [DONE] in:\n{body_str}");

    // 2. Non-streaming Responses request
    let sync_payload = json!({
        "model": "mock-gpt-4",
        "input": [
            {"role": "user", "content": "Hello via Responses API sync"}
        ],
        "stream": false
    });
    let sync_req = harness.client_request(
        Method::POST,
        "/v1/responses",
        Body::from(sync_payload.to_string()),
    );
    let (sync_status, sync_val) = harness.oneshot_json(sync_req).await;
    assert_eq!(sync_status, StatusCode::OK);
    assert_eq!(sync_val["object"], "response");
    assert_eq!(sync_val["status"], "completed");
    assert!(sync_val["output"].is_array());
    assert_eq!(sync_val["output"][0]["type"], "message");
    assert_eq!(sync_val["output"][0]["content"][0]["type"], "output_text");
    assert!(sync_val["usage"]["input_tokens"].is_number());
    assert!(sync_val["usage"]["output_tokens"].is_number());
}

