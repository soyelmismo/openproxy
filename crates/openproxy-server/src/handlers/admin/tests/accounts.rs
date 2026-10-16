use super::common::*;
use crate::handlers::admin::accounts::{refresh_account_quota, reorder_accounts, scan_accounts};

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
async fn test_scan_endpoint_auto_import_creates_accounts() {
    let tmp = tempdir();
    let (state, plaintext) = make_state_with_key(tmp.path()).await;
    seed::seed_builtin_providers(&state.db_pool().writer()).unwrap();
    seed_token(
        tmp.path(),
        serde_json::json!({
            "token": { "access_token": "ya-import", "refresh_token": "1//import", "expiry": "2099-01-01T00:00:00Z" },
            "auth_method": "consumer", "user": { "email": "import@example.com" }
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
        Some(serde_json::json!({"dry_run": false, "auto_import": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let imported = parsed["imported"].as_array().unwrap();
    assert_eq!(imported.len(), 1);
    assert_eq!(imported[0]["provider_id"], "antigravity");

    let count: i64 = state.db_pool().with_conn(|c| {
        c.query_row("SELECT COUNT(*) FROM accounts", [], |r| r.get(0))
            .unwrap()
    });
    assert_eq!(count, 1);
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

/// Prioridad con la que quedó una cuenta en la DB.
fn priority_of(state: &AppState, account_id: i64) -> i32 {
    state.db_pool().with_conn(|c| {
        c.query_row(
            "SELECT priority FROM accounts WHERE id = ?1",
            rusqlite::params![account_id],
            |r| r.get::<_, i32>(0),
        )
        .unwrap()
    })
}

fn count_accounts(state: &AppState) -> i64 {
    state.db_pool().with_conn(|c| {
        c.query_row("SELECT COUNT(*) FROM accounts", [], |r| r.get(0))
            .unwrap()
    })
}

/// El endpoint reescribe el conjunto completo del proveedor como 10, 20, 30…
/// en el orden recibido.
#[tokio::test]
async fn reorder_endpoint_persists_requested_order() {
    let tmp = tempdir();
    let (state, plaintext) = make_state_with_key(tmp.path()).await;
    let a = insert_test_account(&state, "antigravity");
    let b = insert_test_account(&state, "antigravity");
    let c = insert_test_account(&state, "antigravity");

    let app = Router::new()
        .route("/admin/accounts/reorder", post(reorder_accounts))
        .with_state(state.clone());

    let (status, parsed) = test_req(
        &app,
        "POST",
        "/admin/accounts/reorder",
        Some(&plaintext),
        Some(serde_json::json!({
            "provider_id": "antigravity",
            "account_ids": [c, a, b]
        })),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(parsed["reordered"], true);
    assert_eq!(priority_of(&state, c), 10);
    assert_eq!(priority_of(&state, a), 20);
    assert_eq!(priority_of(&state, b), 30);
    assert_eq!(count_accounts(&state), 3);
}

/// Una lista parcial no debe reordenar en silencio: la validación vive en la DB
/// y el handler debe traducirla a 4xx, dejando las prioridades intactas.
#[tokio::test]
async fn reorder_endpoint_rejects_partial_list() {
    let tmp = tempdir();
    let (state, plaintext) = make_state_with_key(tmp.path()).await;
    let a = insert_test_account(&state, "antigravity");
    let _b = insert_test_account(&state, "antigravity");

    let app = Router::new()
        .route("/admin/accounts/reorder", post(reorder_accounts))
        .with_state(state.clone());

    let (status, _) = test_req(
        &app,
        "POST",
        "/admin/accounts/reorder",
        Some(&plaintext),
        Some(serde_json::json!({
            "provider_id": "antigravity",
            "account_ids": [a]
        })),
    )
    .await;

    assert!(status.is_client_error());
}

/// `provider_id` vacío se rechaza en el handler antes de tocar la DB.
#[tokio::test]
async fn reorder_endpoint_rejects_empty_provider() {
    let tmp = tempdir();
    let (state, plaintext) = make_state_with_key(tmp.path()).await;
    let a = insert_test_account(&state, "antigravity");

    let app = Router::new()
        .route("/admin/accounts/reorder", post(reorder_accounts))
        .with_state(state.clone());

    let (status, _) = test_req(
        &app,
        "POST",
        "/admin/accounts/reorder",
        Some(&plaintext),
        Some(serde_json::json!({
            "provider_id": "",
            "account_ids": [a]
        })),
    )
    .await;

    assert!(status.is_client_error());
}

/// Lista vacía: no hay nada que reordenar, se rechaza sin tocar la DB.
#[tokio::test]
async fn reorder_endpoint_rejects_empty_list() {
    let tmp = tempdir();
    let (state, plaintext) = make_state_with_key(tmp.path()).await;

    let app = Router::new()
        .route("/admin/accounts/reorder", post(reorder_accounts))
        .with_state(state.clone());

    let (status, _) = test_req(
        &app,
        "POST",
        "/admin/accounts/reorder",
        Some(&plaintext),
        Some(serde_json::json!({
            "provider_id": "antigravity",
            "account_ids": []
        })),
    )
    .await;

    assert!(status.is_client_error());
}

/// El reorden de un proveedor no debe tocar las cuentas de otro.
#[tokio::test]
async fn reorder_endpoint_is_scoped_to_its_provider() {
    let tmp = tempdir();
    let (state, plaintext) = make_state_with_key(tmp.path()).await;
    let x = insert_test_account(&state, "antigravity");
    let y = insert_test_account(&state, "antigravity");
    let other = insert_test_account(&state, "openai");

    let app = Router::new()
        .route("/admin/accounts/reorder", post(reorder_accounts))
        .with_state(state.clone());

    let (status, _) = test_req(
        &app,
        "POST",
        "/admin/accounts/reorder",
        Some(&plaintext),
        Some(serde_json::json!({
            "provider_id": "antigravity",
            "account_ids": [y, x]
        })),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(priority_of(&state, y), 10);
    assert_eq!(priority_of(&state, x), 20);
    assert_eq!(priority_of(&state, other), 0, "otro proveedor no se toca");
}

/// Un ID de otro proveedor en la lista debe rechazarse: el conjunto no cuadra.
#[tokio::test]
async fn reorder_endpoint_rejects_foreign_account() {
    let tmp = tempdir();
    let (state, plaintext) = make_state_with_key(tmp.path()).await;
    let a = insert_test_account(&state, "antigravity");
    let foreign = insert_test_account(&state, "openai");

    let app = Router::new()
        .route("/admin/accounts/reorder", post(reorder_accounts))
        .with_state(state.clone());

    let (status, _) = test_req(
        &app,
        "POST",
        "/admin/accounts/reorder",
        Some(&plaintext),
        Some(serde_json::json!({
            "provider_id": "antigravity",
            "account_ids": [a, foreign]
        })),
    )
    .await;

    assert!(status.is_client_error());
}

/// IDs duplicados: el conjunto no puede cuadrar dos veces contra el mismo hueco.
#[tokio::test]
async fn reorder_endpoint_rejects_duplicate_ids() {
    let tmp = tempdir();
    let (state, plaintext) = make_state_with_key(tmp.path()).await;
    let a = insert_test_account(&state, "antigravity");

    let app = Router::new()
        .route("/admin/accounts/reorder", post(reorder_accounts))
        .with_state(state.clone());

    let (status, _) = test_req(
        &app,
        "POST",
        "/admin/accounts/reorder",
        Some(&plaintext),
        Some(serde_json::json!({
            "provider_id": "antigravity",
            "account_ids": [a, a]
        })),
    )
    .await;

    assert!(status.is_client_error());
}

/// El endpoint vive tras `admin_auth_middleware`: sin token, 401. Monta el
/// middleware real, no el handler pelado, o la prueba no demuestra nada.
#[tokio::test]
async fn reorder_endpoint_requires_auth() {
    let tmp = tempdir();
    let (state, _plaintext) = make_state_with_key(tmp.path()).await;
    insert_test_account(&state, "antigravity");

    let app = Router::new()
        .route("/admin/accounts/reorder", post(reorder_accounts))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::handlers::admin::auth::admin_auth_middleware,
        ))
        .layer(axum::Extension(axum::extract::connect_info::ConnectInfo(
            "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
        )))
        .with_state(state.clone());

    let (status, _) = test_req(
        &app,
        "POST",
        "/admin/accounts/reorder",
        None,
        Some(serde_json::json!({
            "provider_id": "antigravity",
            "account_ids": [1]
        })),
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// Con token válido el mismo montaje con middleware responde 200: la capa de
/// auth deja pasar y el handler ejecuta el reorden.
#[tokio::test]
async fn reorder_endpoint_accepts_valid_admin_token() {
    let tmp = tempdir();
    let (state, plaintext) = make_state_with_key(tmp.path()).await;
    let a = insert_test_account(&state, "antigravity");
    let b = insert_test_account(&state, "antigravity");

    let app = Router::new()
        .route("/admin/accounts/reorder", post(reorder_accounts))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::handlers::admin::auth::admin_auth_middleware,
        ))
        .layer(axum::Extension(axum::extract::connect_info::ConnectInfo(
            "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
        )))
        .with_state(state.clone());

    let (status, parsed) = test_req(
        &app,
        "POST",
        "/admin/accounts/reorder",
        Some(&plaintext),
        Some(serde_json::json!({
            "provider_id": "antigravity",
            "account_ids": [b, a]
        })),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(parsed["reordered"], true);
    assert_eq!(priority_of(&state, b), 10);
    assert_eq!(priority_of(&state, a), 20);
}
