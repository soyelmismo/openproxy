use super::super::target_headers::*;
use super::*;
use crate::error_classification::{UpstreamErrorClass, is_hard_skip_error};
use crate::retry::RetryPolicy;
use openproxy_db::DbPool;
use openproxy_types::CoreError;
use openproxy_types::ids::{AccountId, ModelId};
use std::sync::Arc;

fn fresh_pool(tag: &str) -> DbPool {
    let pool = DbPool::test_pool_with_prefix(&format!("openproxy-pipeline-gap6-{tag}"))
        .expect("open pool");
    {
        let conn = pool.open_connection().expect("open conn");
        conn.execute(
            "INSERT INTO providers(id, name, base_url, auth_type, format) \
             VALUES ('antigravity', 'Antigravity', 'https://x', 'oauth', 'openai')",
            [],
        )
        .expect("seed provider");
        conn.execute(
            "INSERT INTO accounts(provider_id, label) VALUES ('antigravity', 'a1')",
            [],
        )
        .expect("seed account");
    }
    pool
}

#[test]
fn should_mark_live_limited_only_for_429_resource_exhausted() {
    let yes = CoreError::upstream_error(
        429,
        "antigravity",
        "gemini-2.5",
        r#"{"reason":"RESOURCE_EXHAUSTED"}"#,
        false,
    );
    assert!(should_mark_live_limited(&yes));

    let wrong_status = CoreError::upstream_error(
        503,
        "antigravity",
        "gemini-2.5",
        "RESOURCE_EXHAUSTED",
        false,
    );
    assert!(!should_mark_live_limited(&wrong_status));

    let wrong_body = CoreError::upstream_error(
        429,
        "antigravity",
        "gemini-2.5",
        r#"{"reason":"rate_limited"}"#,
        false,
    );
    assert!(!should_mark_live_limited(&wrong_body));

    let not_upstream = CoreError::RateLimited {
        provider: "antigravity".to_string(),
        retry_after_ms: 1000,
        is_proxy_rotated: false,
    };
    assert!(!should_mark_live_limited(&not_upstream));
}

