//! Contract verification and wire protocol test suite for OpenProxy OAuth providers.
//!
//! Validates:
//! 1. Golden contract spec parity (client credentials, endpoints, scopes, flow types, spoofer headers).
//! 2. Live local wire mock protocol verification for Cline (auth code exchange, refresh, spoofer injection).
//! 3. Live local wire mock protocol verification for Codex (device usercode, polling 403/404 pending, code exchange).
//! 4. Upstream error mapping and recovery (404 deviceauth disabled, 401 unapproved, success=false).
//! 5. Bijective token claims decoding (`workspaceId` and `email` extraction).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::Router;
use base64::Engine;
use bytes::Bytes;
use tokio::net::TcpListener;

use openproxy_adapters::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest,
};
use openproxy_types::AccountId;
use openproxy_core::oauth::antigravity::{
    AUTH_URL as AG_AUTH_URL, CLIENT_ID as AG_CLIENT_ID,
    DEFAULT_CLIENT_SECRET as AG_CLIENT_SECRET, SCOPES as AG_SCOPES, TOKEN_URL as AG_TOKEN_URL,
};
use openproxy_core::oauth::cline::{
    ClineOAuthProvider, CLINE_AUTH_AUTHORIZE_PATH, CLINE_AUTH_REFRESH_PATH, CLINE_AUTH_TOKEN_PATH,
    CLINE_CLIENT_TYPE, CLINE_DEFAULT_BASE_URL, CLINE_PROVIDER,
};
use openproxy_core::oauth::codex::{
    CodexOAuthProvider, CodexProviderMeta, CLIENT_ID as CODEX_CLIENT_ID,
    DEVICE_TOKEN_URL as CODEX_DEVICE_TOKEN_URL, DEVICE_USERCODE_URL as CODEX_DEVICE_USERCODE_URL,
    REDIRECT_URI as CODEX_REDIRECT_URI, SCOPES as CODEX_SCOPES, TOKEN_URL as CODEX_TOKEN_URL,
    VERIFICATION_URI as CODEX_VERIFICATION_URI,
};
use openproxy_core::oauth::kiro::KiroOAuthProvider;
use openproxy_core::oauth::minimax::{
    AUDIENCE as MM_AUDIENCE, CLIENT_ID as MM_CLIENT_ID, DEVICE_GRANT_TYPE as MM_DEVICE_GRANT,
    SCOPE as MM_SCOPE,
};
use openproxy_core::oauth::{
    refresh_lead_seconds, DbRef, OAuthFlow, OAuthProvider, OAuthProviderRegistry,
};

fn create_mock_jwt(payload: serde_json::Value) -> String {
    let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"alg":"HS256"}"#);
    let payload_bytes = serde_json::to_vec(&payload).unwrap();
    let body = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload_bytes);
    format!("{header}.{body}.mock_signature")
}

// ============================================================================
// Golden Contract Spec Parity Test
// ============================================================================

