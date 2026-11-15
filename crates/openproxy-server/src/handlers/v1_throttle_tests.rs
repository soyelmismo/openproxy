//! Integration tests for the public `/v1` pre-authentication throttle
//! (audit H-3): failed credentials must be rate-limited per client IP.
//!
//! Root cause being pinned: in `chat_endpoint` the auth middleware is the
//! OUTERMOST route layer, so a request carrying an invalid or missing Bearer
//! token is rejected with 401 before `rate_limit_middleware` ever runs — the
//! per-IP branch of the rate limiter was unreachable for unauthenticated
//! traffic. `GET /v1/models` performs auth in-handler and had no middleware
//! at all. An attacker could therefore try candidate API keys at full line
//! rate (SHA-256 + SQLite lookup per attempt) with no 429 and no friction.

#![cfg(test)]

use crate::state::AppState;
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use tower::ServiceExt;

async fn make_app() -> AppState {
    let dir = openproxy_db::testing::TempDir::new("v1-throttle-test").expect("mkdir");
    let pool =
        std::sync::Arc::new(openproxy_db::DbPool::open(&dir.join("throttle.db")).expect("open"));
    {
        let mut w = pool.writer();
        openproxy_db::migrations::run(&mut w).expect("migrations");
    }
    let mk = openproxy_db::MasterKey::generate().unwrap();
    let adapters_registry = std::sync::Arc::new(parking_lot::RwLock::new(std::sync::Arc::new(
        openproxy_adapters::adapters::builtin_adapters(),
    )));
    AppState::for_test(
        openproxy_core::AppConfig::default(),
        pool,
        std::sync::Arc::new(mk),
        adapters_registry,
    )
}

async fn get_v1_models(app: &Router, bearer: Option<&str>) -> StatusCode {
    let mut builder = Request::builder().uri("/v1/models");
    if let Some(token) = bearer {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let request = builder.body(Body::empty()).expect("request");
    let response = app.clone().oneshot(request).await.expect("oneshot");
    response.status()
}

#[tokio::test]
async fn failed_v1_auth_attempts_are_throttled_per_ip() {
    let state = make_app().await;
    let app = Router::new()
        .merge(crate::handlers::public_api_routes(&state))
        .with_state(state.clone());

    // Seed a valid key and authenticate once, so it lands in the in-memory
    // key cache (the escape hatch for blocked IPs verifies against the cache
    // only — no DB — so brute force cannot burn SQLite while blocked).
    let plaintext = format!("op_live_throttle_ok_{}", std::process::id());
    {
        let w = state.db_pool().writer();
        let key_hash = openproxy_core::api_keys::hash_key(&plaintext);
        w.execute(
            "INSERT OR REPLACE INTO api_keys (key_hash, key_prefix, label, scopes_json, created_by) \
             VALUES (?1, ?2, 'throttle-test', '[\"chat\"]', 'throttle-test')",
            rusqlite::params![key_hash, &plaintext[..plaintext.len().min(12)]],
        )
        .expect("insert key");
    }
    let status = get_v1_models(&app, Some(&plaintext)).await;
    assert_eq!(status, StatusCode::OK, "valid key authenticates");

    // 20 failing attempts (invalid key on /v1/models, the endpoint that
    // previously had NO middleware at all): all must be 401.
    for _ in 0..20 {
        let status = get_v1_models(&app, Some("op_live_invalidkey")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    // The 21st attempt from the same client IP must be throttled BEFORE
    // authenticate runs: 429, not another unthrottled 401.
    let status = get_v1_models(&app, Some("op_live_invalidkey")).await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "pre-auth brute force must hit the throttle"
    );

    // The chat endpoint (auth middleware path) is equally covered by the
    // outermost layer.
    let request = Request::builder()
        .uri("/v1/chat/completions")
        .method("POST")
        .header("authorization", "Bearer op_live_invalidkey")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"model":"x","messages":[]}"#))
        .expect("request");
    let response = app.clone().oneshot(request).await.expect("oneshot");
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);

    // Escape hatch: the CACHED valid key still passes while the IP is
    // blocked (another client behind the same NAT must not be locked out),
    // and its success clears the failure budget.
    let status = get_v1_models(&app, Some(&plaintext)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "cached valid key must pass while blocked"
    );
    let status = get_v1_models(&app, Some("op_live_invalidkey")).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "budget was cleared by the success, so one new failure is a plain 401"
    );
}
