use super::*;
use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::{Next, from_fn_with_state},
    response::Response,
    routing::post,
};
use http_body_util::BodyExt as _;
use openproxy_core::{
    AppConfig,
    admin::{self, CreateCustomModelInput, CreateProviderInput},
    api_keys::{self, CreateApiKeyInput},
};
use openproxy_db::{DbPool, MasterKey};
use openproxy_types::{AddTargetInput, combos::Strategy};
use serde_json::{Value, json};
use tower::ServiceExt as _;

struct Fixture {
    state: AppState,
    key: String,
    combo_id: ComboId,
    model_a: openproxy_types::ModelRowId,
}

async fn fixture(input: CreateApiKeyInput) -> Fixture {
    let state = tokio::task::spawn_blocking(|| {
        let pool = Arc::new(DbPool::test_pool_with_prefix("routing-reuse").unwrap());
        AppState::for_test(
            AppConfig::default(),
            pool,
            Arc::new(MasterKey::generate().unwrap()),
            Arc::new(parking_lot::RwLock::new(Arc::new(vec![]))),
        )
    })
    .await
    .unwrap();
    state.discovery_scheduler().cancel();
    state.supervisor().shutdown();
    state.discovery_scheduler().shutdown_and_wait().await;
    state.supervisor().shutdown_and_wait().await;
    let (key, combo_id, model_a) = state
        .db_pool()
        .spawn_write(move |conn| {
            let combo_id =
                openproxy_db::combos::create_combo(conn, "reuse-combo", Strategy::Priority, 1)?;
            let mut models = Vec::new();
            for provider in ["prov-a", "prov-b"] {
                let provider_id = admin::create_provider(
                    conn,
                    CreateProviderInput {
                        id: provider.into(),
                        name: provider.into(),
                        base_url: "http://127.0.0.1:9999/v1".into(),
                        auth_type: "bearer".into(),
                        format: "openai".into(),
                        extra_headers_json: None,
                        rate_limit_scope: None,
                        stream_mode: None,
                        prompt_cache_ttl: None,
                    },
                )?;
                let model = admin::create_custom_model(
                    conn,
                    CreateCustomModelInput {
                        provider_id: provider.into(),
                        model_id: "reuse-model".into(),
                        display_name: None,
                        target_format: "openai".into(),
                        ttl_seconds: 0,
                        model_type: Some("chat".into()),
                    },
                )?;
                openproxy_db::combos::add_target(
                    conn,
                    AddTargetInput {
                        combo_id,
                        provider_id,
                        model_row_id: Some(model),
                        account_id: None,
                        sub_combo_id: None,
                        priority_order: 0,
                        description: None,
                        selected_accounts: None,
                    },
                )?;
                models.push(model);
            }
            let (_, key) = api_keys::create(conn, input, "routing-test")?;
            Ok((key, combo_id, models[0]))
        })
        .await
        .unwrap();
    Fixture {
        state,
        key,
        combo_id,
        model_a,
    }
}

fn unrestricted() -> CreateApiKeyInput {
    CreateApiKeyInput {
        scopes: vec!["chat".into()],
        ..Default::default()
    }
}

async fn route_summary(req: Request<Body>) -> Json<Value> {
    let route = req
        .extensions()
        .get::<ResolvedRoute>()
        .expect("resolved route");
    let targets = route.targets_override.as_ref().map(|targets| {
        targets
            .iter()
            .map(|target| {
                json!({
                    "provider": target.provider_id.as_str(),
                    "account": target.account_id.map(|id| id.0),
                    "model_row": target.model_row_id.map(|id| id.0),
                })
            })
            .collect::<Vec<_>>()
    });
    Json(json!({"combo": route.combo_id.0, "targets": targets}))
}

fn router(state: &AppState) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(route_summary))
        .layer(from_fn_with_state(state.clone(), routing_middleware))
        .layer(from_fn_with_state(
            state.clone(),
            super::super::auth::auth_middleware,
        ))
}

fn request(key: &str, model: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json");
    for &(name, value) in headers {
        builder = builder.header(name, value);
    }
    builder
        .body(Body::from(
            json!({
                "model": model, "messages": [{"role": "user", "content": "hello"}]
            })
            .to_string(),
        ))
        .unwrap()
}

async fn response(app: Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

// Alter only this fixture's catalog between the real auth and routing middleware.
// A second resolution would now return NotFound. A later request must not reuse it.
async fn deactivate_after_auth(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    assert!(req.extensions().get::<PreResolvedRoutingPlan>().is_some());
    state
        .db_pool()
        .spawn_write(|conn| {
            conn.execute(
                "UPDATE models SET active = 0 WHERE model_id = ?1",
                ["reuse-model"],
            )
            .map_err(openproxy_db::error::map_db_error)?;
            Ok(())
        })
        .await
        .unwrap();
    next.run(req).await
}

#[tokio::test]
async fn real_auth_routing_reuses_resolution_but_not_across_requests() {
    let f = fixture(unrestricted()).await;
    let app = Router::new()
        .route("/v1/chat/completions", post(route_summary))
        .layer(from_fn_with_state(f.state.clone(), routing_middleware))
        .layer(from_fn_with_state(f.state.clone(), deactivate_after_auth))
        .layer(from_fn_with_state(
            f.state.clone(),
            super::super::auth::auth_middleware,
        ));
    let (status, body) = response(app, request(&f.key, "prov-a/reuse-model", &[])).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["targets"][0]["model_row"], f.model_a.0);
    let (status, body) =
        response(router(&f.state), request(&f.key, "prov-a/reuse-model", &[])).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    f.state.shutdown_usage_worker().await.unwrap();
}