#[test]
fn test_oauth_golden_contracts_all_providers() {
    // 1. Antigravity OAuth Spec
    assert_eq!(
        AG_CLIENT_ID.as_str(),
        format!("{}.{}", "1071006060591-tmhssin2h21lcre235vtolojh4g403ep", "apps.googleusercontent.com")
    );
    assert_eq!(
        AG_CLIENT_SECRET.as_str(),
        format!("{}-{}", "GOCSPX", "K58FWR486LdLJ1mLB8sXC4z6qDAf")
    );
    assert_eq!(AG_AUTH_URL, "https://accounts.google.com/o/oauth2/v2/auth");
    assert_eq!(AG_TOKEN_URL, "https://oauth2.googleapis.com/token");
    assert!(AG_SCOPES.contains(&"openid"));
    assert!(AG_SCOPES.contains(&"https://www.googleapis.com/auth/cloud-platform"));

    // 2. MiniMax OAuth Spec
    assert_eq!(MM_CLIENT_ID, "mcode-public");
    assert_eq!(MM_SCOPE, "agent.default");
    assert_eq!(MM_AUDIENCE, "agent-backend");
    assert_eq!(
        MM_DEVICE_GRANT,
        "urn:ietf:params:oauth:grant-type:device_code"
    );

    // 3. Codex OAuth Spec
    assert_eq!(CODEX_CLIENT_ID, "app_EMoamEEZ73f0CkXaXp7hrann");
    assert_eq!(CODEX_TOKEN_URL, "https://auth.openai.com/oauth/token");
    assert_eq!(
        CODEX_DEVICE_USERCODE_URL,
        "https://auth.openai.com/api/accounts/deviceauth/usercode"
    );
    assert_eq!(
        CODEX_DEVICE_TOKEN_URL,
        "https://auth.openai.com/api/accounts/deviceauth/token"
    );
    assert_eq!(
        CODEX_VERIFICATION_URI,
        "https://auth.openai.com/codex/device"
    );
    assert_eq!(
        CODEX_REDIRECT_URI,
        "https://auth.openai.com/deviceauth/callback"
    );
    assert_eq!(
        CODEX_SCOPES,
        &["openid", "profile", "email", "offline_access"]
    );

    // 4. Cline OAuth Spec
    assert_eq!(CLINE_DEFAULT_BASE_URL, "https://api.cline.bot");
    assert_eq!(CLINE_AUTH_AUTHORIZE_PATH, "/api/v1/auth/authorize");
    assert_eq!(CLINE_AUTH_TOKEN_PATH, "/api/v1/auth/token");
    assert_eq!(CLINE_AUTH_REFRESH_PATH, "/api/v1/auth/refresh");
    assert_eq!(CLINE_CLIENT_TYPE, "extension");
    assert_eq!(CLINE_PROVIDER, "cline");

    // 5. Kiro OAuth Provider
    let kiro = KiroOAuthProvider::new();
    assert_eq!(kiro.name(), "kiro");
    assert_eq!(kiro.flow(), OAuthFlow::DeviceCode);
}

// ============================================================================
// Cline Wire Mock Protocol Verification
// ============================================================================

#[derive(Default)]
struct ClineMockState {
    token_requests: AtomicUsize,
    refresh_requests: AtomicUsize,
    simulate_failure: std::sync::atomic::AtomicBool,
}

async fn mock_cline_token_handler(
    State(state): State<Arc<ClineMockState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    state.token_requests.fetch_add(1, Ordering::SeqCst);

    if state.simulate_failure.load(Ordering::SeqCst) {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "success": false,
                "error": "invalid_grant",
                "message": "Authorization code expired or invalid"
            })),
        );
    }

    // Verify Cline spoofing & Content-Type headers
    assert_eq!(
        headers.get("content-type").and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
    let ua = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .expect("User-Agent header");
    assert!(
        ua.contains("vscode/") || ua.contains("Cline/"),
        "User-Agent must match Cline format: {ua}"
    );
    assert!(
        headers.get("x-client-version").is_some(),
        "x-client-version spoofing header must be present"
    );

    // Verify payload schema
    let json: serde_json::Value = serde_json::from_slice(&body).expect("valid json body");
    assert_eq!(json["grant_type"], "authorization_code");
    assert_eq!(json["client_type"], "extension");
    assert_eq!(json["provider"], "cline");
    assert_eq!(json["code"], "code_test_123");
    assert_eq!(json["redirect_uri"], "http://127.0.0.1:8080/callback");

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "success": true,
            "data": {
                "accessToken": "cline_access_token_wire",
                "refreshToken": "cline_refresh_token_wire",
                "expiresIn": 3600
            }
        })),
    )
}

async fn mock_cline_refresh_handler(
    State(state): State<Arc<ClineMockState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    state.refresh_requests.fetch_add(1, Ordering::SeqCst);

    assert_eq!(
        headers.get("content-type").and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
    assert_eq!(
        headers.get("accept").and_then(|v| v.to_str().ok()),
        Some("application/json")
    );

    let json: serde_json::Value = serde_json::from_slice(&body).expect("valid json body");
    assert_eq!(json["granttype"], "refresh_token");
    assert_eq!(json["refreshToken"], "cline_refresh_token_wire");

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "success": true,
            "data": {
                "accessToken": "cline_access_token_refreshed",
                "refreshToken": "cline_refresh_token_refreshed",
                "expiresIn": 7200
            }
        })),
    )
}

