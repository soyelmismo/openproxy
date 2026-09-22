//! Contract verification and wire protocol test suite for CodeBuddy OAuth provider.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;
use bytes::Bytes;
use tokio::net::TcpListener;

use openproxy_adapters::upstream::UpstreamClient;
use openproxy_core::oauth::codebuddy::{
    CODEBUDDY_REFRESH_PATH, CODEBUDDY_STATE_PATH, CODEBUDDY_TOKEN_PATH,
    CodeBuddyOAuthProvider, LOGIN_TOKEN_PENDING_CODE,
};
use openproxy_core::oauth::{
    decrypt_access_token, decrypt_refresh_token, store_oauth_tokens, DbRef, OAuthFlow,
    OAuthProvider, OAuthRefreshParams, StoreOAuthTokensParams, TokenRefreshCoordinator,
};
use openproxy_db::secrets::MasterKey;

#[derive(Default)]
struct CodeBuddyMockState {
    state_requests: AtomicUsize,
    token_poll_count: AtomicUsize,
    refresh_requests: AtomicUsize,
    simulate_state_error: std::sync::atomic::AtomicBool,
    simulate_refresh_error: std::sync::atomic::AtomicBool,
}

#[derive(serde::Deserialize)]
struct StateQuery {
    platform: Option<String>,
}

#[derive(serde::Deserialize)]
struct PollQuery {
    state: Option<String>,
}

async fn mock_codebuddy_state_handler(
    State(state): State<Arc<CodeBuddyMockState>>,
    Query(q): Query<StateQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    state.state_requests.fetch_add(1, Ordering::SeqCst);

    if state.simulate_state_error.load(Ordering::SeqCst) {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "code": 10001,
                "msg": "invalid platform"
            })),
        );
    }

    assert_eq!(q.platform.as_deref(), Some("CLI"));
    assert_eq!(
        headers.get("content-type").and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
    assert_eq!(
        headers.get("accept").and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
    assert_eq!(
        headers.get("x-no-authorization").and_then(|v| v.to_str().ok()),
        Some("true")
    );
    assert_eq!(
        headers.get("x-ide-type").and_then(|v| v.to_str().ok()),
        Some("CLI")
    );

    let ua = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .expect("User-Agent header required");
    assert!(ua.contains("CodeBuddy"), "UA must contain CodeBuddy: {ua}");

    let parsed: serde_json::Value = serde_json::from_slice(&body).expect("valid json body");
    assert!(parsed.is_object());

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "code": 0,
            "data": {
                "state": "cb-device-state-uuid-777",
                "authUrl": "https://www.codebuddy.ai/auth?state=cb-device-state-uuid-777"
            }
        })),
    )
}

