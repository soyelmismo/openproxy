use super::*;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use openproxy_adapters::adapters;
use openproxy_core::AppConfig;
use openproxy_db as core_db;
use openproxy_db::MasterKey;
use parking_lot::RwLock;
use std::path::PathBuf;
use std::sync::Arc;
use tower::ServiceExt;

async fn make_state() -> AppState {
    let (pool, _path) = fresh_pool();
    let db_pool = Arc::new(pool);
    let master_key = Arc::new(MasterKey::generate().unwrap());
    let adapters = Arc::new(RwLock::new(Arc::new(
        Vec::<adapters::ProviderAdapterEnum>::new(),
    )));
    let mut config = AppConfig::default();
    config.server.allow_anonymous = true;
    AppState::for_test(config, db_pool, master_key, adapters)
}

fn fresh_pool() -> (core_db::DbPool, PathBuf) {
    let pool = core_db::DbPool::test_pool_with_prefix("openproxy-router-test").expect("open pool");
    let path = pool.path().to_path_buf();
    (pool, path)
}

#[tokio::test]
async fn dedicated_routers_isolate_surfaces() {
    let state = make_state().await;
    let public = build_public_router(state.clone());
    for path in [
        "/",
        "/admin",
        "/admin/",
        "/admin/health",
        "/admin/api/config",
        "/admin/ws",
        "/admin/dist/app.js",
    ] {
        let response = public
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "public {path}");
        assert!(response.headers().contains_key("x-request-id"));
    }
    let admin = build_admin_listener_router(state).layer(axum::Extension(
        axum::extract::ConnectInfo("127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap()),
    ));
    for path in ["/v1/health", "/v1/models"] {
        let response = admin
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "admin {path}");
    }
    let response = admin
        .oneshot(
            Request::builder()
                .uri("/admin/api/config")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_public_health() {
    let state = make_state().await;
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/health")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    assert_eq!(
        response
            .headers()
            .get(axum::http::header::X_CONTENT_TYPE_OPTIONS)
            .unwrap(),
        "nosniff"
    );
    assert_eq!(response.headers().get("x-frame-options").unwrap(), "DENY");
    let csp = response
        .headers()
        .get("content-security-policy")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(csp.contains("default-src 'self';"), "{csp}");
    assert!(csp.contains("style-src-elem 'self'"), "{csp}");
    assert!(
        csp.contains("connect-src 'self'"),
        "no Host header → no ws origins: {csp}"
    );

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["status"], "ok");
}

#[tokio::test]
async fn test_models_catalog_requires_key_unless_anonymous_opt_in() {
    // No active keys + `allow_anonymous = false` (the default): the catalog
    // must not leak. Same gate as chat.
    let (pool, _path) = fresh_pool();
    let db_pool = Arc::new(pool);
    let master_key = Arc::new(MasterKey::generate().unwrap());
    let adapters = Arc::new(RwLock::new(Arc::new(
        Vec::<adapters::ProviderAdapterEnum>::new(),
    )));
    let config = AppConfig::default();
    assert!(!config.server.allow_anonymous, "default must be closed");
    let state = AppState::for_test(config, db_pool, master_key, adapters);
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // The explicit opt-in (`make_state()`) keeps the first-boot window open.
    let app = build_router(make_state().await);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_csp_pins_ws_origin_to_host_header() {
    let state = make_state().await;
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/health")
                .header("host", "listo.click")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let csp = response
        .headers()
        .get("content-security-policy")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(csp.contains("ws://listo.click wss://listo.click;"), "{csp}");
    assert!(!csp.contains("connect-src 'self' ws: wss:"), "{csp}");
    assert!(csp.contains("frame-ancestors 'none';"), "{csp}");
}

#[tokio::test]
async fn test_admin_panel_framing_security_headers() {
    let state = make_state().await;
    let app = build_router(state);

    // 1. Allowed paths: /admin/panel and /admin/panel/ must allow same-origin framing.
    for path in ["/admin/panel", "/admin/panel/"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK, "{path} should be 200 OK");
        assert_eq!(
            response.headers().get("x-frame-options").unwrap(),
            "SAMEORIGIN",
            "x-frame-options for {path}"
        );
        let csp = response
            .headers()
            .get("content-security-policy")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            csp.contains("frame-ancestors 'self';"),
            "CSP frame-ancestors for {path}: {csp}"
        );
        assert!(
            !csp.contains("frame-ancestors 'none';"),
            "CSP should not contain 'none' for {path}"
        );

        // Confirm it serves the index.html shell
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body_str = std::str::from_utf8(&body).unwrap();
        assert!(
            body_str.contains("<!doctype html>") || body_str.contains("<html"),
            "must serve index_html for {path}"
        );
    }

    // 2. Denied paths: every other route must strictly enforce DENY and frame-ancestors 'none'
    for denied_path in [
        "/admin",
        "/admin/callback.html",
        "/v1/health",
        "/admin/panel?foo=bar", // query string on /admin/panel is tested below too
        "/admin/panel/sub",
        "/admin/panel-other",
        "/admin/panela",
        "/admin/panel/?foo=1",
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(denied_path)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // Note: /admin redirects to /admin or serves SPA shell, or whatever status;
        // regardless of status, x-frame-options and frame-ancestors MUST be denied unless
        // the exact path is /admin/panel or /admin/panel/.
        let path_only = denied_path.split('?').next().unwrap();
        if path_only == "/admin/panel" || path_only == "/admin/panel/" {
            // If it's a query override attempt on /admin/panel, it still allows same-origin framing
            // for the panel route itself, but we verify no arbitrary query can trick other routes.
            assert_eq!(
                response.headers().get("x-frame-options").unwrap(),
                "SAMEORIGIN"
            );
        } else {
            assert_eq!(
                response.headers().get("x-frame-options").unwrap(),
                "DENY",
                "x-frame-options must be DENY for {denied_path}"
            );
            let csp = response
                .headers()
                .get("content-security-policy")
                .unwrap()
                .to_str()
                .unwrap();
            assert!(
                csp.contains("frame-ancestors 'none';"),
                "frame-ancestors must be 'none' for {denied_path}"
            );
        }
    }

    // Specifically test other routes with query parameters attempting overrides:
    for override_attempt in [
        "/admin?frame=1",
        "/admin/?frame_ancestors=self",
        "/admin/callback.html?frame=self",
        "/v1/health?x-frame-options=SAMEORIGIN",
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(override_attempt)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            response.headers().get("x-frame-options").unwrap(),
            "DENY",
            "x-frame-options must stay DENY on {override_attempt}"
        );
        let csp = response
            .headers()
            .get("content-security-policy")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            csp.contains("frame-ancestors 'none';"),
            "frame-ancestors must stay 'none' on {override_attempt}"
        );
    }
}