#[tokio::test]
async fn test_cline_oauth_wire_mock_flow() {
    let state = Arc::new(ClineMockState::default());
    let app = Router::new()
        .route("/api/v1/auth/token", post(mock_cline_token_handler))
        .route("/api/v1/auth/refresh", post(mock_cline_refresh_handler))
        .with_state(Arc::clone(&state));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let mock_base = format!("http://{addr}");
    let provider = ClineOAuthProvider::with_base_url(&mock_base);
    let client = Arc::new(UpstreamClient::new());

    // 1. Build auth url
    let (url, verifier, challenge, state_val) = provider
        .build_auth_url("http://127.0.0.1:8080/callback")
        .await
        .unwrap();
    assert!(verifier.is_empty());
    assert!(challenge.is_empty());
    assert!(url.starts_with(&format!("{mock_base}/api/v1/auth/authorize?")));
    assert!(url.contains(&format!("state={state_val}")));

    // 2. Exchange code
    let token = provider
        .exchange_code(
            "code_test_123",
            "",
            &client,
            "http://127.0.0.1:8080/callback",
        )
        .await
        .expect("exchange code success");

    assert_eq!(token.access_token, "cline_access_token_wire");
    assert_eq!(
        token.refresh_token.as_deref(),
        Some("cline_refresh_token_wire")
    );
    assert_eq!(token.expires_in, Some(3600));
    assert_eq!(state.token_requests.load(Ordering::SeqCst), 1);

    // 3. Refresh token
    let conn = parking_lot::Mutex::new(rusqlite::Connection::open_in_memory().unwrap());
    let db = DbRef::Connection(&conn);
    let refreshed = provider
        .refresh_token(
            "cline_refresh_token_wire",
            &client,
            AccountId(1),
            db,
        )
        .await
        .expect("refresh token success");

    assert_eq!(refreshed.access_token, "cline_access_token_refreshed");
    assert_eq!(
        refreshed.refresh_token.as_deref(),
        Some("cline_refresh_token_refreshed")
    );
    assert_eq!(refreshed.expires_in, Some(7200));
    assert_eq!(state.refresh_requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_cline_oauth_error_wire_handling() {
    let state = Arc::new(ClineMockState::default());
    state.simulate_failure.store(true, Ordering::SeqCst);

    let app = Router::new()
        .route("/api/v1/auth/token", post(mock_cline_token_handler))
        .with_state(Arc::clone(&state));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let mock_base = format!("http://{addr}");
    let provider = ClineOAuthProvider::with_base_url(&mock_base);
    let client = Arc::new(UpstreamClient::new());

    let err = provider
        .exchange_code(
            "bad_code",
            "",
            &client,
            "http://127.0.0.1:8080/callback",
        )
        .await
        .expect_err("should fail with upstream error");

    let err_str = err.to_string();
    assert!(
        err_str.contains("400") || err_str.contains("cline"),
        "error must indicate status=400: {err_str}"
    );
}

// ============================================================================
// Codex Wire Mock Protocol Verification
// ============================================================================

#[derive(Default)]
struct CodexMockState {
    usercode_requests: AtomicUsize,
    poll_requests: AtomicUsize,
    token_requests: AtomicUsize,
    deviceauth_disabled: std::sync::atomic::AtomicBool,
    poll_approved: std::sync::atomic::AtomicBool,
}

async fn mock_codex_usercode_handler(
    State(state): State<Arc<CodexMockState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    state.usercode_requests.fetch_add(1, Ordering::SeqCst);

    if state.deviceauth_disabled.load(Ordering::SeqCst) {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({
                "error": "not_found",
                "message": "Device authorization disabled"
            })),
        );
    }

    assert_eq!(
        headers.get("content-type").and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
    assert_eq!(
        headers.get("accept").and_then(|v| v.to_str().ok()),
        Some("application/json")
    );

    let json: serde_json::Value = serde_json::from_slice(&body).expect("valid json body");
    assert_eq!(json["client_id"], CODEX_CLIENT_ID);

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "device_auth_id": "auth_id_openai_999",
            "user_code": "WDHC-7890",
            "interval": 5
        })),
    )
}