async fn mock_codebuddy_token_handler(
    State(state): State<Arc<CodeBuddyMockState>>,
    Query(q): Query<PollQuery>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let count = state.token_poll_count.fetch_add(1, Ordering::SeqCst);

    assert_eq!(q.state.as_deref(), Some("cb-device-state-uuid-777"));
    assert_eq!(
        headers.get("accept").and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
    assert_eq!(
        headers.get("x-no-authorization").and_then(|v| v.to_str().ok()),
        Some("true")
    );

    if count == 0 {
        // First poll: pending
        (
            StatusCode::OK,
            axum::Json(serde_json::json!({
                "code": LOGIN_TOKEN_PENDING_CODE,
                "msg": "login ing..."
            })),
        )
    } else {
        // Subsequent poll: success
        (
            StatusCode::OK,
            axum::Json(serde_json::json!({
                "code": 0,
                "data": {
                    "accessToken": "cb_access_token_wire_abc",
                    "refreshToken": "cb_refresh_token_wire_def",
                    "expiresIn": 7200,
                    "refreshExpiresIn": 2592000,
                    "tokenType": "Bearer"
                }
            })),
        )
    }
}

async fn mock_codebuddy_refresh_handler(
    State(state): State<Arc<CodeBuddyMockState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    state.refresh_requests.fetch_add(1, Ordering::SeqCst);

    if state.simulate_refresh_error.load(Ordering::SeqCst) {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "code": 11002,
                "msg": "refresh token invalid or expired"
            })),
        );
    }

    assert_eq!(
        headers.get("content-type").and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
    assert_eq!(
        headers.get("x-refresh-token").and_then(|v| v.to_str().ok()),
        Some("cb_refresh_token_wire_def")
    );
    assert_eq!(
        headers.get("x-auth-refresh-source").and_then(|v| v.to_str().ok()),
        Some("plugin")
    );

    let parsed: serde_json::Value = serde_json::from_slice(&body).expect("valid json body");
    assert!(parsed.is_object());

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "code": 0,
            "data": {
                "accessToken": "cb_access_token_refreshed_999",
                "refreshToken": "cb_refresh_token_new_888",
                "expiresIn": 7200,
                "tokenType": "Bearer"
            }
        })),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn test_codebuddy_oauth_full_device_code_wire_mock() {
    let mock_state = Arc::new(CodeBuddyMockState::default());

    let state_path = CODEBUDDY_STATE_PATH.split('?').next().unwrap();
    let app = Router::new()
        .route(state_path, post(mock_codebuddy_state_handler))
        .route(CODEBUDDY_TOKEN_PATH, get(mock_codebuddy_token_handler))
        .route(CODEBUDDY_REFRESH_PATH, post(mock_codebuddy_refresh_handler))
        .with_state(Arc::clone(&mock_state));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let mock_base = format!("http://{addr}");
    let provider = CodeBuddyOAuthProvider::with_base_url(&mock_base);
    let client = Arc::new(UpstreamClient::new());

    // 1. Contract & Spec verification
    assert_eq!(provider.name(), "codebuddy");
    assert_eq!(provider.flow(), OAuthFlow::DeviceCode);
    assert!(provider.aliases().contains(&"codebuddy-code"));
    assert!(provider.aliases().contains(&"@tencent-ai/codebuddy-code"));

    // 2. Unsupported flows return validation error
    assert!(provider.build_auth_url("http://example.com").await.is_err());
    assert!(
        provider
            .exchange_code("code", "verif", &client, "http://example.com")
            .await
            .is_err()
    );

    // 3. Request device code
    let dar = provider
        .request_device_code(&client)
        .await
        .expect("request_device_code succeeds");
    assert_eq!(dar.device_code, "cb-device-state-uuid-777");
    assert_eq!(dar.user_code, "cb-device-state-uuid-777");
    assert_eq!(
        dar.verification_uri,
        "https://www.codebuddy.ai/auth?state=cb-device-state-uuid-777"
    );
    assert_eq!(
        dar.verification_uri_complete.as_deref(),
        Some("https://www.codebuddy.ai/auth?state=cb-device-state-uuid-777")
    );
    assert_eq!(dar.expires_in, Some(300));
    assert_eq!(dar.interval, Some(2));
    assert_eq!(mock_state.state_requests.load(Ordering::SeqCst), 1);

    // 4. Poll 1: pending (code 11217)
    let poll_pending = provider
        .poll_device_token(&dar.device_code, &client)
        .await
        .expect("poll succeeds");
    assert!(poll_pending.is_none(), "first poll must be pending (None)");

    // 5. Poll 2: success (code 0)
    let poll_success = provider
        .poll_device_token(&dar.device_code, &client)
        .await
        .expect("second poll succeeds")
        .expect("token present");
    assert_eq!(poll_success.access_token, "cb_access_token_wire_abc");
    assert_eq!(
        poll_success.refresh_token.as_deref(),
        Some("cb_refresh_token_wire_def")
    );
    assert_eq!(poll_success.expires_in, Some(7200));
    assert_eq!(poll_success.token_type, "Bearer");

    // 6. DB Storage & AES-256-GCM Encryption verification
    let pool =
        openproxy_db::conn::DbPool::test_pool_with_prefix("openproxy-codebuddy-test").unwrap();
    let master_key = MasterKey::generate().unwrap();
    let (account_id, decrypted_at, decrypted_rt) = {
        let conn = pool.writer();
        // Seed dummy provider row for foreign key
        conn.execute(
            "INSERT INTO providers (id, name, base_url, auth_type, format) VALUES ('codebuddy', 'CodeBuddy', 'https://www.codebuddy.ai/v2', 'oauth', 'openai')",
            [],
        )
        .unwrap();

        let provider_id = openproxy_types::ProviderId::new("codebuddy");
        let account_id = openproxy_core::accounts::create(
            &conn,
            &provider_id,
            None,
            &master_key,
            None,
            10,
            None,
        )
        .expect("create account succeeds");

        let meta = provider.provider_specific_from_token(&poll_success);
        store_oauth_tokens(
            &conn,
            account_id,
            &master_key,
            StoreOAuthTokensParams {
                access_token: &poll_success.access_token,
                refresh_token: poll_success.refresh_token.as_deref(),
                token_type: &poll_success.token_type,
                expires_at: Some("2026-09-22T20:00:00Z"),
                scope: None,
                provider_specific: meta.as_deref(),
                email: provider.email_from_token(&poll_success).as_deref(),
            },
        )
        .expect("store_oauth_tokens succeeds");

        let decrypted_at = decrypt_access_token(&conn, account_id, &master_key).unwrap();
        let decrypted_rt = decrypt_refresh_token(&conn, account_id, &master_key).unwrap();
        (account_id, decrypted_at, decrypted_rt)
    };

    assert_eq!(decrypted_at, "cb_access_token_wire_abc");
    assert_eq!(
        decrypted_rt.as_deref(),
        Some("cb_refresh_token_wire_def")
    );

    // 7. TokenRefreshCoordinator refresh
    let refreshed = TokenRefreshCoordinator::global()
        .refresh_and_store(OAuthRefreshParams {
            provider_id: "codebuddy",
            provider: openproxy_core::oauth::OAuthProviderEnum::CodeBuddy(provider.clone()),
            refresh_token: decrypted_rt.as_deref().unwrap(),
            upstream_client: &client,
            account_id,
            db: DbRef::Pool(&pool),
            master_key: &master_key,
        })
        .await
        .expect("refresh_and_store succeeds");

    assert_eq!(refreshed.access_token, "cb_access_token_refreshed_999");
    assert_eq!(
        refreshed.refresh_token.as_deref(),
        Some("cb_refresh_token_new_888")
    );

    // Verify DB updated with refreshed credentials
    let (new_at, new_rt) = {
        let conn2 = pool.writer();
        let new_at = decrypt_access_token(&conn2, account_id, &master_key).unwrap();
        let new_rt = decrypt_refresh_token(&conn2, account_id, &master_key).unwrap();
        (new_at, new_rt)
    };
    assert_eq!(new_at, "cb_access_token_refreshed_999");
    assert_eq!(new_rt.as_deref(), Some("cb_refresh_token_new_888"));

    // 8. Refresh failure handling
    mock_state.simulate_refresh_error.store(true, Ordering::SeqCst);
    let err = provider
        .refresh_token(
            "cb_refresh_token_wire_def",
            &client,
            account_id,
            DbRef::Pool(&pool),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("11002") || err.to_string().contains("refresh token"),
        "error must reflect upstream failure: {err}"
    );
}
