use super::common::*;
use axum::body::to_bytes;
use openproxy_types::backup::{BackupBundle, BackupValidationSummary, RestoreReport};
use serde_json::json;

#[tokio::test]
async fn test_backup_export_validate_and_restore_endpoints() {
    let tmp = tempdir();
    let (state, token) = make_state_with_key(tmp.path()).await;

    // Seed test provider and account
    let acc_id = insert_test_account(&state, "backup-test-prov");
    assert!(acc_id > 0);

    let app = crate::router::build_router(state.clone()).layer(axum::Extension(
        axum::extract::connect_info::ConnectInfo(
            "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
        ),
    ));

    // 1. Export unencrypted: rejected without the explicit opt-in (OP-16)...
    let req = Request::builder()
        .method("GET")
        .uri("/admin/api/backup/export")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // ...and the query-string passphrase is rejected with a pointer to the
    // header (OP-05).
    let req = Request::builder()
        .method("GET")
        .uri("/admin/api/backup/export?passphrase=leaky")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body_bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let body_str = String::from_utf8_lossy(&body_bytes);
    assert!(body_str.contains("x-backup-passphrase"), "{body_str}");

    // ...but works with the explicit plaintext opt-in.
    let req = Request::builder()
        .method("GET")
        .uri("/admin/api/backup/export?plaintext=confirmed")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("content-disposition").unwrap(),
        "attachment; filename=\"openproxy-backup.json\""
    );

    let body_bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let bundle: BackupBundle = serde_json::from_slice(&body_bytes).unwrap();
    assert!(!bundle.encrypted);
    assert!(bundle.payload.is_some());
    let payload = bundle.payload.as_ref().unwrap();
    assert!(
        payload
            .providers
            .iter()
            .any(|p| p.id.as_str() == "backup-test-prov")
    );

    // 2. Validate
    let req = Request::builder()
        .method("POST")
        .uri("/admin/api/backup/validate")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&bundle).unwrap()))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body_bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let summary: BackupValidationSummary = serde_json::from_slice(&body_bytes).unwrap();
    assert!(summary.providers_count >= 1);

    // 3. Clear/modify providers in state
    {
        let w = state.db_pool().writer();
        let _ = w.execute("DELETE FROM accounts", []);
        let _ = w.execute("DELETE FROM providers WHERE id = 'backup-test-prov'", []);
    }

    // 4. Restore
    let req = Request::builder()
        .method("POST")
        .uri("/admin/api/backup/restore")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&bundle).unwrap()))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body_bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let report: RestoreReport = serde_json::from_slice(&body_bytes).unwrap();
    assert!(report.success);
    assert!(report.providers_restored >= 1);

    // Verify restored provider is present in DB again
    let prov_exists: bool = state.db_pool().with_conn(|c| {
        c.query_row(
            "SELECT COUNT(*) FROM providers WHERE id = 'backup-test-prov'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            > 0
    });
    assert!(prov_exists);
}

#[tokio::test]
async fn test_backup_encrypted_export_and_restore() {
    let tmp = tempdir();
    let (state, token) = make_state_with_key(tmp.path()).await;
    let app = crate::router::build_router(state.clone()).layer(axum::Extension(
        axum::extract::connect_info::ConnectInfo(
            "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
        ),
    ));

    // 1. Export with passphrase — via the x-backup-passphrase header (OP-05:
    //    the query-string form is rejected)
    let req = Request::builder()
        .method("GET")
        .uri("/admin/api/backup/export")
        .header("authorization", format!("Bearer {token}"))
        .header("x-backup-passphrase", "secure-passphrase-123")
        .body(Body::empty())
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body_bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let bundle: BackupBundle = serde_json::from_slice(&body_bytes).unwrap();
    assert!(bundle.encrypted);
    assert!(bundle.payload.is_none());
    assert!(bundle.ciphertext.is_some());

    // 2. Validate with wrong passphrase
    let req = Request::builder()
        .method("POST")
        .uri("/admin/api/backup/validate")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "passphrase": "wrong-password",
                "bundle": bundle,
            }))
            .unwrap(),
        ))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // 3. Restore with correct passphrase
    let req = Request::builder()
        .method("POST")
        .uri("/admin/api/backup/restore")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "passphrase": "secure-passphrase-123",
                "bundle": bundle,
            }))
            .unwrap(),
        ))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body_bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let report: RestoreReport = serde_json::from_slice(&body_bytes).unwrap();
    assert!(report.success);
}

