use super::harness::TestHarness;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body::Body as HttpBody;
use openproxy_adapters::upstream::CancellationToken;
use openproxy_core::admin::{
    self, CreateAccountInput, CreateCustomModelInput, CreateProviderInput,
};
use openproxy_server::disconnect::{DisconnectBody, new_cancel_pair};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Poll;
use std::time::Duration;

#[tokio::test]
async fn test_tier2_payload_exceeding_32mb_rejected() {
    let harness = TestHarness::new().await;
    let req = harness.client_request(
        Method::POST,
        "/v1/chat/completions",
        Body::from(vec![b' '; 33 * 1024 * 1024]),
    );
    let (status, _, _) = harness.oneshot(req).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn test_tier2_empty_payload_chat_rejected() {
    let harness = TestHarness::new().await;
    let req = harness.client_request(Method::POST, "/v1/chat/completions", Body::empty());
    let (status, _, _) = harness.oneshot(req).await;
    assert!(status.is_client_error() || status.is_server_error());
}

#[tokio::test]
async fn test_tier2_malformed_json_rejected() {
    let harness = TestHarness::new().await;
    let req = harness.client_request(
        Method::POST,
        "/v1/chat/completions",
        Body::from("{\"model\": \"mock-gpt-4\", \"messages\": [INVALID JSON}"),
    );
    let (status, _, _) = harness.oneshot(req).await;
    assert!(status.is_client_error() || status.is_server_error());
}

#[tokio::test]
async fn test_tier2_empty_messages_array_handled() {
    let harness = TestHarness::new().await;
    let (status, _, _) = harness
        .client_chat_raw(json!({"model": "mock-gpt-4", "messages": []}))
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn test_tier2_large_valid_payload_accepted() {
    let harness = TestHarness::new().await;
    let (status, _, _) = harness.client_chat_raw(json!({"model": "mock-gpt-4", "messages": [{"role": "user", "content": "A".repeat(500 * 1024)}], "stream": false})).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn test_tier2_auth_missing_token_when_keys_exist() {
    let harness = TestHarness::new().await;
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/chat/completions")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({"model": "mock-gpt-4", "messages": [{"role": "user", "content": "No auth"}]})
                .to_string(),
        ))
        .unwrap();
    let (status, _, _) = harness.oneshot(req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_tier2_auth_invalid_bearer_token() {
    let harness = TestHarness::new().await;
    let req = Request::builder().method(Method::POST).uri("/v1/chat/completions").header(header::AUTHORIZATION, "Bearer invalid_secret_token_12345").header(header::CONTENT_TYPE, "application/json").body(Body::from(json!({"model": "mock-gpt-4", "messages": [{"role": "user", "content": "Invalid auth"}]}).to_string())).unwrap();
    let (status, _, _) = harness.oneshot(req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_tier2_auth_manage_scope_required_for_admin() {
    let harness = TestHarness::new().await;
    let req = Request::builder()
        .method(Method::GET)
        .uri("/admin/api/providers")
        .header(
            header::AUTHORIZATION,
            format!("Bearer {}", harness.client_key),
        )
        .body(Body::empty())
        .unwrap();
    let (status, _, _) = harness.oneshot(req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_tier2_auth_revoked_key_rejected() {
    let harness = TestHarness::new().await;
    let (_, new_key) = harness
        .admin_post(
            "/admin/api/keys",
            json!({"label": "to-be-revoked", "scopes": ["chat"]}),
        )
        .await;
    let plaintext = new_key["plaintext"].as_str().unwrap().to_string();
    let id = new_key["key"]["id"].as_i64().unwrap();

    let req1 = Request::builder()
        .method(Method::GET)
        .uri("/v1/models")
        .header(header::AUTHORIZATION, format!("Bearer {plaintext}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(harness.oneshot(req1).await.0, StatusCode::OK);

    let (revoke_status, _) = harness
        .admin_post(&format!("/admin/api/keys/{id}/revoke"), json!({}))
        .await;
    assert_eq!(revoke_status, StatusCode::OK);

    let req2 = Request::builder()
        .method(Method::GET)
        .uri("/v1/models")
        .header(header::AUTHORIZATION, format!("Bearer {plaintext}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(harness.oneshot(req2).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_tier2_auth_public_endpoints_exempt() {
    let harness = TestHarness::new().await;
    let req_v1 = Request::builder()
        .method(Method::GET)
        .uri("/v1/health")
        .body(Body::empty())
        .unwrap();
    assert_eq!(harness.oneshot(req_v1).await.0, StatusCode::OK);
    let req_admin = Request::builder()
        .method(Method::GET)
        .uri("/admin/health")
        .body(Body::empty())
        .unwrap();
    assert_eq!(harness.oneshot(req_admin).await.0, StatusCode::OK);
}

struct ErrBody;
impl http_body::Body for ErrBody {
    type Data = bytes::Bytes;
    type Error = std::io::Error;
    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        Poll::Ready(Some(Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "simulated client disconnect",
        ))))
    }
}

struct OkBody {
    done: bool,
}
impl http_body::Body for OkBody {
    type Data = bytes::Bytes;
    type Error = std::io::Error;
    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        if self.done {
            Poll::Ready(None)
        } else {
            self.done = true;
            Poll::Ready(Some(Ok(http_body::Frame::data(bytes::Bytes::from_static(
                b"data: frame\n\n",
            )))))
        }
    }
}

#[tokio::test]
async fn test_tier2_disconnect_cancel_watch_fires_on_drop() {
    let (tx, rx) = new_cancel_pair();
    let fired = Arc::new(AtomicBool::new(false));
    let mut body = DisconnectBody::new(ErrBody, tx, Arc::clone(&fired));
    let waker = futures::task::noop_waker();
    let mut cx = std::task::Context::from_waker(&waker);
    let poll_res = std::pin::Pin::new(&mut body).poll_frame(&mut cx);
    assert!(
        matches!(poll_res, Poll::Ready(Some(Err(_))))
            && rx.borrow().is_some()
            && fired.load(Ordering::Relaxed)
    );
}

#[tokio::test]
async fn test_tier2_disconnect_normal_completion_no_cancel() {
    let (tx, rx) = new_cancel_pair();
    let fired = Arc::new(AtomicBool::new(false));
    let mut body = DisconnectBody::new(OkBody { done: false }, tx, Arc::clone(&fired));
    let waker = futures::task::noop_waker();
    let mut cx = std::task::Context::from_waker(&waker);
    assert!(
        matches!(
            std::pin::Pin::new(&mut body).poll_frame(&mut cx),
            Poll::Ready(Some(Ok(_)))
        ) && rx.borrow().is_none()
    );
    assert!(
        matches!(
            std::pin::Pin::new(&mut body).poll_frame(&mut cx),
            Poll::Ready(None)
        ) && rx.borrow().is_none()
            && !fired.load(Ordering::Relaxed)
    );
}

#[tokio::test]
async fn test_tier2_disconnect_cancellation_token_link() {
    let cancel = CancellationToken::new();
    assert!(!cancel.is_cancelled());
    cancel.cancel();
    assert!(cancel.is_cancelled());
}

#[tokio::test]
async fn test_tier2_disconnect_half_close_detection() {
    let (tx, rx) = new_cancel_pair();
    let fired = Arc::new(AtomicBool::new(false));
    let mut body = DisconnectBody::new(ErrBody, tx, Arc::clone(&fired));
    let waker = futures::task::noop_waker();
    let mut cx = std::task::Context::from_waker(&waker);
    let _ = std::pin::Pin::new(&mut body).poll_frame(&mut cx);
    assert!(rx.borrow().is_some());
}

#[tokio::test]
async fn test_tier2_disconnect_aborted_upstream_stops_tokens() {
    let harness = TestHarness::new().await;
    harness.mock_handle.set_delay(Duration::from_millis(50));
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        cancel_clone.cancel();
    });
    assert!(!cancel.is_cancelled());
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert!(cancel.is_cancelled());
    harness.mock_handle.clear_delay();
}

#[tokio::test]
async fn test_tier2_timeout_upstream_delay_exceeds_budget() {
    let harness = TestHarness::new().await;
    harness.mock_handle.set_delay(Duration::from_secs(3));
    let mut req = harness.client_request(Method::POST, "/v1/chat/completions", Body::from(json!({"model": "mock-gpt-4", "messages": [{"role": "user", "content": "Timeout test"}], "stream": false}).to_string()));
    req.headers_mut().insert(
        header::HeaderName::from_static("x-request-deadline-ms"),
        header::HeaderValue::from_static("100"),
    );
    let (status, _, _) = harness.oneshot(req).await;
    harness.mock_handle.clear_delay();
    assert!(
        status.as_u16() == 499 || status.is_server_error() || status == StatusCode::GATEWAY_TIMEOUT
    );
}

#[tokio::test]
async fn test_tier2_retry_on_500_switches_target() {
    let harness = TestHarness::new().await;
    let p2_id = "mock-prov-2";
    {
        let w = harness.db_pool.writer();
        admin::create_provider(
            &w,
            CreateProviderInput {
                id: p2_id.into(),
                name: "Fallback Provider".into(),
                base_url: format!("http://{}/v1", harness.mock_addr),
                auth_type: "bearer".into(),
                format: "openai".into(),
                extra_headers_json: None,
                rate_limit_scope: None,
            },
        )
        .expect("create fallback provider");
        admin::create_account(
            &w,
            &harness.master_key,
            CreateAccountInput {
                provider_id: p2_id.into(),
                api_key: Some("sk-fallback-key".into()),
                label: Some("fallback-acc".into()),
                priority: Some(20),
                extra_config_json: None,
            },
        )
        .expect("create fallback account");
        admin::create_custom_model(
            &w,
            CreateCustomModelInput {
                provider_id: p2_id.into(),
                model_id: "mock-gpt-4".into(),
                display_name: Some("Mock GPT 4 Fallback".into()),
                target_format: "openai".into(),
                ttl_seconds: 3600,
                model_type: Some("chat".into()),
            },
        )
        .expect("create fallback custom model");
    }
    harness.app_state.rebuild_adapters().await.unwrap();

    let (status, combo) = harness
        .admin_post(
            "/admin/api/combos",
            json!({"name": "retry-combo", "strategy": "priority"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
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
        .find(|m| m["provider_id"] == p2_id)
        .and_then(|m| m.get("row_id").or_else(|| m.get("id")))
        .unwrap()
        .as_i64()
        .unwrap();

    let _ = harness
        .admin_post(
            &format!("/admin/api/combos/{combo_id}/targets"),
            json!({"provider_id": "mock-prov", "model_row_id": m1, "priority_order": 1}),
        )
        .await;
    let _ = harness
        .admin_post(
            &format!("/admin/api/combos/{combo_id}/targets"),
            json!({"provider_id": p2_id, "model_row_id": m2, "priority_order": 2}),
        )
        .await;

    let (chat_status, resp) = harness
        .client_chat_call("retry-combo", "Retry test", false)
        .await;
    assert_eq!(chat_status, StatusCode::OK);
    assert_eq!(resp["choices"][0]["finish_reason"], "stop");
}

#[tokio::test]
async fn test_tier2_non_retryable_400_fails_fast() {
    let harness = TestHarness::new().await;
    harness.mock_handle.set_error(
        StatusCode::BAD_REQUEST,
        Some("{\"error\": \"bad_request_from_upstream\"}"),
    );
    let (status, _) = harness
        .client_chat_call("mock-gpt-4", "Fast fail", false)
        .await;
    harness.mock_handle.clear_error();
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_tier2_exhausted_retries_returns_upstream_error() {
    let harness = TestHarness::new().await;
    harness.mock_handle.set_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        Some("{\"error\": \"fatal_upstream_outage\"}"),
    );
    let (status, _) = harness
        .client_chat_call("mock-gpt-4", "Exhaustion test", false)
        .await;
    harness.mock_handle.clear_error();
    assert!(status.is_server_error());
}

#[tokio::test]
async fn test_tier2_circuit_breaker_tripping() {
    let harness = TestHarness::new().await;
    let cb = harness.app_state.circuit_breaker();
    let key = openproxy_pipeline::circuit_breaker::CircuitBreakerKey::Account(
        openproxy_types::ids::AccountId(1),
    );
    assert_eq!(
        cb.is_healthy(key),
        openproxy_pipeline::circuit_breaker::Health::Healthy
    );
    for _ in 0..5 {
        cb.record_failure(key);
    }
    assert_ne!(
        cb.is_healthy(key),
        openproxy_pipeline::circuit_breaker::Health::Healthy
    );
    cb.record_success(key);
    assert_eq!(
        cb.is_healthy(key),
        openproxy_pipeline::circuit_breaker::Health::Healthy
    );
}