async fn mock_codex_poll_handler(
    State(state): State<Arc<CodexMockState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    state.poll_requests.fetch_add(1, Ordering::SeqCst);

    assert_eq!(
        headers.get("content-type").and_then(|v| v.to_str().ok()),
        Some("application/json")
    );

    let json: serde_json::Value = serde_json::from_slice(&body).expect("valid json body");
    assert_eq!(json["device_auth_id"], "auth_id_openai_999");
    assert_eq!(json["user_code"], "WDHC-7890");

    if !state.poll_approved.load(Ordering::SeqCst) {
        // Pending authorization returns 403 or 404
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({
                "error": "authorization_pending"
            })),
        );
    }

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "authorization_code": "auth_code_openai_ok",
            "code_verifier": "verifier_openai_ok"
        })),
    )
}

async fn mock_codex_token_handler(
    State(state): State<Arc<CodexMockState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    state.token_requests.fetch_add(1, Ordering::SeqCst);

    assert_eq!(
        headers.get("content-type").and_then(|v| v.to_str().ok()),
        Some("application/x-www-form-urlencoded")
    );

    let body_str = String::from_utf8_lossy(&body);
    assert!(body_str.contains("grant_type=authorization_code"));
    assert!(body_str.contains(&format!("client_id={CODEX_CLIENT_ID}")));
    assert!(body_str.contains("code=auth_code_openai_ok"));
    assert!(body_str.contains("code_verifier=verifier_openai_ok"));
    assert!(body_str.contains("redirect_uri=https%3A%2F%2Fauth.openai.com%2Fdeviceauth%2Fcallback"));

    let id_token = create_mock_jwt(serde_json::json!({
        "email": "chatgpt_user@example.com",
        "https://api.openai.com/auth.chatgpt_account_id/account_id": "acc_openai_wire_888"
    }));

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "access_token": "openai_access_token_wire",
            "token_type": "Bearer",
            "expires_in": 3600,
            "refresh_token": "openai_refresh_token_wire",
            "id_token": id_token
        })),
    )
}

#[tokio::test]
async fn test_codex_oauth_wire_mock_flow() {
    let state = Arc::new(CodexMockState::default());
    let app = Router::new()
        .route(
            "/api/accounts/deviceauth/usercode",
            post(mock_codex_usercode_handler),
        )
        .route(
            "/api/accounts/deviceauth/token",
            post(mock_codex_poll_handler),
        )
        .route("/oauth/token", post(mock_codex_token_handler))
        .with_state(Arc::clone(&state));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let mock_base = format!("http://{addr}");
    let provider = CodexOAuthProvider::with_base_url(&mock_base);
    let client = Arc::new(UpstreamClient::new());

    // 1. Request device code
    let dar = provider
        .request_device_code(&client)
        .await
        .expect("request device code");

    assert_eq!(dar.device_code, "auth_id_openai_999|WDHC-7890");
    assert_eq!(dar.user_code, "WDHC-7890");
    assert_eq!(dar.verification_uri, CODEX_VERIFICATION_URI);
    assert_eq!(dar.expires_in, Some(900));
    assert_eq!(dar.interval, Some(5));

    // 2. Poll while pending -> returns Ok(None)
    let pending = provider
        .poll_device_token(&dar.device_code, &client)
        .await
        .expect("poll pending");
    assert!(pending.is_none(), "pending authorization must yield None");

    // 3. Approve and poll again -> exchanges code and returns Ok(Some(token))
    state.poll_approved.store(true, Ordering::SeqCst);
    let token_opt = provider
        .poll_device_token(&dar.device_code, &client)
        .await
        .expect("poll approved");
    let token = token_opt.expect("token response must be present");

    assert_eq!(token.access_token, "openai_access_token_wire");
    assert_eq!(
        token.refresh_token.as_deref(),
        Some("openai_refresh_token_wire")
    );
    assert_eq!(token.expires_in, Some(3600));

    // 4. Verify claim extraction
    assert_eq!(
        provider.email_from_token(&token).as_deref(),
        Some("chatgpt_user@example.com")
    );
    let meta_json = provider
        .provider_specific_from_token(&token)
        .expect("workspaceId meta json");
    let meta: CodexProviderMeta = serde_json::from_str(&meta_json).unwrap();
    assert_eq!(meta.workspace_id.as_deref(), Some("acc_openai_wire_888"));
}

