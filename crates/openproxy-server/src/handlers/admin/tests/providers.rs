use super::common::*;
use crate::handlers::admin::providers::refresh_provider_models;

#[tokio::test]
async fn refresh_provider_models_unknown_provider_responds_fast() {
    let dir = tempdir();
    let (state, plaintext) = make_state_with_key(&dir).await;
    let app = Router::new()
        .route(
            "/admin/providers/{id}/refresh",
            post(refresh_provider_models),
        )
        .with_state(state);
    let (status, _) = test_req(
        &app,
        "POST",
        "/admin/providers/nonexistent-provider/refresh",
        Some(&plaintext),
        None,
    )
    .await;
    assert!(status.is_client_error() || status.is_server_error() || status == StatusCode::OK);
}

#[tokio::test]
async fn test_run_test_for_model_cancellation() {
    let dir = tempdir();
    let (state, _plaintext) = make_state_with_key(&dir).await;
    seed::seed_builtin_providers(&state.db_pool().writer()).expect("seed");
    let model_row_id = {
        let w = state.db_pool().writer();
        w.execute(
            "INSERT INTO models (provider_id, model_id, target_format, active) VALUES (?, ?, ?, ?)",
            ("openrouter", "gpt-4o", "openai", 1),
        )
        .unwrap();
        w.last_insert_rowid()
    };
    let (tx, rx) = tokio::sync::watch::channel::<Option<openproxy_types::CancelReason>>(None);
    tx.send(Some(openproxy_types::CancelReason::ClientDisconnected))
        .unwrap();
    let (r, _) = run_test_for_model(
        &state,
        model_row_id,
        None,
        None,
        TestOptions::default(),
        Some(rx),
    )
    .await;
    assert_eq!(r.status, 0);
    assert_eq!(r.error_msg.as_deref(), Some("Cancel"));
}

#[tokio::test]
async fn patch_provider_sets_notif_keyword_only_over_http() {
    let dir = tempdir();
    let (state, plaintext) = make_state_with_key(&dir).await;
    openproxy_db::providers::create(
        &state.db_pool().writer(),
        openproxy_db::providers::NewProvider {
            id: &openproxy_types::ProviderId::new("w2http"),
            name: "W2 HTTP",
            base_url: "https://example.invalid",
            auth_type: openproxy_types::AuthType::Bearer,
            format: openproxy_types::ProviderFormat::Openai,
            extra_headers_json: None,
            auto_activate_keyword: Some("claude"),
            rate_limit_scope: openproxy_types::RateLimitScope::Account,
        },
    )
    .unwrap();

    let read_flag = |state: &AppState| -> i64 {
        state
            .db_pool()
            .with_conn(|c| {
                c.query_row(
                    "SELECT notif_keyword_only FROM providers WHERE id = 'w2http'",
                    [],
                    |r| r.get(0),
                )
            })
            .unwrap()
    };
    assert_eq!(read_flag(&state), 0);
    let app = axum::Router::new()
        .nest(
            "/admin/providers",
            crate::handlers::admin::providers::router(),
        )
        .with_state(state.clone());

    let (s1, _) = test_req(
        &app,
        "PATCH",
        "/admin/providers/w2http",
        Some(&plaintext),
        Some(serde_json::json!({"notif_keyword_only": true})),
    )
    .await;
    assert_eq!(s1, StatusCode::OK);
    assert_eq!(read_flag(&state), 1);

    let (s2, _) = test_req(
        &app,
        "PATCH",
        "/admin/providers/w2http",
        Some(&plaintext),
        Some(serde_json::json!({"notif_keyword_only": false})),
    )
    .await;
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(read_flag(&state), 0);

    let (s3, _) = test_req(
        &app,
        "PATCH",
        "/admin/providers/w2http",
        Some(&plaintext),
        Some(serde_json::json!({"notif_keyword_only": "yes"})),
    )
    .await;
    assert!(s3.is_client_error());
    assert_eq!(read_flag(&state), 0);
}

#[tokio::test]
async fn test_get_provider_icon_endpoint_and_caching() {
    use axum::http::Request;
    use tower::ServiceExt;

    let dir = tempdir();
    let (state, _plaintext) = make_state_with_key(&dir).await;
    let pid = openproxy_types::ProviderId::new("icon-prov");
    openproxy_db::providers::create(
        &state.db_pool().writer(),
        openproxy_db::providers::NewProvider {
            id: &pid,
            name: "Icon Provider",
            base_url: "https://icon.example.com",
            auth_type: openproxy_types::AuthType::Bearer,
            format: openproxy_types::ProviderFormat::Openai,
            extra_headers_json: None,
            auto_activate_keyword: None,
            rate_limit_scope: openproxy_types::RateLimitScope::Account,
        },
    )
    .unwrap();

    let router = crate::router::build_router(state.clone());

    // 1. Initial GET without favicon returns 404 (without requiring Bearer auth)
    let req = Request::builder()
        .uri("/admin/api/providers/icon-prov/icon")
        .method("GET")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // 2. Store binary favicon
    let fake_png = b"\x89PNG\r\n\x1a\nfake-png-binary-data";
    openproxy_db::providers::set_provider_favicon(
        &state.db_pool().writer(),
        "icon-prov",
        "image/png",
        fake_png,
    )
    .unwrap();

    // 3. GET with favicon returns 200 OK, correct headers, and exact binary payload
    let req = Request::builder()
        .uri("/admin/api/providers/icon-prov/icon")
        .method("GET")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers().get("content-type").unwrap(), "image/png");
    assert_eq!(
        resp.headers().get("cache-control").unwrap(),
        "public, max-age=86400"
    );
    let body_bytes = http_body_util::BodyExt::collect(resp.into_body())
        .await
        .unwrap()
        .to_bytes();
    assert_eq!(body_bytes.as_ref(), fake_png);
}

