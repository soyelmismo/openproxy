use super::*;

#[test]
fn user_agent_contains_antigravity_and_version() {
    let ua_hdr = user_agent();
    let ua = ua_hdr.to_str().unwrap();
    assert!(ua.contains("Antigravity/"));
    assert!(ua.contains("Chrome/"));
    assert!(ua.contains("Electron/"));
}

#[test]
fn machine_id_is_stable_within_process() {
    let id1 = machine_id();
    let id2 = machine_id();
    assert_eq!(id1, id2, "machine_id must be stable within a process");
    assert!(!id1.is_empty());
}

#[test]
fn session_id_is_stable_within_process() {
    let s1 = session_id();
    let s2 = session_id();
    assert_eq!(s1, s2, "session_id must be stable within a process");
}

#[test]
fn inject_sets_all_headers() {
    let mut headers = http::HeaderMap::new();
    inject_antigravity_headers(&mut headers, Some("my-project-123"));
    assert_eq!(headers.get("x-client-name").unwrap(), "antigravity");
    assert!(headers.get("x-client-version").is_some());
    assert!(headers.get("x-machine-id").is_some());
    assert!(headers.get("x-vscode-sessionid").is_some());
    assert_eq!(
        headers.get("x-goog-user-project").unwrap(),
        "my-project-123"
    );
    assert!(headers.get(http::header::USER_AGENT).is_some());
}

#[test]
fn inject_skips_empty_project() {
    let mut headers = http::HeaderMap::new();
    inject_antigravity_headers(&mut headers, Some(""));
    assert!(headers.get("x-goog-user-project").is_none());
}

#[test]
fn inject_skips_test_project() {
    let mut headers = http::HeaderMap::new();
    inject_antigravity_headers(&mut headers, Some("test-project"));
    assert!(headers.get("x-goog-user-project").is_none());
}

#[test]
fn inject_skips_none_project() {
    let mut headers = http::HeaderMap::new();
    inject_antigravity_headers(&mut headers, None);
    assert!(headers.get("x-goog-user-project").is_none());
}

#[test]
fn inject_skips_placeholder_project_id() {
    let mut headers = http::HeaderMap::new();
    inject_antigravity_headers(&mut headers, Some("project-id"));
    assert!(headers.get("x-goog-user-project").is_none());
}

#[test]
fn test_is_valid_project_id() {
    assert!(!is_valid_project_id(""));
    assert!(!is_valid_project_id("test-project"));
    assert!(!is_valid_project_id("project-id"));
    assert!(is_valid_project_id("my-real-project-123"));
}

#[test]
fn test_oauth_user_agent_and_current_version() {
    let ua = oauth_user_agent();
    let version = current_version();
    assert!(ua.starts_with("vscode/1.X.X (Antigravity/"));
    assert!(ua.ends_with(')'));
    assert!(!version.is_empty());
}

#[test]
fn test_dynamic_version_override() {
    let old_ver = current_version();
    set_dynamic_version("4.7.3");
    assert_eq!(current_version(), "4.7.3");

    let ua = user_agent();
    assert!(ua.to_str().unwrap().contains("Antigravity/4.7.3"));

    let oauth = oauth_user_agent();
    assert_eq!(oauth, "vscode/1.X.X (Antigravity/4.7.3)");

    let mut hm = http::HeaderMap::new();
    inject_antigravity_headers(&mut hm, None);
    assert_eq!(hm.get("x-client-version").unwrap(), "4.7.3");
    assert!(hm.get(http::header::USER_AGENT).unwrap().to_str().unwrap().contains("Antigravity/4.7.3"));

    // Reset back to previous
    set_dynamic_version(old_ver);
}

#[test]
fn test_dynamic_extra_headers_override() {
    reset_dynamic_overrides();

    set_dynamic_extra_header("x-goog-api-client", "gl-rust/1.80");
    set_dynamic_extra_header("x-antigravity-canary", "canary-v1");

    let mut hm = http::HeaderMap::new();
    inject_antigravity_headers(&mut hm, None);

    assert_eq!(hm.get("x-goog-api-client").unwrap(), "gl-rust/1.80");
    assert_eq!(hm.get("x-antigravity-canary").unwrap(), "canary-v1");

    reset_dynamic_overrides();
    let mut hm_clean = http::HeaderMap::new();
    inject_antigravity_headers(&mut hm_clean, None);
    assert!(hm_clean.get("x-antigravity-canary").is_none());
}

#[test]
fn build_bearer_header_produces_correct_value() {
    let token = "ya29.test-token-with.dots_and-dashes";
    let header = build_bearer_header(token).expect("ascii token is valid");
    assert_eq!(
        header.to_str().unwrap(),
        "Bearer ya29.test-token-with.dots_and-dashes"
    );
}

#[test]
fn build_bearer_header_is_zero_alloc() {
    let token = "tok";
    let header = build_bearer_header(token).expect("ascii token is valid");
    let s = header.to_str().unwrap();
    assert_eq!(s.len(), "Bearer ".len() + token.len());
    assert!(s.starts_with("Bearer "));
    assert!(s.ends_with(token));
}

#[cfg(feature = "upstream-hyper")]
#[tokio::test]
async fn oauth_post_json_propagates_4xx() {
    use crate::upstream::tests_helper as mock_helper;
    use std::sync::Arc;

    let upstream: Arc<crate::upstream::UpstreamClient> =
        mock_helper::build_mock_upstream_returning_status(401, "auth required").await;
    let body = serde_json::json!({ "request": {} });
    let res = oauth_post_json(
        &upstream,
        "https://example.test/countTokens",
        &body,
        "fake-token",
        crate::upstream::TimeoutProfile::Chat,
    )
    .await;
    let err = res.expect_err("must error on 401");
    assert!(err.contains("401"), "msg must mention status: {err}");
    assert!(err.contains("auth required"), "body must be in msg: {err}");
    assert!(
        err.contains("countTokens"),
        "msg must mention the url: {err}"
    );
}

