//! `POST /v1/tokenize` — proxy the `v1internal:countTokens` upstream call.
//!
//! For now only the `antigravity` provider is wired. Every other
//! provider returns `501 Not Implemented` with a structured error
//! envelope so the client can distinguish "not supported" from a real
//! server failure.
//!
//! The handler is mounted under `/v1` and applies `auth_middleware`
//! locally so unauthenticated clients cannot consume upstream
//! antigravity quota via `v1internal:countTokens`. Routing/rate-limit
//! stay at the chat pipeline's middleware stack.

use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use openproxy_compression::message_content_to_text;
use openproxy_core::routing::{self, RoutingPlan};
use openproxy_db::accounts as db_accounts;
use openproxy_types::CoreError;
use openproxy_types::ids::AccountId;
use serde::Serialize;
use std::sync::Arc;

use crate::{error::ApiError, middleware::auth::ParsedChatRequest, state::AppState};

/// Build the `/v1` sub-router containing only `POST /tokenize`.
///
/// Applies `auth_middleware` via `route_layer` so any client without
/// a valid `Authorization` header is rejected with 401 before reaching
/// the handler — otherwise unauthenticated requests could consume
/// antigravity upstream quota via `v1internal:countTokens` and observe
/// per-account latency.
pub fn router(state: &crate::state::AppState) -> axum::Router<crate::state::AppState> {
    use axum::middleware;
    axum::Router::new().route(
        "/tokenize",
        axum::routing::post(tokenize).route_layer(middleware::from_fn_with_state(
            state.clone(),
            crate::middleware::auth::auth_middleware,
        )),
    )
}

#[derive(Serialize)]
struct TokenizeResponse {
    model: String,
    prompt_tokens: i64,
    total_tokens: i64,
}

#[derive(Serialize)]
struct ErrorEnvelope<'a> {
    error: ErrorBody<'a>,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    code: &'a str,
    message: String,
}