#[tokio::test]
async fn test_backup_restore_with_existing_satellite_tables_and_missing_proxy() {
    let tmp = tempdir();
    let (state, token) = make_state_with_key(tmp.path()).await;

    // Seed existing provider, combo, combo_target, and satellite tables (cooldowns, favicons, usage)
    let acc_id = insert_test_account(&state, "existing-prov");
    assert!(acc_id > 0);

    {
        let w = state.db_pool().writer();
        let _ = w.execute(
            "INSERT INTO provider_favicons (provider_id, favicon_bytes) VALUES ('existing-prov', X'00')",
            [],
        );
        let _ = w.execute(
            "INSERT INTO combos (id, name, strategy) VALUES (999, 'existing-combo', 'priority')",
            [],
        );
        let _ = w.execute(
            "INSERT INTO combo_targets (id, combo_id, provider_id, priority_order, active) \
             VALUES (888, 999, 'existing-prov', 1, 1)",
            [],
        );
        let _ = w.execute(
            "INSERT INTO target_cooldowns (combo_target_id, cooldown_until, reason) \
             VALUES (888, datetime('now', '+1 hour'), 'test')",
            [],
        );
        let _ = w.execute(
            "INSERT INTO usage (model, provider, status_code, total_ms) \
             VALUES ('test-m', 'existing-prov', 200, 100)",
            [],
        );
    }

    let app = crate::router::build_router(state.clone()).layer(axum::Extension(
        axum::extract::connect_info::ConnectInfo(
            "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
        ),
    ));

    // Construct a backup bundle with a provider referencing a non-existent current_proxy_id
    let bundle = openproxy_types::backup::BackupBundle {
        version: openproxy_types::backup::BACKUP_FORMAT_VERSION,
        exported_at: chrono::Utc::now().to_rfc3339(),
        openproxy_version: "1.0.0".into(),
        encrypted: false,
        kdf: None,
        kdf_salt: None,
        kdf_iterations: None,
        nonce: None,
        ciphertext: None,
        payload: Some(openproxy_types::backup::BackupPayload {
            providers: vec![openproxy_types::backup::BackupProvider {
                id: openproxy_types::ids::ProviderId::new("restored-prov"),
                name: "Restored Prov".into(),
                base_url: "https://api.restored.com".into(),
                auth_type: openproxy_types::providers::AuthType::Bearer,
                format: openproxy_types::providers::ProviderFormat::Openai,
                extra_headers_json: None,
                auto_activate_keyword: None,
                active: true,
                use_proxies: true,
                current_proxy_id: Some("non-existent-proxy-uuid".into()),
                proxy_rotation_errors: "403,429".into(),
                rate_limit_scope: openproxy_types::providers::RateLimitScope::Account,
                notif_keyword_only: false,
                proxy_rotation_mode: "global".into(),
                direct_first: false,
            }],
            accounts: vec![],
            models: vec![],
            combos: vec![],
            combo_targets: vec![],
            proxy_sources: vec![],
            api_keys: vec![],
            app_config: vec![],
        }),
    };

    let req = Request::builder()
        .method("POST")
        .uri("/admin/api/backup/restore")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&bundle).unwrap()))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body_bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let report: RestoreReport = serde_json::from_slice(&body_bytes).unwrap();
    assert!(report.success);
    assert_eq!(report.providers_restored, 1);

    // Verify restored provider is present and current_proxy_id safely sanitized to NULL
    let (prov_count, proxy_id): (i64, Option<String>) = state.db_pool().with_conn(|c| {
        c.query_row(
            "SELECT COUNT(*), current_proxy_id FROM providers WHERE id = 'restored-prov'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    });
    assert_eq!(prov_count, 1);
    assert!(proxy_id.is_none());
}

#[tokio::test]
async fn test_backup_endpoints_reject_read_only_scope() {
    let tmp = tempdir();
    let (state, _manage_token) = make_state_with_key(tmp.path()).await;

    // Insert an API key with ONLY "read" scope (not "manage")
    let read_only_token = format!("sk-read-{}", "y".repeat(40));
    {
        let w = state.db_pool().writer();
        let key_hash = core_api_keys::hash_key(&read_only_token);
        w.execute(
            "INSERT INTO api_keys (key_hash, key_prefix, label, scopes_json, is_active, created_at) \
             VALUES (?1, 'sk-read-yyyy', 'Read Only Key', '[\"read\"]', 1, datetime('now'))",
            rusqlite::params![key_hash],
        )
        .unwrap();
    }

    let app = crate::router::build_router(state.clone()).layer(axum::Extension(
        axum::extract::connect_info::ConnectInfo(
            "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
        ),
    ));

    // 1. Calling export with read-only key must be rejected (401)
    let req = Request::builder()
        .method("GET")
        .uri("/admin/api/backup/export")
        .header("authorization", format!("Bearer {read_only_token}"))
        .body(Body::empty())
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // 2. Calling restore with read-only key must also be rejected (401)
    let req = Request::builder()
        .method("POST")
        .uri("/admin/api/backup/restore")
        .header("authorization", format!("Bearer {read_only_token}"))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"bundle":{"version":1,"encrypted":false}}"#))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