#[tokio::test]
async fn test_admin_api_fallback_404_json() {
    // Unmatched /admin/api/* must return JSON 404, not the HTML SPA shell.
    let state = make_state().await;
    let app = build_router(state.clone()).layer(axum::Extension(
        axum::extract::connect_info::ConnectInfo(
            "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
        ),
    ));

    let api_key = "test-api-key-123";
    let key_hash = openproxy_core::api_keys::hash_key(api_key);
    {
        let w = state.db_pool().writer();
        w.execute(
            "INSERT INTO api_keys (key_hash, key_prefix, label, scopes_json, \
                    allowed_models_json, allowed_combos_json, expires_at, created_by) \
                 VALUES (?1, ?2, ?3, ?4, NULL, NULL, NULL, 'test')",
            rusqlite::params![
                key_hash,
                &api_key[..api_key.len().min(12)],
                "smoke-test",
                "[\"manage\"]",
            ],
        )
        .unwrap();
    }

    let response = app
        .oneshot(
            Request::builder()
                .uri("/admin/api/does-not-exist-12345")
                .header("Authorization", format!("Bearer {api_key}"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/json"
    );

    assert_eq!(
        response
            .headers()
            .get(axum::http::header::X_CONTENT_TYPE_OPTIONS)
            .unwrap(),
        "nosniff"
    );
    assert_eq!(response.headers().get("x-frame-options").unwrap(), "DENY");
    let csp = response
        .headers()
        .get("content-security-policy")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(csp.contains("default-src 'self';"), "{csp}");
    assert!(csp.contains("style-src-elem 'self'"), "{csp}");
    assert!(
        csp.contains("connect-src 'self'"),
        "no Host header → no ws origins: {csp}"
    );

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["error"]["code"], "not_found");
}

#[tokio::test]
async fn test_transport_compression_json_gzip() {
    let state = make_state().await;
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .header("Accept-Encoding", "gzip")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-encoding")
            .and_then(|v| v.to_str().ok()),
        Some("gzip")
    );
}

#[tokio::test]
async fn test_transport_compression_json_gzip_fallback_or_bypass() {
    let state = make_state().await;
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .header("Accept-Encoding", "gzip, zstd")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-encoding")
            .and_then(|v| v.to_str().ok()),
        Some("gzip")
    );
}

#[tokio::test]
async fn test_image_generations_not_found_returns_404() {
    let state = make_state().await;
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/images/generations")
                .header("Content-Type", "application/json")
                .body(axum::body::Body::from(
                    r#"{"prompt":"a painting of a sunset","model":"nonexistent-image-model"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_image_generations_empty_prompt_validation() {
    let state = make_state().await;
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/images/generations")
                .header("Content-Type", "application/json")
                .body(axum::body::Body::from(
                    r#"{"prompt":"","model":"dall-e-3"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_embeddings_not_found_returns_404() {
    let state = make_state().await;
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/embeddings")
                .header("Content-Type", "application/json")
                .body(axum::body::Body::from(
                    r#"{"input":"hello world","model":"nonexistent-embedding-model"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_embeddings_empty_input_validation() {
    let state = make_state().await;
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/embeddings")
                .header("Content-Type", "application/json")
                .body(axum::body::Body::from(
                    r#"{"input":"","model":"text-embedding-3-small"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_image_edits_missing_image_returns_400() {
    let state = make_state().await;
    let app = build_router(state);

    let boundary = "------------------------boundary123";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\nedit test\r\n--{boundary}--\r\n"
    );

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/images/edits")
                .header(
                    "Content-Type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(axum::body::Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_image_edits_missing_prompt_returns_400() {
    let state = make_state().await;
    let app = build_router(state);

    let boundary = "------------------------boundary123";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"image\"; filename=\"image.png\"\r\nContent-Type: image/png\r\n\r\nfakeimagebytes\r\n--{boundary}--\r\n"
    );

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/images/edits")
                .header(
                    "Content-Type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(axum::body::Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_image_variations_missing_image_returns_400() {
    let state = make_state().await;
    let app = build_router(state);

    let boundary = "------------------------boundary123";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"n\"\r\n\r\n1\r\n--{boundary}--\r\n"
    );

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/images/variations")
                .header(
                    "Content-Type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(axum::body::Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_image_edits_not_found_returns_404() {
    let state = make_state().await;
    let app = build_router(state);

    let boundary = "------------------------boundary123";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nnonexistent-image-model\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\nedit test\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"image\"; filename=\"image.png\"\r\nContent-Type: image/png\r\n\r\nfakeimagebytes\r\n--{boundary}--\r\n"
    );

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/images/edits")
                .header(
                    "Content-Type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(axum::body::Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