#[tokio::test(flavor = "current_thread")]
async fn mark_live_limited_inner_inserts_a_row() {
    let pool = fresh_pool("insert");
    let conn_arc = pool.writer_arc();
    let aid = AccountId(1);
    let mid = ModelId::new("gemini-2.5");
    let err = CoreError::upstream_error(
        429,
        "antigravity",
        "gemini-2.5",
        r#"{"reason":"RESOURCE_EXHAUSTED"}"#,
        false,
    );

    mark_live_limited_inner(Arc::clone(&conn_arc), aid, &mid, &err);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let guard = pool.writer();
    assert!(
        openproxy_db::live_limited::is_limited(&guard, aid, &mid).expect("is_limited"),
        "row should be present after mark_live_limited_inner"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn mark_live_limited_inner_skips_non_matching_bodies() {
    let pool = fresh_pool("skip");
    let conn_arc = pool.writer_arc();
    let aid = AccountId(1);
    let mid = ModelId::new("gemini-2.5");
    let err = CoreError::upstream_error(500, "antigravity", "gemini-2.5", "boom", false);

    mark_live_limited_inner(Arc::clone(&conn_arc), aid, &mid, &err);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let guard = pool.writer();
    assert!(
        !openproxy_db::live_limited::has_row(&guard, aid, &mid).expect("has_row"),
        "no row should be inserted for non-matching bodies"
    );
}

#[test]
fn hard_skip_403_validation_required_is_not_retryable() {
    let err = CoreError::upstream_error_with_skip(
        403,
        "antigravity",
        "gemini-2.5",
        r#"{"error":"VALIDATION_REQUIRED"}"#,
        false,
        true,
    );
    assert!(err.is_hard_skip());
    assert!(is_hard_skip_error(&err));
    assert!(!RetryPolicy::is_retryable(&err, false));
}

#[test]
fn hard_skip_400_malformed_tool_call_is_not_retryable() {
    let err = CoreError::upstream_error_with_skip(
        400,
        "antigravity",
        "gemini-2.5",
        r#"{"error":{"code":2013,"message":"function name or parameters is empty"}}"#,
        false,
        true,
    );
    assert!(err.is_hard_skip());
    assert!(!RetryPolicy::is_retryable(&err, false));
}

#[test]
fn hard_skip_403_permission_denied_is_not_retryable() {
    let err = CoreError::upstream_error_with_skip(
        403,
        "antigravity",
        "gemini-2.5",
        "API_KEY_INVALID",
        false,
        true,
    );
    assert!(err.is_hard_skip());
    assert!(!RetryPolicy::is_retryable(&err, false));
}

#[test]
fn generic_500_is_still_retryable() {
    let err = CoreError::upstream_error(500, "antigravity", "gemini-2.5", "boom", false);
    assert!(!err.is_hard_skip());
    assert!(RetryPolicy::is_retryable(&err, false));
}

#[test]
fn hard_skip_classification_is_stable() {
    let cases: &[(u16, &str, UpstreamErrorClass)] = &[
        (
            403,
            r#"{"error":"VALIDATION_REQUIRED"}"#,
            UpstreamErrorClass::ValidationRequired,
        ),
        (
            403,
            "PERMISSION_DENIED",
            UpstreamErrorClass::PermissionDenied,
        ),
        (403, "API_KEY_INVALID", UpstreamErrorClass::PermissionDenied),
        (
            429,
            r#"{"reason":"RESOURCE_EXHAUSTED"}"#,
            UpstreamErrorClass::ResourceExhausted,
        ),
        (
            400,
            "function name or parameters is empty",
            UpstreamErrorClass::MalformedToolCall,
        ),
        (
            400,
            "Base64 decoding failed",
            UpstreamErrorClass::InvalidPayload,
        ),
        (500, "upstream down", UpstreamErrorClass::Generic),
        (403, "", UpstreamErrorClass::Generic),
    ];
    for (status, body, expected) in cases {
        let class = crate::error_classification::classify_upstream_error(*status, body);
        assert_eq!(class, *expected, "status={status} body={body:?}");
    }
}

#[test]
fn hard_skip_400_2013_marker_only() {
    let err = CoreError::upstream_error_with_skip(
        400,
        "antigravity",
        "gemini-2.5",
        r#"{"error":{"code":2013,"message":"oops"}}"#,
        false,
        true,
    );
    assert!(err.is_hard_skip());
    assert!(!RetryPolicy::is_retryable(&err, false));
}

#[test]
fn test_propagate_opencode_headers_preserves_valid_session() {
    let mut headers = vec![
        ("User-Agent".into(), "opencode/1.18.31".into()),
        ("x-opencode-client".into(), "cli".into()),
        ("x-opencode-project".into(), "global".into()),
        ("x-opencode-session".into(), "ses_default".into()),
    ];
    let valid_session = "ses_f534dfae8ffeCy4Ee4tLWNygDc";
    let mut req_headers = std::collections::BTreeMap::new();
    req_headers.insert("x-opencode-session".into(), valid_session.into());
    req_headers.insert("x-opencode-client".into(), "desktop".into());
    req_headers.insert("x-opencode-project".into(), "project-42".into());
    let openai_req = openproxy_types::OpenAIRequest::default();

    propagate_opencode_headers(&mut headers, &req_headers, &openai_req);

    let session = headers
        .iter()
        .find(|(k, _)| k == "x-opencode-session")
        .unwrap();
    assert_eq!(session.1, valid_session);
    let client = headers
        .iter()
        .find(|(k, _)| k == "x-opencode-client")
        .unwrap();
    assert_eq!(client.1, "desktop");
    let project = headers
        .iter()
        .find(|(k, _)| k == "x-opencode-project")
        .unwrap();
    assert_eq!(project.1, "project-42");
}

#[test]
fn test_propagate_opencode_headers_translates_candidate_session() {
    let mut headers = vec![
        ("User-Agent".into(), "opencode/1.18.31".into()),
        ("x-opencode-session".into(), "ses_default".into()),
    ];
    let mut req_headers = std::collections::BTreeMap::new();
    req_headers.insert("x-session-id".into(), "cursor:uuid-9876".into());
    req_headers.insert("user-agent".into(), "opencode/1.20.0".into());
    let openai_req = openproxy_types::OpenAIRequest::default();

    propagate_opencode_headers(&mut headers, &req_headers, &openai_req);

    let session = headers
        .iter()
        .find(|(k, _)| k == "x-opencode-session")
        .unwrap();
    assert!(openproxy_adapters::spoofer::is_valid_opencode_session_id(
        &session.1
    ));
    assert_ne!(session.1, "ses_default");

    let ua = headers.iter().find(|(k, _)| k == "User-Agent").unwrap();
    assert_eq!(ua.1, "opencode/1.20.0");
}

#[test]
fn test_propagate_opencode_headers_from_body_user_field() {
    let mut headers = vec![
        ("User-Agent".into(), "opencode/1.18.31".into()),
        ("x-opencode-session".into(), "ses_default".into()),
    ];
    let req_headers = std::collections::BTreeMap::new();
    let openai_req = openproxy_types::OpenAIRequest {
        user: Some("chat-session-user-123".into()),
        ..Default::default()
    };

    propagate_opencode_headers(&mut headers, &req_headers, &openai_req);

    let session = headers
        .iter()
        .find(|(k, _)| k == "x-opencode-session")
        .unwrap();
    assert!(openproxy_adapters::spoofer::is_valid_opencode_session_id(
        &session.1
    ));
    assert_ne!(session.1, "ses_default");
    assert_eq!(
        session.1,
        openproxy_adapters::spoofer::translate_session_id("chat-session-user-123", None)
    );
}

use crate::oauth::{PipelineOAuthRegistry, TokenResponse};
use openproxy_adapters::upstream::UpstreamClient;
use openproxy_db::secrets::MasterKey;
use std::sync::atomic::{AtomicUsize, Ordering};

struct MockOAuthRegistry {
    pool_call_count: Arc<AtomicUsize>,
    shared_call_count: Arc<AtomicUsize>,
    refreshed_token: String,
}

impl PipelineOAuthRegistry for MockOAuthRegistry {
    fn refresh_and_store<'a>(
        &'a self,
        _provider_id: &'a str,
        _refresh_token: &'a str,
        _upstream_client: &'a Arc<UpstreamClient>,
        _account_id: AccountId,
        _db_pool: Option<&'a openproxy_db::DbPool>,
        _master_key: &'a MasterKey,
    ) -> futures_util::future::BoxFuture<'a, Result<TokenResponse, CoreError>> {
        self.pool_call_count.fetch_add(1, Ordering::SeqCst);
        let token = TokenResponse {
            access_token: self.refreshed_token.clone(),
            token_type: "Bearer".to_string(),
            expires_in: Some(3600),
            refresh_token: None,
            scope: None,
            id_token: None,
        };
        Box::pin(async move { Ok(token) })
    }

    fn refresh_and_store_shared<'a>(
        &'a self,
        _provider_id: &'a str,
        _refresh_token: &'a str,
        _upstream_client: &'a Arc<UpstreamClient>,
        _account_id: AccountId,
        _conn: &'a Arc<parking_lot::Mutex<rusqlite::Connection>>,
        _master_key: &'a MasterKey,
    ) -> futures_util::future::BoxFuture<'a, Result<TokenResponse, CoreError>> {
        self.shared_call_count.fetch_add(1, Ordering::SeqCst);
        let token = TokenResponse {
            access_token: self.refreshed_token.clone(),
            token_type: "Bearer".to_string(),
            expires_in: Some(3600),
            refresh_token: None,
            scope: None,
            id_token: None,
        };
        Box::pin(async move { Ok(token) })
    }
}

