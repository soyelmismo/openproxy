use super::common::*;
use crate::handlers::admin::runtime::{get_recording_ttl, put_recording_ttl, put_runtime_timeouts};

#[tokio::test]
async fn put_runtime_timeouts_writes_db_and_updates_slot() {
    let dir = tempdir();
    let (state, plaintext) = make_state_with_key(&dir).await;
    assert_eq!(state.timeouts().connect_ms, 5_000);
    let app = Router::new()
        .route("/admin/config/timeouts", put(put_runtime_timeouts))
        .with_state(state.clone());
    let body = serde_json::json!({"connect_ms": 1_u64, "request_send_ms": 2_u64, "ttft_ms": 3_u64, "idle_chunk_ms": 4_u64, "total_ms": 5_u64});
    let (status, parsed) = test_req(
        &app,
        "PUT",
        "/admin/config/timeouts",
        Some(&plaintext),
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(parsed["connect_ms"], 1);
    assert_eq!(parsed["total_ms"], 5);
    assert_eq!(parsed["applies_to"], "next_requests");
    let after = state.timeouts();
    assert_eq!(
        after,
        TimeoutsConfig {
            connect_ms: 1,
            request_send_ms: 2,
            ttft_ms: 3,
            idle_chunk_ms: 4,
            total_ms: 5
        }
    );
    let count: i64 = state.db_pool().with_conn(|c| {
        c.query_row(
            "SELECT COUNT(*) FROM app_config WHERE key = 'timeouts'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    });
    assert_eq!(count, 1);
}

#[tokio::test]
async fn put_runtime_timeouts_without_auth_returns_401() {
    let dir = tempdir();
    let (state, _plaintext) = make_state_with_key(&dir).await;
    let app = Router::new()
        .route("/admin/config/timeouts", put(put_runtime_timeouts))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::handlers::admin::auth::admin_auth_middleware,
        ))
        .layer(axum::Extension(axum::extract::connect_info::ConnectInfo(
            "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
        )))
        .with_state(state);
    let body = serde_json::json!({"connect_ms": 1_u64, "request_send_ms": 2_u64, "ttft_ms": 3_u64, "idle_chunk_ms": 4_u64, "total_ms": 5_u64});
    let (status, _) = test_req(&app, "PUT", "/admin/config/timeouts", None, Some(body)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn put_runtime_timeouts_malformed_body_returns_400() {
    let dir = tempdir();
    let (state, plaintext) = make_state_with_key(&dir).await;
    let app = Router::new()
        .route("/admin/config/timeouts", put(put_runtime_timeouts))
        .with_state(state);
    let bad = serde_json::json!({"connect_ms": 1, "request_send_ms": 2, "ttft_ms": 3, "idle_chunk_ms": 4});
    let (status, _) = test_req(
        &app,
        "PUT",
        "/admin/config/timeouts",
        Some(&plaintext),
        Some(bad),
    )
    .await;
    assert!(status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn auth_bypass_sentinel_1_admits_admin_request_without_key() {
    let tmp = tempdir();
    let (state, _key) = make_state_with_key(tmp.path()).await;
    {
        let w = state.db_pool().writer();
        w.execute("DELETE FROM api_keys", []).expect("delete keys");
    }
    let headers = HeaderMap::new();
    let lock_guard = AUTH_BYPASS_TEST_LOCK.lock().unwrap();
    let _env_guard = EnvVarGuard::set(&lock_guard, "OPENPROXY_DASHBOARD_AUTH_BYPASS", "1");
    let addr = "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        authenticate_admin_ws(&state, &headers, None, Some(&addr))
    }));
    let result = result.expect("authenticate_admin_ws should not panic");
    assert!(
        result.is_ok(),
        "authenticate_admin_ws should succeed when bypass=1 is set, got {:?}",
        result.err()
    );
}

#[tokio::test]
async fn auth_bypass_does_not_admit_on_non_sentinel_values() {
    let tmp = tempdir();
    let (state, _key) = make_state_with_key(tmp.path()).await;
    {
        let w = state.db_pool().writer();
        w.execute("DELETE FROM api_keys", []).expect("delete keys");
    }
    for sentinel in ["false", "yes", "0", "true", "TRUE", "legacy-token", " "] {
        let headers = HeaderMap::new();
        let lock_guard = AUTH_BYPASS_TEST_LOCK.lock().unwrap();
        let _env_guard = EnvVarGuard::set(&lock_guard, "OPENPROXY_DASHBOARD_AUTH_BYPASS", sentinel);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            authenticate_admin_ws(&state, &headers, None, None)
        }));
        let result = result.expect("authenticate_admin_ws should not panic");
        assert!(
            result.is_err(),
            "OPENPROXY_DASHBOARD_AUTH_BYPASS={sentinel:?} must NOT bypass auth"
        );
    }
}

