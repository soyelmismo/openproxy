use super::common::*;
use crate::handlers::admin::accounts::{refresh_account_quota, scan_accounts};

#[tokio::test]
async fn quota_refresh_supported_matrix() {
    let adapters = openproxy_adapters::adapters::builtin_adapters();
    let supported: Vec<String> = adapters
        .iter()
        .filter(|a| a.metadata().quota_refresh_supported)
        .map(|a| a.id().to_string())
        .collect();

    assert!(
        supported.contains(&"antigravity".to_string()),
        "antigravity must support quota refresh"
    );
    assert!(
        !supported.contains(&"openrouter".to_string()),
        "openrouter must NOT support quota refresh"
    );
    assert!(
        supported.contains(&"kiro".to_string()),
        "kiro must support quota refresh"
    );
    assert!(
        !supported.contains(&"openai".to_string()),
        "openai must NOT support quota refresh"
    );
    assert!(
        !supported.contains(&"anthropic".to_string()),
        "anthropic must NOT support quota refresh"
    );
}

#[tokio::test]
async fn refresh_account_quota_unsupported_provider_responds_fast() {
    let dir = tempdir();
    let (state, plaintext) = make_state_with_key(&dir).await;
    let account_id = insert_test_account(&state, "openai");
    let app = Router::new()
        .route(
            "/admin/accounts/{id}/refresh-quota",
            post(refresh_account_quota),
        )
        .with_state(state);
    let (status, v) = test_req(
        &app,
        "POST",
        &format!("/admin/accounts/{account_id}/refresh-quota"),
        Some(&plaintext),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["supported"], false);
}

#[tokio::test]
async fn refresh_account_quota_nonexistent_account_responds_fast() {
    let dir = tempdir();
    let (state, plaintext) = make_state_with_key(&dir).await;
    let app = Router::new()
        .route(
            "/admin/accounts/{id}/refresh-quota",
            post(refresh_account_quota),
        )
        .with_state(state);
    let (status, _) = test_req(
        &app,
        "POST",
        "/admin/accounts/99999/refresh-quota",
        Some(&plaintext),
        None,
    )
    .await;
    assert!(status.is_client_error() || status.is_server_error());
}

fn seed_token(path: &std::path::Path, val: serde_json::Value) {
    let p = path.join(".gemini/antigravity-cli/antigravity-oauth-token");
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, serde_json::to_vec(&val).unwrap()).unwrap();
}

#[tokio::test]
async fn test_scan_endpoint_dry_run_does_not_create() {
    let tmp = tempdir();
    let (state, plaintext) = make_state_with_key(tmp.path()).await;
    seed::seed_builtin_providers(&state.db_pool().writer()).unwrap();
    seed_token(
        tmp.path(),
        serde_json::json!({
            "token": { "access_token": "ya-dry", "refresh_token": "1//dry", "expiry": "2099-01-01T00:00:00Z" },
            "auth_method": "consumer", "user": { "email": "dry@example.com" }
        }),
    );
    let _lock = SCAN_TEST_LOCK.lock().await;
    let _home = HomeGuard::set(tmp.path());
    let app = Router::new()
        .route("/admin/accounts/scan", post(scan_accounts))
        .with_state(state.clone());
    let (status, parsed) = test_req(
        &app,
        "POST",
        "/admin/accounts/scan",
        Some(&plaintext),
        Some(serde_json::json!({"dry_run": true, "auto_import": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!parsed["scanned"].as_array().unwrap().is_empty());
    assert!(parsed["imported"].as_array().unwrap().is_empty());
    let count: i64 = state.db_pool().with_conn(|c| {
        c.query_row("SELECT COUNT(*) FROM accounts", [], |r| r.get(0))
            .unwrap()
    });
    assert_eq!(count, 0);
}

#[tokio::test]
async fn adv_scan_endpoint_empty_body_defaults() {
    let tmp = tempdir();
    let (state, plaintext) = make_state_with_key(tmp.path()).await;
    seed::seed_builtin_providers(&state.db_pool().writer()).unwrap();
    seed_token(
        tmp.path(),
        serde_json::json!({"token": {"access_token": "ya-adv", "refresh_token": "1//adv"}}),
    );
    let _lock = SCAN_TEST_LOCK.lock().await;
    let _home = HomeGuard::set(tmp.path());
    let app = Router::new()
        .route("/admin/accounts/scan", post(scan_accounts))
        .with_state(state.clone());
    let (status, parsed) = test_req(
        &app,
        "POST",
        "/admin/accounts/scan",
        Some(&plaintext),
        Some(serde_json::json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(parsed["imported"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn adv_scan_endpoint_malformed_json_body_returns_4xx() {
    let tmp = tempdir();
    let (state, plaintext) = make_state_with_key(tmp.path()).await;
    let app = Router::new()
        .route("/admin/accounts/scan", post(scan_accounts))
        .with_state(state.clone());
    let req = Request::builder()
        .method("POST")
        .uri("/admin/accounts/scan")
        .header("authorization", format!("Bearer {plaintext}"))
        .header("content-type", "application/json")
        .body(Body::from("{ not json"))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert!(resp.status().is_client_error());
}

#[tokio::test]
async fn adv_scan_endpoint_with_dry_run_true_and_auto_import_true() {
    let tmp = tempdir();
    let (state, plaintext) = make_state_with_key(tmp.path()).await;
    seed::seed_builtin_providers(&state.db_pool().writer()).unwrap();
    seed_token(
        tmp.path(),
        serde_json::json!({"token": {"access_token": "ya-both", "refresh_token": "1//both"}}),
    );
    let _lock = SCAN_TEST_LOCK.lock().await;
    let _home = HomeGuard::set(tmp.path());
    let app = Router::new()
        .route("/admin/accounts/scan", post(scan_accounts))
        .with_state(state.clone());
    let (status, parsed) = test_req(
        &app,
        "POST",
        "/admin/accounts/scan",
        Some(&plaintext),
        Some(serde_json::json!({"dry_run": true, "auto_import": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(parsed["imported"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn adv_scan_endpoint_array_body_returns_4xx() {
    let tmp = tempdir();
    let (state, plaintext) = make_state_with_key(tmp.path()).await;
    let app = Router::new()
        .route("/admin/accounts/scan", post(scan_accounts))
        .with_state(state.clone());
    let req = Request::builder()
        .method("POST")
        .uri("/admin/accounts/scan")
        .header("authorization", format!("Bearer {plaintext}"))
        .header("content-type", "application/json")
        .body(Body::from("[1, 2, 3]"))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert!(resp.status().is_client_error());
}