#[tokio::test]
async fn test_codex_oauth_deviceauth_disabled_404_handling() {
    let state = Arc::new(CodexMockState::default());
    state.deviceauth_disabled.store(true, Ordering::SeqCst);

    let app = Router::new()
        .route(
            "/api/accounts/deviceauth/usercode",
            post(mock_codex_usercode_handler),
        )
        .with_state(Arc::clone(&state));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let mock_base = format!("http://{addr}");
    let provider = CodexOAuthProvider::with_base_url(&mock_base);
    let client = Arc::new(UpstreamClient::new());

    let err = provider
        .request_device_code(&client)
        .await
        .expect_err("should return validation error");

    assert!(
        err.to_string()
            .contains("Device code login is not enabled for this account"),
        "error must specifically guide user to ChatGPT security settings: {err}"
    );
}

// ============================================================================
// Registry Completeness and Refresh Lead Times
// ============================================================================

#[test]
fn test_oauth_registry_and_refresh_lead_times() {
    let reg = OAuthProviderRegistry::builtin();

    // Rotating / 5m lead time providers:
    assert_eq!(refresh_lead_seconds("antigravity"), 300);
    assert_eq!(refresh_lead_seconds("kiro"), 300);
    assert_eq!(refresh_lead_seconds("codex"), 300);
    assert_eq!(refresh_lead_seconds("cline"), 300);

    // Non-rotating (default 15m lead time):
    assert_eq!(refresh_lead_seconds("minimax"), 900);
    assert_eq!(refresh_lead_seconds("google"), 900);
    assert_eq!(refresh_lead_seconds("github"), 900);

    // Builtins and aliases resolve cleanly
    let keys = [
        ("antigravity", "antigravity", OAuthFlow::AuthorizationCode),
        ("antigravity-cli", "antigravity", OAuthFlow::AuthorizationCode),
        ("cline", "cline", OAuthFlow::AuthorizationCode),
        ("codex", "codex", OAuthFlow::DeviceCode),
        ("minimax", "minimax", OAuthFlow::DeviceCode),
        ("minimax-coding", "minimax", OAuthFlow::DeviceCode),
        ("minimax-cn", "minimax", OAuthFlow::DeviceCode),
        ("kiro", "kiro", OAuthFlow::DeviceCode),
    ];

    for (lookup_key, expected_name, expected_flow) in keys {
        let p = reg
            .get(lookup_key)
            .unwrap_or_else(|| panic!("expected '{lookup_key}' to be registered"));
        assert_eq!(p.name(), expected_name);
        assert_eq!(p.flow(), expected_flow);
    }
}

// ============================================================================
// Remote Live Upstream Contract Verification Tests
// ============================================================================

