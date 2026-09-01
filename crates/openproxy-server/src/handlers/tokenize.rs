//! `POST /v1/tokenize` — proxy the `v1internal:countTokens` upstream call.
//!
//! For now only the `antigravity` provider is wired. Every other
//! provider returns `501 Not Implemented` with a structured error
//! envelope so the client can distinguish "not supported" from a real
//! server failure.
//!
//! The handler is mounted under `/v1` without the chat pipeline's
//! `routing` / `auth` / `rate_limit` middleware stack — auth runs at the
//! global API-key middleware already applied by `AppState::router`.

use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use openproxy_compression::message_content_to_text;
use openproxy_core::routing::{self, RoutingPlan};
use openproxy_db::accounts as db_accounts;
use openproxy_types::{
    CoreError,
    ids::AccountId,
    message::OpenAIRequest,
};
use serde::Serialize;
use std::sync::Arc;

use crate::{error::ApiError, state::AppState};

/// Build the `/v1` sub-router containing only `POST /tokenize`.
pub fn router() -> axum::Router<AppState> {
    axum::Router::new().route("/tokenize", axum::routing::post(tokenize))
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
    Json(req): Json<OpenAIRequest>,
) -> Result<Response, ApiError> {
    if req.model.is_empty() {
        return Err(ApiError(CoreError::Validation(
            "model is required".into(),
        )));
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
    use std::{path::PathBuf, sync::Arc};
    use tower::ServiceExt;

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir();
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = base.join(format!("openproxy-tokenize-test-{pid}-{nanos}"));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    /// Seed a provider + model + healthy account so `routing::resolve`
    /// produces a `Combo` with `account_id = Some(_)`. Encrypts the
    /// OAuth access token with the SAME `MasterKey` the `AppState`
    /// uses — otherwise `decrypt_access_token` fails with "aes-gcm
    /// decrypt failed" and the 501 branch never runs.
    fn seed_openai_model(state: &AppState, model_id: &str, mk: &openproxy_db::MasterKey) {
        let w = state.db_pool().writer();
        let provider = ProviderId::new("openai");
        providers::create(
            &w,
            providers::NewProvider {
                id: &provider,
                name: "openai",
                base_url: "https://api.openai.com/v1",
                auth_type: AuthType::Bearer,
                format: ProviderFormat::Openai,
                extra_headers_json: None,
                auto_activate_keyword: None,
                rate_limit_scope: RateLimitScope::Account,
            },
        )
        .expect("seed provider");
        w.execute(
            "INSERT INTO models(provider_id, model_id, target_format) \
             VALUES (?1, ?2, 'openai')",
            params![provider.as_str(), model_id],
        )
        .expect("seed model");
        let access_token = "ya-test-access-token";
        let blob = mk.encrypt(access_token).expect("encrypt test token");
        w.execute(
            "INSERT INTO accounts(provider_id, api_key_encrypted, access_token_encrypted, \
                auth_type, health_status) \
             VALUES (?1, X'00', ?2, 'oauth', 'healthy')",
            params![provider.as_str(), blob],
        )
        .expect("seed account");
    }

    /// Returns `(AppState, Router-with_tokenize-only, Arc<DbPool>, MasterKey)`.
    async fn make_tokenize_test_app(
    ) -> (AppState, Router, Arc<core_db::DbPool>, openproxy_db::MasterKey) {
        let dir = tempdir();
        let pool = Arc::new(core_db::DbPool::open(&dir.join("tokenize.db")).expect("open"));
        {
            let mut w = pool.writer();
            core_db::migrations::run(&mut w).expect("migrations");
        }
        let mk = openproxy_db::MasterKey::generate();
        let adapters_registry = Arc::new(RwLock::new(Arc::new(
            adapters::builtin_adapters(),
        )));
        let state = AppState::for_test(
            openproxy_core::AppConfig::default(),
            Arc::clone(&pool),
            Arc::new(mk.clone()),
            adapters_registry,
        );
        let app: Router = Router::new().merge(router()).with_state(state.clone());
        (state, app, pool, mk)
    }

    #[tokio::test]
    async fn tokenize_returns_501_for_openai_provider() {
        let (state, app, _pool, mk) = make_tokenize_test_app().await;
        seed_openai_model(&state, "gpt-x", &mk);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tokenize")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"model":"gpt-x","messages":[{"role":"user","content":"hi"}]}"#,
                    ))
                    .expect("build request"),
            )
            .await
            .expect("send");

        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);

        let body_bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .expect("read body");
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).expect("parse json");
        assert_eq!(body["error"]["code"], "not_implemented");
        assert!(
            body["error"]["message"]
                .as_str()
                .is_some_and(|m| m.contains("openai")),
            "error message should mention the provider: {body}"
        );
    }

    #[tokio::test]
    async fn tokenize_returns_400_for_empty_model() {
        // No seed needed — the validator runs before the resolver.
        let (_state, app, _pool, _mk) = make_tokenize_test_app().await;

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tokenize")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"model":"","messages":[{"role":"user","content":"hi"}]}"#,
                    ))
                    .expect("build request"),
            )
            .await
            .expect("send");

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn tokenize_returns_404_for_unknown_model() {
        let (_state, app, _pool, _mk) = make_tokenize_test_app().await;
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tokenize")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"model":"ghost","messages":[{"role":"user","content":"hi"}]}"#,
                    ))
                    .expect("build request"),
            )
            .await
            .expect("send");

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // The two spec-mandated unit tests:
    //   count_tokens_wraps_request_only_no_project
    //   parse_total_tokens_flat
    //   parse_total_tokens_nested
    // are in `crates/openproxy-adapters/src/adapters/antigravity.rs`
    // (next to the implementation they cover). The 4xx propagation
    // test lives there too because it needs the upstream-hyper test
    // harness.

    #[test]
    fn router_builds_without_state() {
        // Structural pin: the route builder compiles without an
        // AppState. The actual path (`/tokenize`) is verified by
        // the integration tests above.
        let _app: axum::Router<AppState> = router();
    }
}