#[tokio::test]
async fn auth_bypass_sentinel_1_rejects_non_loopback() {
    let tmp = tempdir();
    let (state, _key) = make_state_with_key(tmp.path()).await;
    {
        let w = state.db_pool().writer();
        w.execute("DELETE FROM api_keys", []).expect("delete keys");
    }
    let headers = HeaderMap::new();
    let lock_guard = AUTH_BYPASS_TEST_LOCK.lock().unwrap();
    let _env_guard = EnvVarGuard::set(&lock_guard, "OPENPROXY_DASHBOARD_AUTH_BYPASS", "1");

    let addr = "192.168.1.100:12345"
        .parse::<std::net::SocketAddr>()
        .unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        authenticate_admin_ws(&state, &headers, None, Some(&addr))
    }));
    let result = result.expect("authenticate_admin_ws should not panic");
    assert!(
        result.is_err(),
        "authenticate_admin_ws should reject non-loopback IPs even with bypass"
    );
}

#[tokio::test]
async fn body_limit_accepts_10_mib_chat_body() {
    let tmp = tempdir();
    let (state, key) = make_state_with_key(tmp.path()).await;
    let app = crate::router::build_router(state);
    let big = "x".repeat(10 * 1024 * 1024);
    let body_json =
        format!(r#"{{"model":"gpt-4o","messages":[{{"role":"system","content":"{big}"}}]}}"#);
    let mut req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {key}"))
        .body(Body::from(body_json))
        .expect("build req");
    req.extensions_mut()
        .insert(axum::extract::connect_info::ConnectInfo(
            std::net::SocketAddr::from(([127, 0, 0, 1], 12345)),
        ));
    let resp = app.oneshot(req).await.expect("oneshot");
    assert_ne!(
        resp.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "10 MiB body must be accepted"
    );
}

#[tokio::test]
async fn body_limit_rejects_100_mib_chat_body() {
    let tmp = tempdir();
    let (state, key) = make_state_with_key(tmp.path()).await;
    let app = crate::router::build_router(state);
    let big = "x".repeat(100 * 1024 * 1024);
    let body_json =
        format!(r#"{{"model":"gpt-4o","messages":[{{"role":"system","content":"{big}"}}]}}"#);
    let mut req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {key}"))
        .body(Body::from(body_json))
        .expect("build req");
    req.extensions_mut()
        .insert(axum::extract::connect_info::ConnectInfo(
            std::net::SocketAddr::from(([127, 0, 0, 1], 12345)),
        ));
    let resp = app.oneshot(req).await.expect("oneshot");
    assert_eq!(
        resp.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "100 MiB body must be rejected"
    );
}

#[tokio::test]
async fn get_recording_ttl_returns_default_value() {
    let dir = tempdir();
    let (state, plaintext) = make_state_with_key(&dir).await;
    assert_eq!(state.recording_ttl_secs(), 300);
    let app = Router::new()
        .route("/admin/config/recording-ttl", get(get_recording_ttl))
        .with_state(state);
    let (status, parsed) = test_req(
        &app,
        "GET",
        "/admin/config/recording-ttl",
        Some(&plaintext),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(parsed["recording_ttl_secs"], 300);
}

#[tokio::test]
async fn put_recording_ttl_persists_new_value() {
    let dir = tempdir();
    let (state, plaintext) = make_state_with_key(&dir).await;
    let app = Router::new()
        .route(
            "/admin/config/recording-ttl",
            get(get_recording_ttl).put(put_recording_ttl),
        )
        .with_state(state.clone());
    let (status, parsed) = test_req(
        &app,
        "PUT",
        "/admin/config/recording-ttl",
        Some(&plaintext),
        Some(serde_json::json!({ "recording_ttl_secs": 600_i64 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(parsed["recording_ttl_secs"], 600);
    assert_eq!(state.recording_ttl_secs(), 600);
    let value: String = state.db_pool().with_conn(|c| {
        c.query_row(
            "SELECT value FROM app_config WHERE key = 'recording_ttl_secs'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    });
    assert_eq!(serde_json::from_str::<i64>(&value).unwrap(), 600);
}

#[tokio::test]
async fn put_recording_ttl_rejects_negative_value() {
    let dir = tempdir();
    let (state, plaintext) = make_state_with_key(&dir).await;
    let app = Router::new()
        .route("/admin/config/recording-ttl", put(put_recording_ttl))
        .with_state(state.clone());
    let (status, _) = test_req(
        &app,
        "PUT",
        "/admin/config/recording-ttl",
        Some(&plaintext),
        Some(serde_json::json!({ "recording_ttl_secs": -1_i64 })),
    )
    .await;
    assert!(status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(state.recording_ttl_secs(), 300);
    assert_recording_ttl_db_count(&state, 0);
}

#[tokio::test]
async fn put_recording_ttl_rejects_missing_field() {
    let dir = tempdir();
    let (state, plaintext) = make_state_with_key(&dir).await;
    let app = Router::new()
        .route("/admin/config/recording-ttl", put(put_recording_ttl))
        .with_state(state.clone());
    let (status, _) = test_req(
        &app,
        "PUT",
        "/admin/config/recording-ttl",
        Some(&plaintext),
        Some(serde_json::json!({ "foo": "bar" })),
    )
    .await;
    assert!(status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(state.recording_ttl_secs(), 300);
    assert_recording_ttl_db_count(&state, 0);
}

#[tokio::test]
async fn put_recording_ttl_rejects_invalid_json_syntax() {
    let dir = tempdir();
    let (state, plaintext) = make_state_with_key(&dir).await;
    let app = Router::new()
        .route("/admin/config/recording-ttl", put(put_recording_ttl))
        .with_state(state.clone());
    let req = Request::builder()
        .method("PUT")
        .uri("/admin/config/recording-ttl")
        .header("authorization", format!("Bearer {plaintext}"))
        .header("content-type", "application/json")
        .body(Body::from(r"{invalid"))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert!(
        resp.status() == StatusCode::BAD_REQUEST
            || resp.status() == StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(state.recording_ttl_secs(), 300);
    assert_recording_ttl_db_count(&state, 0);
}

#[tokio::test]
async fn put_pii_persists_new_config_and_updates_memory() {
    let dir = tempdir();
    let (state, plaintext) = make_state_with_key(&dir).await;
    let app = Router::new()
        .route(
            "/admin/config/pii",
            get(crate::handlers::admin::runtime::get_runtime_pii)
                .put(crate::handlers::admin::runtime::put_runtime_pii),
        )
        .route(
            "/admin/config",
            get(crate::handlers::admin::runtime::get_runtime_config),
        )
        .with_state(state.clone());
    assert!(!state.pii_config().pii_enabled);
    let payload = serde_json::json!({"pii_enabled": true, "pii_reversible": true, "pii_redact_logs": false, "pii_entities": ["email", "card", "ip"]});
    let (status, _) = test_req(
        &app,
        "PUT",
        "/admin/config/pii",
        Some(&plaintext),
        Some(payload),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let current = state.pii_config();
    assert!(
        current.pii_enabled
            && current.pii_reversible
            && !current.pii_redact_logs
            && current.pii_entities.len() == 3
    );
    let loaded = state
        .db_pool()
        .with_conn(openproxy_db::app_config::load_pii_config_from_db)
        .unwrap()
        .unwrap();
    assert!(
        loaded.pii_enabled
            && loaded.pii_reversible
            && !loaded.pii_redact_logs
            && loaded.pii_entities.len() == 3
    );
    let (status, json) = test_req(&app, "GET", "/admin/config", Some(&plaintext), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["pii_enabled"], true);
    assert_eq!(json["pii_reversible"], true);
    assert_eq!(json["pii_redact_logs"], false);
}