/// Handle `POST /v1/tokenize`.
///
/// Flow (mirrors `crates/openproxy-server/src/middleware/routing.rs:67-90`):
/// 1. Validate the inbound body.
/// 2. Resolve the model via `routing::resolve` + expand the account
///    rotation via `routing::expand_account_rotation`. Both run inside
///    a single `spawn_blocking` together with the access-token decrypt
///    (I/O SQLite + AES-GCM are synchronous; AGENTS §4.3 forbids
///    holding locks across `.await`).
/// 3. If the resolved provider is not `antigravity`, return 501.
/// 4. Otherwise call `antigravity::count_tokens` and return the count.
pub async fn tokenize(
    State(s): State<AppState>,
    axum::Extension(parsed_req): axum::Extension<ParsedChatRequest>,
) -> Result<Response, ApiError> {
    let req = parsed_req.parsed.as_ref().clone();
    if req.model.is_empty() {
        return Err(ApiError(CoreError::Validation("model is required".into())));
    }

    // 1. Resolve routing + expand rotation + decrypt access_token, all
    //    inside a single spawn_blocking. The DB reader guard is
    //    released before the future resolves (no `.await` while the
    //    guard is live).
    let (provider_id, _account_id, model_id, access_token) = {
        let db_pool = Arc::clone(s.db_pool());
        let master_key = Arc::clone(s.master_key());
        let model = req.model.clone();
        tokio::task::spawn_blocking(move || -> Result<_, ApiError> {
            let r = db_pool.reader();
            let plan = routing::resolve(&r, &model)?;
            let RoutingPlan::Combo { targets, .. } = plan else {
                return Err(ApiError(CoreError::model_not_found(
                    "<unknown>",
                    model.clone(),
                )));
            };
            let expanded = routing::expand_account_rotation(&r, targets)?;
            let target = expanded
                .into_iter()
                .next()
                .ok_or_else(|| ApiError(CoreError::NoHealthyTargets(0)))?;
            let account_id: AccountId = target
                .account_id
                .ok_or_else(|| ApiError(CoreError::NoHealthyTargets(0)))?;
            let access_token =
                db_accounts::decrypt_access_token(&r, account_id, master_key.as_ref())?;
            Ok::<_, ApiError>((target.provider_id, account_id, model, access_token))
        })
        .await
        .map_err(|e| ApiError(CoreError::Internal(format!("join error: {e}"))))?
    }?;

    // 2. Provider branch: only antigravity is wired for now.
    if provider_id.as_str() != "antigravity" {
        let body = ErrorEnvelope {
            error: ErrorBody {
                code: "not_implemented",
                message: format!(
                    "tokenize not supported for provider '{}'; only antigravity is wired",
                    provider_id.as_str()
                ),
            },
        };
        return Ok((StatusCode::NOT_IMPLEMENTED, Json(body)).into_response());
    }

    // 3. Build the inner `request` body for `:countTokens`. The
    //    upstream only accepts `{"contents": [...]}` — tools and
    //    tool_choice are intentionally NOT forwarded (the upstream
    //    rejects them today; see spec GAP-3 §3.5).
    let inner_body = serde_json::json!({
        "contents": req
            .messages
            .iter()
            .map(|m| serde_json::json!({
                "role": m.role,
                "parts": [{"text": message_content_to_text(m)}],
            }))
            .collect::<Vec<_>>(),
    });

    // 4. Call upstream.
    let total = openproxy_adapters::adapters::antigravity::count_tokens(
        s.upstream_client(),
        &access_token,
        &inner_body,
    )
    .await
    .map_err(|e| {
        ApiError(CoreError::UpstreamConnection(format!(
            "antigravity count_tokens: {e}"
        )))
    })?;

    Ok(Json(TokenizeResponse {
        model: model_id,
        prompt_tokens: total,
        total_tokens: total,
    })
    .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;
    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode},
    };
    use openproxy_adapters::adapters;
    use openproxy_core::providers::{self, AuthType, ProviderFormat, RateLimitScope};
    use openproxy_db as core_db;
    use openproxy_types::ids::ProviderId;
    use parking_lot::RwLock;
    use rusqlite::params;
    use std::sync::Arc;
    use tower::ServiceExt;

    async fn make_test_app() -> (
        AppState,
        Router,
        Arc<core_db::DbPool>,
        openproxy_db::MasterKey,
        String,
    ) {
        let dir = openproxy_db::testing::TempDir::new("tokenize-test").expect("mkdir");
        let pool = Arc::new(core_db::DbPool::open(&dir.join("tokenize.db")).expect("open"));
        {
            let mut w = pool.writer();
            core_db::migrations::run(&mut w).expect("migrations");
        }
        let plaintext = format!("sk-tok-{}", std::process::id());
        {
            let w = pool.writer();
            let key_hash = openproxy_core::api_keys::hash_key(&plaintext);
            w.execute(
                "INSERT OR REPLACE INTO api_keys (key_hash, key_prefix, label, scopes_json, created_by) \
                 VALUES (?1, ?2, 'test', '[\"chat\"]', 'tokenize-test')",
                params![key_hash, &plaintext[..plaintext.len().min(12)]],
            ).expect("insert key");
        }
        let mk = openproxy_db::MasterKey::generate().unwrap();
        let adapters_registry = Arc::new(RwLock::new(Arc::new(adapters::builtin_adapters())));
        let state = AppState::for_test(
            openproxy_core::AppConfig::default(),
            Arc::clone(&pool),
            Arc::new(mk.clone()),
            adapters_registry,
        );
        let app = Router::new()
            .merge(router(&state))
            .with_state(state.clone());
        (state, app, pool, mk, plaintext)
    }

    fn seed_model(
        state: &AppState,
        prov: &str,
        model: &str,
        fmt: ProviderFormat,
        auth: AuthType,
        mk: Option<&openproxy_db::MasterKey>,
    ) {
        let w = state.db_pool().writer();
        let pid = ProviderId::new(prov);
        let _ = providers::create(
            &w,
            providers::NewProvider {
                id: &pid,
                name: prov,
                base_url: "https://api.test.com",
                auth_type: auth,
                format: fmt,
                extra_headers_json: None,
                auto_activate_keyword: None,
                rate_limit_scope: RateLimitScope::Account,
            },
        );
        w.execute("INSERT OR REPLACE INTO models(provider_id, model_id, target_format) VALUES (?1, ?2, 'openai')", params![pid.as_str(), model]).expect("model");
        if let Some(key) = mk {
            let blob = key.encrypt("ya-test-access-token").expect("encrypt");
            w.execute("INSERT INTO accounts(provider_id, api_key_encrypted, access_token_encrypted, auth_type, health_status) VALUES (?1, X'00', ?2, 'oauth', 'healthy')", params![pid.as_str(), blob]).expect("account");
        }
    }

    async fn post(
        app: &Router,
        uri: &str,
        auth: Option<&str>,
        body: &str,
    ) -> axum::response::Response {
        let mut req = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(tok) = auth {
            req = req.header("authorization", format!("Bearer {tok}"));
        }
        app.clone()
            .oneshot(req.body(Body::from(body.to_string())).expect("build"))
            .await
            .expect("send")
    }

    #[tokio::test]
    async fn test_tokenize_provider_and_auth() {
        let (state, app, _pool, mk, key) = make_test_app().await;
        seed_model(
            &state,
            "openai",
            "gpt-x",
            ProviderFormat::Openai,
            AuthType::Bearer,
            Some(&mk),
        );

        // 501 for openai provider
        let resp = post(
            &app,
            "/tokenize",
            Some(&key),
            r#"{"model":"gpt-x","messages":[{"role":"user","content":"hi"}]}"#,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
        let b = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let val: serde_json::Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(val["error"]["code"], "not_implemented");
        assert!(
            val["error"]["message"]
                .as_str()
                .is_some_and(|m| m.contains("openai"))
        );

        // 401 without auth and with bad auth
        let resp = post(
            &app,
            "/tokenize",
            None,
            r#"{"model":"gpt-x","messages":[{"role":"user","content":"hi"}]}"#,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let resp = post(
            &app,
            "/tokenize",
            Some("invalid-key"),
            r#"{"model":"gpt-x","messages":[{"role":"user","content":"hi"}]}"#,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // Sanity check seed antigravity model
        seed_model(
            &state,
            "antigravity",
            "agy-x",
            ProviderFormat::Openai,
            AuthType::OAuth,
            None,
        );
        let cnt: i64 = state.db_pool().with_conn(|c| {
            c.query_row("SELECT COUNT(*) FROM providers", [], |r| r.get(0))
                .unwrap()
        });
        assert!(cnt >= 2);
    }

    #[tokio::test]
    async fn test_tokenize_adversarial_payloads() {
        let (_state, app, _pool, _mk, key) = make_test_app().await;
        let cases = [
            (
                r#"{"messages":[{"role":"user","content":"hi"}]}"#,
                StatusCode::BAD_REQUEST,
            ),
            (
                r#"{"model":"","messages":[{"role":"user","content":"hi"}]}"#,
                StatusCode::BAD_REQUEST,
            ),
            (
                r#"{"model":"ghost","messages":[{"role":"user","content":"hi"}]}"#,
                StatusCode::NOT_FOUND,
            ),
            (
                r#"{"model":"   ","messages":[{"role":"user","content":"hi"}]}"#,
                StatusCode::BAD_REQUEST,
            ),
            (
                r#"{"model":"nonexistent","messages":[]}"#,
                StatusCode::NOT_FOUND,
            ),
        ];
        for (body, expected) in cases {
            let resp = post(&app, "/tokenize", Some(&key), body).await;
            assert!(
                resp.status() == expected
                    || (expected == StatusCode::BAD_REQUEST
                        && resp.status() == StatusCode::NOT_FOUND)
            );
        }

        // Extremely long model name
        let long_body = format!(
            r#"{{"model":"{}","messages":[{{"role":"user","content":"hi"}}]}}"#,
            "x".repeat(10000)
        );
        assert_eq!(
            post(&app, "/tokenize", Some(&key), &long_body)
                .await
                .status(),
            StatusCode::NOT_FOUND
        );

        // Malformed or non-object bodies
        for body in [
            "{ not valid json",
            r#"{"model":"x","messages":null}"#,
            "",
            "[]",
        ] {
            let resp = post(&app, "/tokenize", Some(&key), body).await;
            assert!(resp.status().is_client_error() || resp.status().is_server_error());
        }
    }

    #[tokio::test]
    async fn test_router_builds_with_state() {
        let pool =
            Arc::new(core_db::DbPool::test_pool_with_prefix("tokenize-struct").expect("open"));
        let mk = openproxy_db::MasterKey::generate().unwrap();
        let adapters = Arc::new(RwLock::new(Arc::new(adapters::builtin_adapters())));
        let state = AppState::for_test(
            openproxy_core::AppConfig::default(),
            pool,
            Arc::new(mk),
            adapters,
        );
        let _app: axum::Router<AppState> = router(&state);
    }
}