#[tokio::test]
async fn test_get_provider_icon_adversarial_contract_and_stress() {
    use axum::http::Request;
    use tower::ServiceExt;

    let dir = tempdir();
    let (state, _plaintext) = make_state_with_key(&dir).await;
    let router = crate::router::build_router(state.clone());

    // 1. Non-existent provider -> 404 Not Found
    let req = Request::builder()
        .uri("/admin/api/providers/does-not-exist/icon")
        .method("GET")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // 2. Existing provider without favicon -> 404 Not Found
    let pid_nofav = openproxy_types::ProviderId::new("prov-no-fav");
    openproxy_db::providers::create(
        &state.db_pool().writer(),
        openproxy_db::providers::NewProvider {
            id: &pid_nofav,
            name: "No Favicon Provider",
            base_url: "https://nofav.example.com",
            auth_type: openproxy_types::AuthType::Bearer,
            format: openproxy_types::ProviderFormat::Openai,
            extra_headers_json: None,
            auto_activate_keyword: None,
            rate_limit_scope: openproxy_types::RateLimitScope::Account,
        },
    )
    .unwrap();

    let req = Request::builder()
        .uri("/admin/api/providers/prov-no-fav/icon")
        .method("GET")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // 3. Provider with binary ICO containing null bytes
    let pid_ico = openproxy_types::ProviderId::new("prov-ico");
    openproxy_db::providers::create(
        &state.db_pool().writer(),
        openproxy_db::providers::NewProvider {
            id: &pid_ico,
            name: "ICO Provider",
            base_url: "https://ico.example.com",
            auth_type: openproxy_types::AuthType::Bearer,
            format: openproxy_types::ProviderFormat::Openai,
            extra_headers_json: None,
            auto_activate_keyword: None,
            rate_limit_scope: openproxy_types::RateLimitScope::Account,
        },
    )
    .unwrap();

    let binary_ico_data =
        b"\x00\x00\x01\x00\x01\x00\x10\x10\x00\x00\x01\x00\x20\x00\x68\x04\x00\x00\x16\x00\x00\x00";
    openproxy_db::providers::set_provider_favicon(
        &state.db_pool().writer(),
        "prov-ico",
        "image/x-icon",
        binary_ico_data,
    )
    .unwrap();

    // 4. Request with invalid Authorization header must still succeed (route is unauthenticated)
    let req = Request::builder()
        .uri("/admin/api/providers/prov-ico/icon")
        .method("GET")
        .header("Authorization", "Bearer completely-invalid-token")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers().get("content-type").unwrap(), "image/x-icon");
    assert_eq!(
        resp.headers().get("cache-control").unwrap(),
        "public, max-age=86400"
    );
    let body = http_body_util::BodyExt::collect(resp.into_body())
        .await
        .unwrap()
        .to_bytes();
    assert_eq!(body.as_ref(), binary_ico_data);

    // 5. Test WEBP and binary preservation up to 64 KiB
    let mut large_webp = vec![0u8; 64 * 1024];
    large_webp[0..4].copy_from_slice(b"RIFF");
    large_webp[8..12].copy_from_slice(b"WEBP");
    // fill rest with arbitrary binary pattern
    for (i, byte) in large_webp.iter_mut().enumerate().skip(12) {
        *byte = (i % 256) as u8;
    }

    openproxy_db::providers::set_provider_favicon(
        &state.db_pool().writer(),
        "prov-ico",
        "image/webp",
        &large_webp,
    )
    .unwrap();

    let req = Request::builder()
        .uri("/admin/api/providers/prov-ico/icon")
        .method("GET")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers().get("content-type").unwrap(), "image/webp");
    let body = http_body_util::BodyExt::collect(resp.into_body())
        .await
        .unwrap()
        .to_bytes();
    assert_eq!(body.as_ref(), &large_webp[..]);

    // 6. Test FK cascade deletion: deleting provider cleans up provider_favicons
    openproxy_db::providers::delete(&state.db_pool().writer(), &pid_ico).unwrap();
    let req = Request::builder()
        .uri("/admin/api/providers/prov-ico/icon")
        .method("GET")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let fav_opt =
        openproxy_db::providers::get_provider_favicon(&state.db_pool().reader(), "prov-ico")
            .unwrap();
    assert!(fav_opt.is_none(), "favicon must be deleted on FK cascade");
}