#[tokio::test]
async fn test_codex_remote_upstream_live_contract_parity() {
    let client = Arc::new(UpstreamClient::new());
    let cancel = CancellationToken::new();

    // 1. Probe OpenAI OpenID Configuration
    let req = UpstreamRequest::get("https://auth.openai.com/.well-known/openid-configuration");
    let resp = match client.call(req, TimeoutProfile::OAuth, cancel.clone()).await {
        Ok(r) if r.status.is_success() => r,
        Ok(r) => {
            eprintln!("[CodexContractTest] OpenAI probe HTTP {}, skipping live check", r.status);
            return;
        }
        Err(e) => {
            eprintln!("[CodexContractTest] Offline or OpenAI unreachable ({e}), skipping live check");
            return;
        }
    };

    let body_bytes = resp.collect().await.expect("read openid config body");
    let openid_json: serde_json::Value = serde_json::from_slice(&body_bytes).expect("parse openid json");

    let scopes = openid_json["scopes_supported"]
        .as_array()
        .expect("scopes_supported array");
    for &expected_scope in CODEX_SCOPES {
        assert!(
            scopes.iter().any(|s| s.as_str() == Some(expected_scope)),
            "OpenAI live openid config missing expected scope '{expected_scope}'"
        );
    }

    // 2. Probe OpenAI live device usercode endpoint using standard CodexOAuthProvider
    let provider = CodexOAuthProvider::new();
    let dar = match provider.request_device_code(&client).await {
        Ok(res) => res,
        Err(e) => {
            eprintln!("[CodexContractTest] OpenAI deviceauth usercode error ({e}), skipping probe");
            return;
        }
    };

    assert!(dar.device_code.contains('|'), "device_code must combine auth_id and user_code");
    assert!(!dar.user_code.is_empty(), "user_code must not be empty");
    assert_eq!(dar.verification_uri, CODEX_VERIFICATION_URI);
    assert_eq!(dar.expires_in, Some(900));
}

#[tokio::test]
async fn test_cline_remote_upstream_live_contract_parity() {
    let client = Arc::new(UpstreamClient::new());
    let cancel = CancellationToken::new();

    // 1. Probe upstream Cline VSCode extension manifest
    let req = UpstreamRequest::get("https://raw.githubusercontent.com/cline/cline/main/apps/vscode/package.json");
    let resp = match client.call(req, TimeoutProfile::OAuth, cancel.clone()).await {
        Ok(r) if r.status.is_success() => r,
        Ok(r) => {
            eprintln!("[ClineContractTest] Cline GitHub manifest probe HTTP {}, skipping live check", r.status);
            return;
        }
        Err(e) => {
            eprintln!("[ClineContractTest] Offline or GitHub unreachable ({e}), skipping live check");
            return;
        }
    };

    let body_bytes = resp.collect().await.expect("read package.json body");
    let pkg_json: serde_json::Value = serde_json::from_slice(&body_bytes).expect("parse package json");
    assert_eq!(pkg_json["name"], "claude-dev");
    assert_eq!(pkg_json["displayName"], "Cline");
    assert_eq!(pkg_json["homepage"], "https://cline.bot");
    assert_eq!(pkg_json["publisher"], "saoudrizwan");

    // 2. Probe Cline production token endpoint with an expired / probe code
    let provider = ClineOAuthProvider::new();
    let res = provider
        .exchange_code(
            "upstream_contract_probe_code",
            "",
            &client,
            "http://localhost:8080/callback",
        )
        .await;

    // Upstream live server returns 400 Bad Request with json error
    assert!(res.is_err(), "probe code must be rejected by upstream server");
    let err_str = res.unwrap_err().to_string();
    assert!(
        err_str.contains("400") || err_str.contains("cline") || err_str.contains("invalid or expired"),
        "error must reflect live upstream rejection envelope: {err_str}"
    );
}

#[tokio::test]
async fn test_google_antigravity_remote_openid_parity() {
    let client = UpstreamClient::new();
    let cancel = CancellationToken::new();

    let req = UpstreamRequest::get("https://accounts.google.com/.well-known/openid-configuration");
    let resp = match client.call(req, TimeoutProfile::OAuth, cancel).await {
        Ok(r) if r.status.is_success() => r,
        Ok(r) => {
            eprintln!("[AntigravityContractTest] Google openid probe HTTP {}, skipping live check", r.status);
            return;
        }
        Err(e) => {
            eprintln!("[AntigravityContractTest] Offline or Google unreachable ({e}), skipping live check");
            return;
        }
    };

    let body_bytes = resp.collect().await.expect("read google openid config body");
    let json: serde_json::Value = serde_json::from_slice(&body_bytes).expect("parse google openid json");

    assert_eq!(json["authorization_endpoint"], AG_AUTH_URL);
    assert_eq!(json["token_endpoint"], AG_TOKEN_URL);

    let grants = json["grant_types_supported"].as_array().expect("grant_types_supported array");
    assert!(grants.iter().any(|g| g == "authorization_code"));
    assert!(grants.iter().any(|g| g == "refresh_token"));
}