struct DefaultOnlyMockOAuthRegistry;

impl PipelineOAuthRegistry for DefaultOnlyMockOAuthRegistry {
    fn refresh_and_store<'a>(
        &'a self,
        _provider_id: &'a str,
        _refresh_token: &'a str,
        _upstream_client: &'a Arc<UpstreamClient>,
        _account_id: AccountId,
        _db_pool: Option<&'a openproxy_db::DbPool>,
        _master_key: &'a MasterKey,
    ) -> futures_util::future::BoxFuture<'a, Result<TokenResponse, CoreError>> {
        Box::pin(async move {
            Ok(TokenResponse {
                access_token: "pool_only_token".to_string(),
                token_type: "Bearer".to_string(),
                expires_in: Some(3600),
                refresh_token: None,
                scope: None,
                id_token: None,
            })
        })
    }
}

fn make_test_target(
    acc_id: Option<AccountId>,
    access_token: &str,
    refresh: Option<&str>,
) -> openproxy_types::context::ResolvedTarget {
    openproxy_types::context::ResolvedTarget {
        target: openproxy_types::combos::ComboTarget {
            id: openproxy_types::ids::ComboTargetId(1),
            combo_id: openproxy_types::ids::ComboId(1),
            provider_id: openproxy_types::ids::ProviderId::new("antigravity"),
            account_id: acc_id,
            model_row_id: None,
            sub_combo_id: None,
            priority_order: 1,
            weight: 1,
            active: true,
            rate_limit_scope: openproxy_types::providers::RateLimitScope::Account,
            cooldown_mode: None,
            cooldown_base_secs: None,
            cooldown_max_secs: None,
            cooldown_factor: None,
            thinking_effort: None,
            ..Default::default()
        },
        model: openproxy_types::models::Model {
            row_id: openproxy_types::ids::ModelRowId(1),
            provider_id: openproxy_types::ids::ProviderId::new("antigravity"),
            model_id: "gemini-2.5".into(),
            ..Default::default()
        },
        api_key: "k1".into(),
        api_key_label: None,
        custom_meta: Some(openproxy_types::context::CustomProviderMeta {
            access_token: access_token.to_string(),
            maybe_refresh: refresh.map(|s| s.to_string()),
            kiro_region: None,
            kiro_profile_arn: None,
            antigravity_project: None,
            antigravity_metadata: None,
            codex_workspace_id: None,
            claude_account_uuid: None,
            claude_metadata: None,
        }),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn test_legacy_pipeline_without_pool_invokes_shared_oauth_refresh() {
    let pool_count = Arc::new(AtomicUsize::new(0));
    let shared_count = Arc::new(AtomicUsize::new(0));
    let mock = Arc::new(MockOAuthRegistry {
        pool_call_count: Arc::clone(&pool_count),
        shared_call_count: Arc::clone(&shared_count),
        refreshed_token: "shared_refreshed_token".to_string(),
    });

    let master_key = Arc::new(MasterKey::generate().expect("generate master key"));
    let mut config = crate::test_utils::test_config(Arc::clone(&master_key));
    config.oauth_provider_registry = Some(mock);

    let conn = tokio::task::spawn_blocking(|| {
        Arc::new(parking_lot::Mutex::new(
            rusqlite::Connection::open_in_memory().expect("in-memory conn"),
        ))
    })
    .await
    .expect("open conn");

    let pipeline = crate::Pipeline::new(conn, config);
    assert!(
        pipeline.db_pool.is_none(),
        "legacy pipeline must have db_pool == None"
    );

    let mut target = make_test_target(
        Some(AccountId(10)),
        "initial_token",
        Some("valid_refresh_token"),
    );

    try_proactive_oauth_refresh(&pipeline, &mut target).await;

    assert_eq!(
        shared_count.load(Ordering::SeqCst),
        1,
        "shared method must be called for legacy pipeline"
    );
    assert_eq!(
        pool_count.load(Ordering::SeqCst),
        0,
        "pool method must not be called when pool is None"
    );
    let meta = target.custom_meta.expect("custom_meta present");
    assert_eq!(meta.access_token, "shared_refreshed_token");
    assert!(
        meta.maybe_refresh.is_none(),
        "refresh token should be cleared on success"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn test_pipeline_with_pool_invokes_existing_pool_oauth_refresh() {
    let pool_count = Arc::new(AtomicUsize::new(0));
    let shared_count = Arc::new(AtomicUsize::new(0));
    let mock = Arc::new(MockOAuthRegistry {
        pool_call_count: Arc::clone(&pool_count),
        shared_call_count: Arc::clone(&shared_count),
        refreshed_token: "pool_refreshed_token".to_string(),
    });

    let master_key = Arc::new(MasterKey::generate().expect("generate master key"));
    let mut config = crate::test_utils::test_config(Arc::clone(&master_key));
    config.oauth_provider_registry = Some(mock);

    let pool = tokio::task::spawn_blocking(|| fresh_pool("oauth-pool-test"))
        .await
        .expect("fresh pool");
    let pipeline = crate::test_utils::test_pipeline_with_pool(pool, config);
    assert!(
        pipeline.db_pool.is_some(),
        "pipeline must have db_pool == Some"
    );

    let mut target = make_test_target(
        Some(AccountId(20)),
        "initial_token",
        Some("valid_refresh_token"),
    );

    try_proactive_oauth_refresh(&pipeline, &mut target).await;

    assert_eq!(
        pool_count.load(Ordering::SeqCst),
        1,
        "existing pool method must be called when pool is Some"
    );
    assert_eq!(
        shared_count.load(Ordering::SeqCst),
        0,
        "shared method must not be called when pool is Some"
    );
    let meta = target.custom_meta.expect("custom_meta present");
    assert_eq!(meta.access_token, "pool_refreshed_token");
    assert!(
        meta.maybe_refresh.is_none(),
        "refresh token should be cleared on success"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn test_legacy_pipeline_default_shared_impl_returns_explicit_error() {
    let mock = Arc::new(DefaultOnlyMockOAuthRegistry);

    let master_key = Arc::new(MasterKey::generate().expect("generate master key"));
    let mut config = crate::test_utils::test_config(Arc::clone(&master_key));
    config.oauth_provider_registry = Some(Arc::clone(&mock) as Arc<dyn PipelineOAuthRegistry>);

    let conn = tokio::task::spawn_blocking(|| {
        Arc::new(parking_lot::Mutex::new(
            rusqlite::Connection::open_in_memory().expect("in-memory conn"),
        ))
    })
    .await
    .expect("open conn");

    // Verify direct call to default trait method returns explicit error
    let upstream_client = Arc::new(UpstreamClient::new());
    let direct_res = mock
        .refresh_and_store_shared(
            "antigravity",
            "ref_tok",
            &upstream_client,
            AccountId(30),
            &conn,
            &master_key,
        )
        .await;
    match direct_res {
        Err(CoreError::Internal(msg)) => {
            assert!(
                msg.contains("refresh_and_store_shared not implemented"),
                "expected explicit not implemented error message, got: {msg}"
            );
        }
        other => panic!("expected Err(CoreError::Internal), got: {other:?}"),
    }

    // Verify try_proactive_oauth_refresh handles the error gracefully without panicking
    let pipeline = crate::Pipeline::new(conn, config);
    let mut target = make_test_target(
        Some(AccountId(30)),
        "initial_token",
        Some("valid_refresh_token"),
    );

    try_proactive_oauth_refresh(&pipeline, &mut target).await;

    let meta = target.custom_meta.expect("custom_meta present");
    assert_eq!(
        meta.access_token, "initial_token",
        "access token must remain unchanged on refresh failure"
    );
    assert!(
        meta.maybe_refresh.is_none(),
        "refresh token should be cleared to avoid loop"
    );
}
