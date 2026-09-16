use super::harness::TestHarness;
use axum::http::StatusCode;
use openproxy_core::admin::{
    self, CreateAccountInput, CreateCustomModelInput, CreateProviderInput,
};
use openproxy_core::rate_limit::{
    RateLimitConfig, RateLimitKey, RateLimiter, SlidingWindowRateLimiter,
};
use openproxy_types::ids::ComboTargetId;
use serde_json::json;

#[tokio::test]
async fn test_tier3_combo_resolution_failover_cooldown() {
    let harness = TestHarness::new().await;
    let p2 = "mock-prov-failover-2";
    {
        let w = harness.db_pool.writer();
        admin::create_provider(
            &w,
            CreateProviderInput {
                id: p2.into(),
                name: "Fallback Provider 2".into(),
                base_url: format!("http://{}/v1", harness.mock_addr),
                auth_type: "bearer".into(),
                format: "openai".into(),
                extra_headers_json: None,
                rate_limit_scope: None,
            },
        )
        .unwrap();
        admin::create_account(
            &w,
            &harness.master_key,
            CreateAccountInput {
                provider_id: p2.into(),
                api_key: Some("sk-p2".into()),
                label: Some("p2-acc".into()),
                priority: Some(10),
                extra_config_json: None,
            },
        )
        .unwrap();
        admin::create_custom_model(
            &w,
            CreateCustomModelInput {
                provider_id: p2.into(),
                model_id: "mock-gpt-4".into(),
                display_name: Some("GPT 4 on P2".into()),
                target_format: "openai".into(),
                ttl_seconds: 3600,
                model_type: Some("chat".into()),
            },
        )
        .unwrap();
    }
    harness.app_state.rebuild_adapters().await.unwrap();

    let (_, combo) = harness.admin_post("/admin/api/combos", json!({"name": "tier3-failover-combo", "strategy": "priority", "cooldown_mode": "flat", "cooldown_base_secs": 60})).await;
    let combo_id = combo["id"].as_i64().unwrap();
    let (_, models) = harness.admin_get("/admin/api/models").await;
    let m1 = models
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["provider_id"] == "mock-prov")
        .and_then(|m| m.get("row_id").or_else(|| m.get("id")))
        .unwrap()
        .as_i64()
        .unwrap();
    let m2 = models
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["provider_id"] == p2)
        .and_then(|m| m.get("row_id").or_else(|| m.get("id")))
        .unwrap()
        .as_i64()
        .unwrap();

    let (_, t1) = harness
        .admin_post(
            &format!("/admin/api/combos/{combo_id}/targets"),
            json!({"provider_id": "mock-prov", "model_row_id": m1, "priority_order": 1}),
        )
        .await;
    let t1_id = t1["id"].as_i64().unwrap();
    let _ = harness
        .admin_post(
            &format!("/admin/api/combos/{combo_id}/targets"),
            json!({"provider_id": p2, "model_row_id": m2, "priority_order": 2}),
        )
        .await;

    {
        let w = harness.db_pool.writer();
        let _ = openproxy_db::cooldowns::record_cooldown(
            &w,
            ComboTargetId(t1_id),
            "simulated 500 error from upstream",
            openproxy_types::CooldownMode::Flat,
            60,
            3600,
            2,
        );
    }

    let (status, resp) = harness
        .client_chat_call("tier3-failover-combo", "Failover test", false)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["choices"][0]["finish_reason"], "stop");
}

#[tokio::test]
async fn test_tier3_sse_streaming_with_compression() {
    let harness = TestHarness::new().await;
    harness
        .app_state
        .set_compression_mode(openproxy_compression::CompressionMode::Lite);
    let repeated_prompt = format!(
        "Please explain the following data:\n{}",
        "key: value, repeated entry pattern\n".repeat(30)
    );
    let (status, headers, body) = harness.client_chat_raw(json!({"model": "mock-gpt-4", "messages": [{"role": "user", "content": repeated_prompt}], "stream": true})).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers
            .get(axum::http::header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("text/event-stream")
    );
    let body_str = String::from_utf8_lossy(&body);
    assert!(body_str.contains("data: ") && body_str.contains("[DONE]"));
    harness
        .app_state
        .set_compression_mode(openproxy_compression::CompressionMode::Off);
}

#[tokio::test]
async fn test_tier3_bearer_auth_rate_limiting_usage() {
    let harness = TestHarness::new().await;
    let (status, _) = harness
        .client_chat_call("mock-gpt-4", "Tracking test", false)
        .await;
    assert_eq!(status, StatusCode::OK);

    let rate_limiter = SlidingWindowRateLimiter::new(RateLimitConfig {
        max_requests: 2,
        window: std::time::Duration::from_secs(60),
        max_capacity: 1000,
    });
    let key = RateLimitKey::Key(harness.client_key_id);
    assert!(rate_limiter.check(key) && rate_limiter.check(key) && !rate_limiter.check(key));
}

#[tokio::test]
async fn test_tier3_combo_round_robin_distribution() {
    let harness = TestHarness::new().await;
    let (_, combo) = harness
        .admin_post(
            "/admin/api/combos",
            json!({"name": "rr-combo-tier3", "strategy": "round_robin"}),
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

    for i in 1..=3 {
        let (status, resp) = harness
            .client_chat_call("rr-combo-tier3", &format!("RR message {i}"), false)
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(resp["object"], "chat.completion");
    }
    assert!(harness.mock_handle.recorded_requests().len() >= 3);
}

#[tokio::test]
async fn test_tier3_key_model_restrictions_with_combo() {
    let harness = TestHarness::new().await;
    let (_, combo) = harness
        .admin_post(
            "/admin/api/combos",
            json!({"name": "restricted-combo", "strategy": "priority"}),
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

    let (_, key_res) = harness
        .admin_post(
            "/admin/api/keys",
            json!({"label": "disallowed-combo-key", "scopes": ["chat"], "allowed_combos": [99999]}),
        )
        .await;
    let plaintext = key_res["plaintext"].as_str().unwrap();
    let req = axum::http::Request::builder().method(axum::http::Method::POST).uri("/v1/chat/completions").header(axum::http::header::AUTHORIZATION, format!("Bearer {plaintext}")).header(axum::http::header::CONTENT_TYPE, "application/json").body(axum::body::Body::from(json!({"model": "restricted-combo", "messages": [{"role": "user", "content": "Should be blocked"}]}).to_string())).unwrap();
    assert_eq!(harness.oneshot(req).await.0, StatusCode::UNAUTHORIZED);
}
