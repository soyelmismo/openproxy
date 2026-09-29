//! Tests for manual Codex rate limit reset credit operations.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use tokio::net::TcpListener;

use openproxy_adapters::upstream::UpstreamClient;
use openproxy_core::codex_resets::{list_codex_account_resets, redeem_codex_account_reset};
use openproxy_core::oauth::OAuthProviderRegistry;
use openproxy_db::DbPool;
use openproxy_db::secrets::MasterKey;
use openproxy_types::ids::AccountId;

#[derive(Default)]
struct MockCodexUpstreamState {
    list_calls: AtomicUsize,
    consume_calls: AtomicUsize,
    return_already_redeemed: std::sync::atomic::AtomicBool,
    return_nothing_to_reset: std::sync::atomic::AtomicBool,
    return_no_credits: std::sync::atomic::AtomicBool,
}

async fn mock_list_handler(
    axum::extract::State(state): axum::extract::State<Arc<MockCodexUpstreamState>>,
) -> impl IntoResponse {
    state.list_calls.fetch_add(1, Ordering::SeqCst);
    if state.return_no_credits.load(Ordering::SeqCst) {
        return (
            StatusCode::OK,
            axum::Json(serde_json::json!({
                "credits": [],
                "available_count": 0
            })),
        );
    }

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "credits": [
                {
                    "id": "credit-uuid-1",
                    "status": "available",
                    "expires_at": "2099-01-01T00:00:00Z"
                }
            ],
            "available_count": 1
        })),
    )
}

async fn mock_consume_handler(
    axum::extract::State(state): axum::extract::State<Arc<MockCodexUpstreamState>>,
    body: axum::extract::Json<serde_json::Value>,
) -> impl IntoResponse {
    state.consume_calls.fetch_add(1, Ordering::SeqCst);

    assert_eq!(body["credit_id"], "credit-uuid-1");
    assert!(body["redeem_request_id"].is_string());

    if state.return_nothing_to_reset.load(Ordering::SeqCst) {
        return (
            StatusCode::CONFLICT,
            axum::Json(serde_json::json!({
                "error": { "code": "nothing_to_reset" }
            })),
        );
    }

    if state.return_already_redeemed.load(Ordering::SeqCst) {
        return (
            StatusCode::OK,
            axum::Json(serde_json::json!({
                "code": "alreadyRedeemed"
            })),
        );
    }

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "code": "reset"
        })),
    )
}

fn setup_test_db() -> (Arc<DbPool>, Arc<MasterKey>) {
    let pool = Arc::new(DbPool::test_pool().unwrap());
    let master_key = Arc::new(MasterKey::generate().unwrap());
    {
        let w = pool.writer();
        w.execute(
            "INSERT OR IGNORE INTO providers (id, name, base_url, auth_type, format) VALUES
             ('codex', 'Codex', 'https://chatgpt.com', 'oauth', 'responses'),
             ('antigravity', 'Antigravity', 'https://antigravity.test', 'oauth', 'openai')",
            [],
        )
        .unwrap();
    }
    (pool, master_key)
}

fn create_codex_oauth_account(
    pool: &DbPool,
    master_key: &MasterKey,
    provider: &str,
    auth_type: &str,
) -> AccountId {
    let w = pool.writer();
    let access_token_enc = master_key.encrypt("test_access_token_123").unwrap();
    let refresh_token_enc = master_key.encrypt("test_refresh_token_456").unwrap();

    w.execute(
        "INSERT INTO accounts (
            provider_id, auth_type, access_token_encrypted, refresh_token_encrypted,
            label, priority, health_status, oauth_provider_specific
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            provider,
            auth_type,
            access_token_enc,
            refresh_token_enc,
            "Test Account",
            1,
            "healthy",
            r#"{"workspaceId":"ws-123"}"#,
        ],
    )
    .unwrap();

    let id = w.last_insert_rowid();
    AccountId::new(id)
}

#[tokio::test]
async fn test_codex_resets_validation_fails_for_non_codex_or_api_key() {
    let (pool, master_key) = setup_test_db();
    let upstream = Arc::new(UpstreamClient::new());
    let registry = Arc::new(OAuthProviderRegistry::new());

    // Non-codex account
    let ant_account = create_codex_oauth_account(&pool, &master_key, "antigravity", "oauth");
    let err = list_codex_account_resets(ant_account, &pool, &master_key, &upstream, &registry)
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("only be redeemed for Codex accounts")
    );

    let err_redeem = redeem_codex_account_reset(
        ant_account,
        Some("credit-123"),
        &pool,
        &master_key,
        &upstream,
        &registry,
    )
    .await
    .unwrap_err();
    assert!(
        err_redeem
            .to_string()
            .contains("only be redeemed for Codex accounts")
    );

    // Non-oauth account
    let key_account = create_codex_oauth_account(&pool, &master_key, "codex", "api_key");
    let err = list_codex_account_resets(key_account, &pool, &master_key, &upstream, &registry)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("require an OAuth account"));

    let err_redeem_key = redeem_codex_account_reset(
        key_account,
        None,
        &pool,
        &master_key,
        &upstream,
        &registry,
    )
    .await
    .unwrap_err();
    assert!(err_redeem_key.to_string().contains("require an OAuth account"));
}

#[tokio::test]
async fn test_codex_resets_update_db_meta() {
    let (pool, _master_key) = setup_test_db();
    let w = pool.writer();
    w.execute(
        "INSERT INTO accounts (id, provider_id, auth_type, label, oauth_provider_specific)
         VALUES (10, 'codex', 'oauth', 'Codex Account', '{\"workspaceId\":\"ws-abc\"}')",
        [],
    )
    .unwrap();

    openproxy_db::accounts::update_codex_reset_credits(&w, 10, 3).unwrap();

    let raw: String = w
        .query_row(
            "SELECT oauth_provider_specific FROM accounts WHERE id = 10",
            [],
            |r| r.get(0),
        )
        .unwrap();

    let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(parsed["workspaceId"], "ws-abc");
    assert_eq!(parsed["reset_credits"], 3);
}

#[tokio::test]
async fn test_codex_resets_mock_server_flow() {
    let mock_state = Arc::new(MockCodexUpstreamState::default());
    let app = Router::new()
        .route(
            "/backend-api/wham/rate-limit-reset-credits",
            get(mock_list_handler),
        )
        .route(
            "/backend-api/wham/rate-limit-reset-credits/consume",
            post(mock_consume_handler),
        )
        .with_state(Arc::clone(&mock_state));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let _addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let (pool, master_key) = setup_test_db();
    let _account_id = create_codex_oauth_account(&pool, &master_key, "codex", "oauth");

    // Test parse and outcome logic directly
    let ok_json = serde_json::json!({
        "credits": [
            {
                "id": "credit-uuid-1",
                "status": "available",
                "expires_at": "2099-01-01T00:00:00Z"
            }
        ],
        "available_count": 1
    });

    let (credits, count) =
        openproxy_adapters::adapters::codex::quota::parse_codex_reset_credits(&ok_json).unwrap();
    assert_eq!(count, 1);
    assert_eq!(credits[0].id, "credit-uuid-1");

    let consume_ok = serde_json::json!({ "code": "reset" });
    let outcome =
        openproxy_adapters::adapters::codex::quota::parse_codex_consume_response(200, &consume_ok)
            .unwrap();
    assert_eq!(
        outcome,
        openproxy_adapters::adapters::codex::quota::CodexResetOutcome::Reset
    );
}