#[cfg(feature = "upstream-hyper")]
#[tokio::test]
async fn oauth_post_json_returns_body_on_2xx() {
    use crate::upstream::tests_helper as mock_helper;
    use std::sync::Arc;

    let upstream: Arc<crate::upstream::UpstreamClient> =
        mock_helper::build_mock_upstream_returning_status(200, r#"{"ok":true}"#).await;
    let body = serde_json::json!({ "metadata": {} });
    let res = oauth_post_json(
        &upstream,
        "https://example.test/loadCodeAssist",
        &body,
        "fake-token",
        crate::upstream::TimeoutProfile::OAuth,
    )
    .await
    .expect("2xx must return body");
    assert_eq!(std::str::from_utf8(&res).unwrap(), r#"{"ok":true}"#);
}

#[cfg(feature = "upstream-hyper")]
#[tokio::test]
async fn oauth_post_json_serializes_correctly() {
    use crate::upstream::tests_helper as mock_helper;
    use std::sync::Arc;

    let upstream: Arc<crate::upstream::UpstreamClient> =
        mock_helper::build_mock_upstream_returning_status(200, "{}").await;
    let body = serde_json::json!({
        "projectId": "p1",
        "metadata": {"ideType": "ANTIGRAVITY"},
        "tier": "free-tier",
    });
    let _ = oauth_post_json(
        &upstream,
        "https://example.test/onboardUser",
        &body,
        "fake-token",
        crate::upstream::TimeoutProfile::OAuth,
    )
    .await
    .expect("complex body must serialize");
}

#[cfg(feature = "upstream-hyper")]
#[tokio::test]
async fn fetch_with_fallback_tries_next_on_error() {
    use crate::upstream::tests_helper as mock_helper;
    use std::sync::Arc;

    let upstream: Arc<crate::upstream::UpstreamClient> =
        mock_helper::build_mock_upstream_routing(|path| {
            if path.contains("daily-cloudcode") {
                (401, "auth required".to_string())
            } else {
                (200, r#"{"id":"fallback-ok"}"#.to_string())
            }
        })
        .await;

    let endpoints = [
        "https://daily-cloudcode-pa.googleapis.com/v1internal:foo",
        "https://cloudcode-pa.googleapis.com/v1internal:foo",
    ];

    #[derive(serde::Deserialize, Debug, PartialEq)]
    struct Resp {
        id: String,
    }

    let res: Resp = fetch_with_fallback(
        &upstream,
        &endpoints,
        &serde_json::json!({}),
        "tok",
        crate::upstream::TimeoutProfile::Quota,
        "test",
    )
    .await
    .expect("must fallback to second endpoint");
    assert_eq!(res.id, "fallback-ok");
}

#[cfg(feature = "upstream-hyper")]
#[tokio::test]
async fn fetch_with_fallback_returns_last_error_on_all_failure() {
    use crate::upstream::tests_helper as mock_helper;
    use std::sync::Arc;

    let upstream: Arc<crate::upstream::UpstreamClient> =
        mock_helper::build_mock_upstream_routing(|_path| (503, "down".to_string())).await;

    let endpoints = ["https://a.test/foo", "https://b.test/foo"];

    let res: Result<serde_json::Value, _> = fetch_with_fallback(
        &upstream,
        &endpoints,
        &serde_json::json!({}),
        "tok",
        crate::upstream::TimeoutProfile::Quota,
        "test",
    )
    .await;
    let err = res.expect_err("all 503 must fail");
    assert!(err.contains("503"));
    assert!(err.contains("down"));
}

#[cfg(feature = "upstream-hyper")]
#[tokio::test]
async fn fetch_with_fallback_empty_endpoints_returns_error() {
    use crate::upstream::tests_helper as mock_helper;
    use std::sync::Arc;

    let upstream: Arc<crate::upstream::UpstreamClient> =
        mock_helper::build_mock_upstream_returning_status(200, "{}").await;

    let res: Result<serde_json::Value, _> = fetch_with_fallback(
        &upstream,
        &[],
        &serde_json::json!({}),
        "tok",
        crate::upstream::TimeoutProfile::Quota,
        "test-empty",
    )
    .await;
    let err = res.expect_err("empty slice must fail");
    assert!(err.contains("all endpoints failed"), "msg: {err}");
    assert!(err.contains("test-empty"), "msg: {err}");
}

#[cfg(feature = "upstream-hyper")]
#[tokio::test]
async fn fetch_with_fallback_propagates_parse_error() {
    use crate::upstream::tests_helper as mock_helper;
    use std::sync::Arc;

    let upstream: Arc<crate::upstream::UpstreamClient> =
        mock_helper::build_mock_upstream_returning_status(200, "not-json-at-all").await;

    let endpoints = ["https://a.test/foo"];

    let res: Result<serde_json::Value, _> = fetch_with_fallback(
        &upstream,
        &endpoints,
        &serde_json::json!({}),
        "tok",
        crate::upstream::TimeoutProfile::Quota,
        "test-parse",
    )
    .await;
    let err = res.expect_err("non-JSON body must fail to parse");
    assert!(err.contains("parse"), "msg must mention parse: {err}");
    assert!(
        err.contains("test-parse"),
        "msg must mention context: {err}"
    );
}
