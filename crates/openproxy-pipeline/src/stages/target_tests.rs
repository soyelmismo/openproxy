use super::*;
use crate::error_classification::{UpstreamErrorClass, is_hard_skip_error};
use openproxy_db::DbPool;
use openproxy_types::CoreError;
use openproxy_types::ids::{AccountId, ModelId};

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
