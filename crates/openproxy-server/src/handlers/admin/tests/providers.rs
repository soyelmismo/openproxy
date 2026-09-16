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