#[tokio::test]
async fn provider_account_and_legacy_combo_overrides_bypass_reuse() {
    let f = fixture(unrestricted()).await;
    let app = router(&f.state);
    let (status, body) = response(
        app.clone(),
        request(
            &f.key,
            "reuse-model",
            &[
                ("x-openproxy-provider", "prov-b"),
                ("x-openproxy-account", "777777"),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["targets"][0]["provider"], "prov-b");
    // A nonexistent account retains the established auto-rotation fallback.
    assert!(body["targets"][0]["account"].is_null());
    let (status, body) = response(
        app.clone(),
        request(
            &f.key,
            "reuse-model",
            &[("x-openproxy-provider", "unknown-provider")],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body) = response(
        app,
        request(
            &f.key,
            "prov-a/reuse-model",
            &[("x-openproxy-combo", "reuse-combo")],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["combo"], f.combo_id.0);
    f.state.shutdown_usage_worker().await.unwrap();
}

#[tokio::test]
async fn account_override_is_not_replaced_by_the_auth_snapshot() {
    let f = fixture(unrestricted()).await;
    let account = f.state.db_pool().spawn_write(|conn| {
        conn.execute("INSERT INTO accounts(provider_id, api_key_encrypted, health_status) VALUES (?1, X'00', 'healthy')", ["prov-a"])
            .map_err(openproxy_db::error::map_db_error)?;
        Ok(conn.last_insert_rowid())
    }).await.unwrap().to_string();
    let (status, body) = response(
        router(&f.state),
        request(
            &f.key,
            "prov-a/reuse-model",
            &[("x-openproxy-account", &account)],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["targets"][0]["account"].as_i64().unwrap().to_string(),
        account
    );
    f.state.shutdown_usage_worker().await.unwrap();
}

#[tokio::test]
async fn reused_combo_preserves_blacklist_and_allowlist_checks() {
    for (allowed_models, blacklisted_models, blacklisted_providers, expected, expected_provider) in [
        (
            None,
            None,
            Some(vec!["prov-a".into()]),
            StatusCode::OK,
            "prov-b",
        ),
        (
            None,
            None,
            Some(vec!["*".into()]),
            StatusCode::UNAUTHORIZED,
            "",
        ),
        (
            Some(vec![
                "combo:reuse-combo".into(),
                "prov-b/reuse-model".into(),
            ]),
            None,
            None,
            StatusCode::OK,
            "prov-b",
        ),
        (
            None,
            Some(vec!["reuse-model".into()]),
            None,
            StatusCode::UNAUTHORIZED,
            "",
        ),
        // An explicit allowlist entry overrides a provider blacklist: whitelisting
        // `prov-a/reuse-model` grants it even though `prov-a` is provider-blocked.
        (
            Some(vec![
                "combo:reuse-combo".into(),
                "prov-a/reuse-model".into(),
            ]),
            None,
            Some(vec!["prov-a".into()]),
            StatusCode::OK,
            "prov-a",
        ),
    ] {
        let f = fixture(CreateApiKeyInput {
            allowed_models,
            blacklisted_models,
            blacklisted_providers,
            ..unrestricted()
        })
        .await;
        let (status, body) =
            response(router(&f.state), request(&f.key, "combo:reuse-combo", &[])).await;
        assert_eq!(status, expected, "{body}");
        if status == StatusCode::OK {
            assert_eq!(body["targets"].as_array().unwrap().len(), 1, "{body}");
            assert_eq!(body["targets"][0]["provider"], expected_provider, "{body}");
        }
        f.state.shutdown_usage_worker().await.unwrap();
    }
}

#[tokio::test]
async fn combo_permission_is_enforced_on_reuse_and_legacy_override() {
    let f = fixture(CreateApiKeyInput {
        allowed_combos: Some(vec![SYNTHETIC_COMBO_ID, 999999]),
        ..unrestricted()
    })
    .await;
    for (model, headers) in [
        ("combo:reuse-combo", vec![]),
        (
            "prov-a/reuse-model",
            vec![("x-openproxy-combo", "reuse-combo")],
        ),
    ] {
        let (status, body) = response(router(&f.state), request(&f.key, model, &headers)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    }
    f.state.shutdown_usage_worker().await.unwrap();
}

#[test]
fn identity_covers_every_routing_input() {
    let pre = PreResolvedRoutingPlan {
        model: "model".into(),
        legacy_combo_name: None,
        provider_override: None,
        account_override: None,
        plan: RoutingPlan::NotFound {
            model: "model".into(),
            hint: None,
        },
    };
    assert!(pre.matches_request("model", None, None, None));
    assert!(!pre.matches_request("other", None, None, None));
    assert!(!pre.matches_request("model", Some("combo"), None, None));
    assert!(!pre.matches_request("model", None, Some("provider"), None));
    assert!(!pre.matches_request("model", None, None, Some(AccountId(1))));
}
