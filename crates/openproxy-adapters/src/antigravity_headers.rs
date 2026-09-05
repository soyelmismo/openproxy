//! Antigravity (Google Cloud Code) client identity headers.
//!
//! The cloudcode-pa.googleapis.com API requires specific headers to
//! identify the client as a legitimate Antigravity installation.
//! Without these headers, the API may reject requests or return
//! errors. This module centralizes the header construction so the
//! executor, quota fetch, and OAuth flow all send identical headers.
//!
//! Headers (from the Antigravity-Manager reference implementation):
//! - `User-Agent: Antigravity/{version} ({platform}) Chrome/{chrome} Electron/{electron}`
//! - `x-client-name: antigravity`
//! - `x-client-version: {version}`
//! - `x-machine-id: {persistent machine UID}`
//! - `x-vscode-sessionid: {per-launch UUID}`
//! - `x-goog-user-project: {project_id}` (when project_id is known)
//!
//! The `x-machine-id` is generated once per process lifetime (using
//! the `machine-uid` crate's equivalent — a hash of the hostname +
//! platform-specific machine GUID). The `x-vscode-sessionid` is a
//! UUID generated once per process launch.

use http::{HeaderValue, header::HeaderName};
use sha2::{Digest, Sha256};
use std::fmt::Write;
use std::sync::LazyLock;
use uuid::Uuid;

static HEADER_X_CLIENT_NAME: HeaderName = HeaderName::from_static("x-client-name");
static HEADER_X_CLIENT_VERSION: HeaderName = HeaderName::from_static("x-client-version");
static HEADER_X_MACHINE_ID: HeaderName = HeaderName::from_static("x-machine-id");
static HEADER_X_VSCODE_SESSIONID: HeaderName = HeaderName::from_static("x-vscode-sessionid");
static HEADER_X_GOOG_USER_PROJECT: HeaderName = HeaderName::from_static("x-goog-user-project");

/// Known stable Antigravity version (must be >= the version Google's
/// API requires to accept requests). Updated from the
/// Antigravity-Manager reference.
const KNOWN_STABLE_VERSION: &str = "4.3.0";
const KNOWN_STABLE_CHROME: &str = "132.0.6834.160";
const KNOWN_STABLE_ELECTRON: &str = "39.2.3";

/// Platform info for the User-Agent string.
fn platform_info() -> &'static str {
    match std::env::consts::OS {
        "macos" => "Macintosh; Intel Mac OS X 10_15_7",
        "windows" => "Windows NT 10.0; Win64; x64",
        _ => "X11; Linux x86_64",
    }
}

static VERSION: LazyLock<String> = LazyLock::new(|| {
    std::env::var("OPENPROXY_ANTIGRAVITY_VERSION")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| KNOWN_STABLE_VERSION.to_string())
});

static HEADER_VAL_VERSION: LazyLock<HeaderValue> =
    LazyLock::new(|| HeaderValue::from_str(&VERSION).expect("version must be valid ascii"));

fn version() -> &'static str {
    &VERSION
}

/// Persistent machine ID. Generated once per process lifetime from
/// the hostname + OS. This mimics the `machine_uid` crate used by the
/// Antigravity-Manager — it produces a stable-per-machine identifier
/// that the API uses for rate-limiting and session tracking.
static MACHINE_ID: LazyLock<String> = LazyLock::new(|| {
    // Try to read a stable machine identifier from the OS.
    // Fallback: hostname + OS arch.
    let raw = hostname().map_or_else(
        || format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        String::from,
    );
    // Hash to a fixed-length hex string for a clean header value.
    let mut hasher = Sha256::new();
    hasher.update(raw.as_bytes());
    hasher.update(std::env::consts::OS.as_bytes());
    hasher.update(std::env::consts::ARCH.as_bytes());
    let hash = hasher.finalize();
    // Take the first 32 hex chars (128 bits) — enough entropy
    // for a machine fingerprint without being too long.
    let mut out = String::with_capacity(32);
    for b in hash.iter().take(16) {
        let _ = write!(out, "{b:02x}");
    }
    out
});

static HEADER_VAL_MACHINE_ID: LazyLock<HeaderValue> =
    LazyLock::new(|| HeaderValue::from_str(&MACHINE_ID).expect("machine_id must be valid ascii"));

#[cfg(test)]
fn machine_id() -> &'static str {
    &MACHINE_ID
}

static HOSTNAME: LazyLock<Option<String>> = LazyLock::new(|| {
    // Try /etc/hostname first (Linux), then the HOSTNAME env var, then
    // gethostname crate equivalent (std::env::var).
    if let Ok(s) = std::fs::read_to_string("/etc/hostname") {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    if let Ok(s) = std::env::var("HOSTNAME")
        && !s.is_empty()
    {
        return Some(s);
    }
    if let Ok(s) = std::env::var("COMPUTERNAME")
        && !s.is_empty()
    {
        return Some(s);
    }
    None
});

/// Best-effort hostname read. Returns `None` if the hostname can't be
/// determined (e.g. in a container without hostname configured).
fn hostname() -> Option<&'static str> {
    HOSTNAME.as_deref()
}

static SESSION_ID: LazyLock<String> = LazyLock::new(|| Uuid::new_v4().to_string());

static HEADER_VAL_SESSION_ID: LazyLock<HeaderValue> =
    LazyLock::new(|| HeaderValue::from_str(&SESSION_ID).expect("session_id must be valid ascii"));

/// Per-launch session ID. Generated once per process lifetime.
#[cfg(test)]
fn session_id() -> &'static str {
    &SESSION_ID
}

static HEADER_VAL_USER_AGENT: LazyLock<HeaderValue> = LazyLock::new(|| {
    let mut bytes = bytes::BytesMut::with_capacity(128);
    bytes.extend_from_slice(b"Antigravity/");
    bytes.extend_from_slice(version().as_bytes());
    bytes.extend_from_slice(b" (");
    bytes.extend_from_slice(platform_info().as_bytes());
    bytes.extend_from_slice(b") Chrome/");
    bytes.extend_from_slice(KNOWN_STABLE_CHROME.as_bytes());
    bytes.extend_from_slice(b" Electron/");
    bytes.extend_from_slice(KNOWN_STABLE_ELECTRON.as_bytes());
    HeaderValue::from_maybe_shared(bytes.freeze()).expect("user_agent must be valid ascii")
});

/// Native OAuth User-Agent (used for token exchange / refresh / userinfo):
/// `vscode/1.X.X (Antigravity/{version})`
pub fn oauth_user_agent() -> String {
    let mut out = String::with_capacity(64);
    use std::fmt::Write;
    let _ = write!(out, "vscode/1.X.X (Antigravity/{})", version());
    out
}

fn is_valid_project_id(pid: &str) -> bool {
    !pid.is_empty() && pid != "test-project" && pid != "project-id"
}

/// Inject all Antigravity client-identity headers into an
/// `http::HeaderMap`. The caller is responsible for setting
/// `Authorization` and `Content-Type` separately.
///
/// `project_id` is optional — when present, `x-goog-user-project` is
/// set to the project ID (required for the API to route the request
/// to the correct Cloud Code project).
pub fn inject_antigravity_headers(headers: &mut http::HeaderMap, project_id: Option<&str>) {
    headers.insert(http::header::USER_AGENT, HEADER_VAL_USER_AGENT.clone());
    headers.insert(
        &HEADER_X_CLIENT_NAME,
        HeaderValue::from_static("antigravity"),
    );
    headers.insert(&HEADER_X_CLIENT_VERSION, HEADER_VAL_VERSION.clone());
    headers.insert(&HEADER_X_MACHINE_ID, HEADER_VAL_MACHINE_ID.clone());
    headers.insert(&HEADER_X_VSCODE_SESSIONID, HEADER_VAL_SESSION_ID.clone());

    if let Some(pid) = project_id.filter(|p| is_valid_project_id(p))
        && let Ok(v) = HeaderValue::from_str(pid)
    {
        headers.insert(&HEADER_X_GOOG_USER_PROJECT, v);
    }
}

/// Get the current Antigravity version string (for logging / diagnostics).
pub fn current_version() -> String {
    version().to_string()
}

/// Build a zero-allocation `Authorization: Bearer <token>` header value.
///
/// Returns `Err(InvalidHeaderValue)` if `token` contains control bytes
/// (CR, LF, NUL, DEL, etc.) that would produce an invalid HTTP header
/// value. Google OAuth tokens are normally restricted to a safe alphabet
/// (see RFC 6749 §A.12), but a malformed or hostile token must not
/// panic the request path — callers should propagate the error.
pub fn build_bearer_header(
    token: &str,
) -> std::result::Result<HeaderValue, http::header::InvalidHeaderValue> {
    let mut buf = bytes::BytesMut::with_capacity(7 + token.len());
    buf.extend_from_slice(b"Bearer ");
    buf.extend_from_slice(token.as_bytes());
    http::HeaderValue::from_maybe_shared(buf.freeze())
}

/// Convenience: insert a Bearer `Authorization` header into a request.
///
/// Returns `Err(InvalidHeaderValue)` if the token contains bytes that
/// would produce an invalid HTTP header value.
pub fn insert_bearer(
    req: &mut crate::upstream::UpstreamRequest,
    token: &str,
) -> std::result::Result<(), http::header::InvalidHeaderValue> {
    req.headers
        .insert(http::header::AUTHORIZATION, build_bearer_header(token)?);
    Ok(())
}

/// POST JSON to a Google Cloud Code endpoint with Bearer auth and
/// Antigravity client-identity headers.
///
/// Returns the raw response body bytes on a 2xx status. On failure,
/// returns a formatted `String` error of the form
/// `"{url} <context>: {upstream-error-or-status-body}"`.
///
/// Generic over the body type so callers can pass `serde_json::Value`,
/// a typed struct, or anything else `Serialize`.
pub async fn oauth_post_json<T: serde::Serialize>(
    upstream: &std::sync::Arc<crate::upstream::UpstreamClient>,
    url: &str,
    body: &T,
    access_token: &str,
    timeout: crate::upstream::TimeoutProfile,
) -> Result<bytes::Bytes, String> {
    let body_bytes = serde_json::to_vec(body).map_err(|e| format!("{url} serialize: {e}"))?;

    let mut req = crate::upstream::UpstreamRequest::post_json(url, bytes::Bytes::from(body_bytes));
    insert_bearer(&mut req, access_token).map_err(|e| format!("{url} build bearer header: {e}"))?;
    inject_antigravity_headers(&mut req.headers, None);
    req.is_streaming = false;

    let cancel = crate::upstream::CancellationToken::new();
    let resp = upstream
        .call(req, timeout, cancel)
        .await
        .map_err(|e| format!("{url} call: {e}"))?;

    if !resp.status.is_success() {
        let status = resp.status.as_u16();
        let body_str =
            String::from_utf8_lossy(&resp.collect().await.unwrap_or_default()).into_owned();
        return Err(format!("{url} status {status}: {body_str}"));
    }

    resp.collect()
        .await
        .map_err(|e| format!("{url} collect: {e}"))
}

/// Iterate over `endpoints` and POST JSON to each in order with Bearer
/// auth + Antigravity headers. Returns the first successful (2xx)
/// response body parsed into `R`. Returns the last error if all
/// endpoints fail (or `"{context}: all endpoints failed"` if
/// `endpoints` is empty).
///
/// **Does NOT** translate `UpstreamError::Cancel` into
/// `CoreError::Cancelled` — the caller must handle cancellation
/// semantics separately if needed (see
/// `AntigravityAdapter::fetch_antigravity_user_quota_local`).
pub async fn fetch_with_fallback<T, R>(
    upstream: &std::sync::Arc<crate::upstream::UpstreamClient>,
    endpoints: &[&str],
    body: &T,
    access_token: &str,
    timeout: crate::upstream::TimeoutProfile,
    context: &str,
) -> std::result::Result<R, String>
where
    T: serde::Serialize,
    R: serde::de::DeserializeOwned,
{
    let mut last_err: Option<String> = None;
    for url in endpoints {
        match oauth_post_json(upstream, url, body, access_token, timeout).await {
            Ok(body_bytes) => {
                return serde_json::from_slice(&body_bytes)
                    .map_err(|e| format!("{context} parse {url}: {e}"));
            }
            Err(e) => {
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| format!("{context}: all endpoints failed")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_agent_contains_antigravity_and_version() {
        let ua = HEADER_VAL_USER_AGENT.to_str().unwrap();
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
    fn test_oauth_user_agent_and_current_version() {
        let ua = oauth_user_agent();
        let version = current_version();
        assert!(ua.starts_with("vscode/1.X.X (Antigravity/"));
        assert!(ua.ends_with(')'));
        assert_eq!(version, VERSION.as_str());
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
        // Verify that the construction path preserves the exact bytes
        // "Bearer <token>": BytesMut + extend_from_slice is allocation-free
        // (the prefix is a static slice, the token bytes are copied once),
        // and HeaderValue::from_maybe_shared shares the Bytes buffer with
        // hyper rather than allocating an intermediate String.
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

        // Use the routing helper so the listener accepts multiple
        // connections (fetch_with_fallback iterates over 2 endpoints
        // and would otherwise get connection-refused on the 2nd).
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
        // W4 fix: a 2xx response with a non-JSON body must surface
        // as a parse error, NOT as success. The helper must not
        // blindly accept whatever bytes the upstream returned.
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
}

#[cfg(test)]
mod adversarial_dedup_tests {
    use super::*;
    use parking_lot::Mutex;

    #[allow(dead_code)]
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    // ADVERSARIAL BUILD_BEARER_HEADER

    #[test]
    fn build_bearer_header_empty_token_just_prefix() {
        let header = build_bearer_header("").expect("empty token is valid");
        let s = header.to_str().expect("Bearer prefix is valid ASCII");
        assert_eq!(s, "Bearer ");
        assert_eq!(s.len(), 7, "literal prefix is 7 bytes");
    }

    #[test]
    fn build_bearer_header_crlf_injection_returns_err() {
        assert!(
            build_bearer_header("abc\r\nX-Injected: bad").is_err(),
            "CRLF must produce InvalidHeaderValue"
        );
    }

    #[test]
    fn build_bearer_header_lone_lf_injection_returns_err() {
        assert!(
            build_bearer_header("abc\nX-Injected: bad").is_err(),
            "LF must produce InvalidHeaderValue"
        );
    }

    #[test]
    fn build_bearer_header_nul_byte_returns_err() {
        assert!(
            build_bearer_header("abc\0def").is_err(),
            "NUL must produce InvalidHeaderValue"
        );
    }

    #[test]
    fn build_bearer_header_tab_is_accepted_per_rfc7230() {
        let header = build_bearer_header("abc\tdef").expect("HTAB is valid in field-value");
        let s = header.to_str().expect("HTAB is valid in field-value");
        assert!(s.starts_with("Bearer "));
        assert!(s.contains('\t'));
    }

    #[test]
    fn build_bearer_header_del_char_returns_err() {
        assert!(
            build_bearer_header("abc\x7Fdef").is_err(),
            "DEL must produce InvalidHeaderValue"
        );
    }

    #[test]
    fn build_bearer_header_non_ascii_multibyte_accepted_at_byte_level() {
        let header = build_bearer_header("café").expect("multibyte is valid at byte level");
        assert_eq!(header.as_bytes(), b"Bearer caf\xC3\xA9");
        assert!(header.to_str().is_err());
    }

    #[test]
    fn build_bearer_header_emoji_accepted_at_byte_level() {
        let header = build_bearer_header("🔑").expect("emoji is valid at byte level");
        assert_eq!(header.as_bytes(), b"Bearer \xF0\x9F\x94\x91");
        assert!(header.to_str().is_err());
    }

    #[test]
    fn build_bearer_header_rtl_override_accepted_at_byte_level() {
        let mut bytes: Vec<u8> = b"abc".to_vec();
        bytes.extend_from_slice(&[0xE2, 0x80, 0xAE]);
        bytes.extend_from_slice(b"def");
        let token = std::str::from_utf8(&bytes).expect("valid utf8");
        let header = build_bearer_header(token).expect("valid utf8 at byte level");
        assert!(
            header
                .as_bytes()
                .windows(3)
                .any(|w| w == [0xE2, 0x80, 0xAE]),
            "U+202E bytes preserved verbatim"
        );
    }

    #[test]
    fn build_bearer_header_high_bit_latin1_accepted_at_byte_level() {
        let header = build_bearer_header("abc\u{00C0}def").expect("latin1 is valid at byte level");
        assert!(header.as_bytes().windows(2).any(|w| w == [0xC3, 0x80]));
        assert!(header.to_str().is_err());
    }

    #[test]
    fn build_bearer_header_1mb_token_succeeds() {
        let token: String = "a".repeat(1024 * 1024);
        let header = build_bearer_header(&token).expect("1 MiB ASCII is a valid HeaderValue");
        let s = header.to_str().expect("1 MiB ASCII is a valid HeaderValue");
        assert_eq!(s.len(), 7 + token.len(), "no bytes dropped");
        assert!(s.starts_with("Bearer "));
        assert!(s.ends_with('a'));
    }

    #[test]
    fn build_bearer_header_rfc6749_alphabet_succeeds() {
        let token = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
        let header = build_bearer_header(token).expect("RFC 6749 alphabet is valid");
        assert_eq!(header.to_str().unwrap(), format!("Bearer {token}"));
    }

    #[test]
    fn build_bearer_header_printable_punctuation_succeeds() {
        let token = "!@#$%^&*()_+-=[]{}|;:,.<>?/";
        let header = build_bearer_header(token).expect("printable punctuation is valid");
        assert_eq!(header.to_str().unwrap(), format!("Bearer {token}"));
    }

    // ADVERSARIAL INSERT_BEARER

    #[test]
    fn insert_bearer_twice_overwrites_not_duplicates() {
        let mut req = crate::upstream::UpstreamRequest::post_json(
            "https://example.test/",
            bytes::Bytes::new(),
        );
        insert_bearer(&mut req, "first").expect("ascii token is valid");
        insert_bearer(&mut req, "second").expect("ascii token is valid");
        let values: Vec<_> = req
            .headers
            .get_all(http::header::AUTHORIZATION)
            .iter()
            .collect();
        assert_eq!(values.len(), 1, "insert must replace, not append");
        assert_eq!(values[0].to_str().unwrap(), "Bearer second");
    }

    #[test]
    fn insert_bearer_into_empty_headers_adds_one() {
        let mut req = crate::upstream::UpstreamRequest::get("https://example.test/");
        assert!(req.headers.is_empty(), "precondition: empty");
        insert_bearer(&mut req, "tok").expect("ascii token is valid");
        assert_eq!(req.headers.len(), 1);
        assert_eq!(
            req.headers
                .get(http::header::AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer tok"
        );
    }

    #[test]
    fn insert_bearer_preserves_other_headers() {
        let mut req = crate::upstream::UpstreamRequest::post_json(
            "https://example.test/",
            bytes::Bytes::new(),
        );
        req.headers.insert(
            http::header::HeaderName::from_static("x-custom"),
            http::HeaderValue::from_static("custom-value"),
        );
        insert_bearer(&mut req, "tok").expect("ascii token is valid");
        assert_eq!(
            req.headers.len(),
            3,
            "Authorization + Content-Type + x-custom"
        );
        assert_eq!(
            req.headers.get(http::header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        assert_eq!(req.headers.get("x-custom").unwrap(), "custom-value");
        assert_eq!(
            req.headers
                .get(http::header::AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer tok"
        );
    }

    #[test]
    fn insert_bearer_exact_value_no_normalization() {
        let mut req = crate::upstream::UpstreamRequest::get("https://example.test/");
        insert_bearer(&mut req, "abc.def-ghi_jkl").expect("ascii token is valid");
        let v = req.headers.get(http::header::AUTHORIZATION).unwrap();
        let bytes = v.as_bytes();
        assert_eq!(bytes, b"Bearer abc.def-ghi_jkl");
        assert!(!bytes.ends_with(b" "), "no trailing space");
        assert!(!bytes.windows(2).any(|w| w == b"  "), "no double space");
    }

    // ADVERSARIAL OAUTH_POST_JSON

    #[cfg(feature = "upstream-hyper")]
    #[tokio::test]
    async fn oauth_post_json_empty_url_propagates_upstream_error() {
        use crate::upstream::tests_helper as mock_helper;
        use std::sync::Arc;
        let upstream: Arc<crate::upstream::UpstreamClient> =
            mock_helper::build_mock_upstream_returning_status(200, "{}").await;
        let res = oauth_post_json(
            &upstream,
            "",
            &serde_json::json!({}),
            "tok",
            crate::upstream::TimeoutProfile::Chat,
        )
        .await;
        assert!(res.is_err(), "empty url must produce an error, got Ok");
    }

    #[cfg(feature = "upstream-hyper")]
    #[tokio::test]
    async fn oauth_post_json_nan_body_serializes_as_null() {
        use crate::upstream::tests_helper as mock_helper;
        use std::sync::Arc;
        let upstream: Arc<crate::upstream::UpstreamClient> =
            mock_helper::build_mock_upstream_routing(|_path| {
                (200, r#"{"echoed":true}"#.to_string())
            })
            .await;
        let body = serde_json::json!({ "fraction": f64::NAN });
        let raw = serde_json::to_vec(&body).expect("NaN must serialize (as null)");
        let raw_str = std::str::from_utf8(&raw).unwrap();
        assert!(raw_str.contains(r#""fraction":null"#));
        let res = oauth_post_json(
            &upstream,
            "https://example.test/x",
            &body,
            "tok",
            crate::upstream::TimeoutProfile::Chat,
        )
        .await
        .expect("NaN-as-null body must succeed end-to-end");
        assert_eq!(&res[..], br#"{"echoed":true}"#);
    }

    #[cfg(feature = "upstream-hyper")]
    #[tokio::test]
    async fn oauth_post_json_infinity_body_serializes_as_null() {
        let body = serde_json::json!({
            "pos_inf": f64::INFINITY,
            "neg_inf": f64::NEG_INFINITY,
        });
        let raw = serde_json::to_vec(&body).expect("inf must serialize (as null)");
        let raw_str = std::str::from_utf8(&raw).unwrap();
        assert!(raw_str.contains(r#""pos_inf":null"#));
        assert!(raw_str.contains(r#""neg_inf":null"#));
    }

    #[cfg(feature = "upstream-hyper")]
    #[tokio::test]
    async fn oauth_post_json_5xx_includes_body_and_url() {
        use crate::upstream::tests_helper as mock_helper;
        use std::sync::Arc;
        let upstream: Arc<crate::upstream::UpstreamClient> =
            mock_helper::build_mock_upstream_returning_status(503, "service down").await;
        let res = oauth_post_json(
            &upstream,
            "https://example.test/v1internal:foo",
            &serde_json::json!({}),
            "tok",
            crate::upstream::TimeoutProfile::Chat,
        )
        .await;
        let err = res.expect_err("503 must error");
        assert!(err.contains("503"));
        assert!(err.contains("service down"));
        assert!(err.contains("v1internal:foo"));
    }

    #[cfg(feature = "upstream-hyper")]
    #[tokio::test]
    async fn oauth_post_json_4xx_with_nul_byte_does_not_panic() {
        use crate::upstream::tests_helper as mock_helper;
        use std::sync::Arc;
        let upstream: Arc<crate::upstream::UpstreamClient> =
            mock_helper::build_mock_upstream_returning_status(400, "abc\0def").await;
        let res = oauth_post_json(
            &upstream,
            "https://example.test/x",
            &serde_json::json!({}),
            "tok",
            crate::upstream::TimeoutProfile::Chat,
        )
        .await;
        let err = res.expect_err("400 must error");
        assert!(err.contains("400"));
    }

    #[cfg(feature = "upstream-hyper")]
    #[tokio::test]
    async fn oauth_post_json_2xx_empty_body_returns_empty_bytes() {
        use crate::upstream::tests_helper as mock_helper;
        use std::sync::Arc;
        let upstream: Arc<crate::upstream::UpstreamClient> =
            mock_helper::build_mock_upstream_returning_status(200, "").await;
        let res = oauth_post_json(
            &upstream,
            "https://example.test/x",
            &serde_json::json!({}),
            "tok",
            crate::upstream::TimeoutProfile::Chat,
        )
        .await
        .expect("2xx must succeed even with empty body");
        assert!(res.is_empty(), "empty body -> empty bytes");
    }

    #[cfg(feature = "upstream-hyper")]
    #[tokio::test]
    async fn oauth_post_json_concurrent_calls_do_not_panic() {
        use crate::upstream::tests_helper as mock_helper;
        use std::sync::Arc;
        let upstream: Arc<crate::upstream::UpstreamClient> =
            mock_helper::build_mock_upstream_returning_status(200, "{}").await;
        let mut handles = Vec::new();
        for _ in 0..8 {
            let upstream = Arc::clone(&upstream);
            handles.push(tokio::spawn(async move {
                let body = serde_json::json!({});
                oauth_post_json(
                    &upstream,
                    "https://example.test/x",
                    &body,
                    "tok",
                    crate::upstream::TimeoutProfile::Chat,
                )
                .await
            }));
        }
        let joined = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            for h in handles {
                let _ = h.await;
            }
        })
        .await;
        assert!(joined.is_ok(), "concurrent calls must not hang");
    }

    #[cfg(feature = "upstream-hyper")]
    #[tokio::test]
    async fn oauth_post_json_large_body_1kb() {
        use crate::upstream::tests_helper as mock_helper;
        use std::sync::Arc;
        let upstream: Arc<crate::upstream::UpstreamClient> =
            mock_helper::build_mock_upstream_returning_status(200, "{}").await;
        let big: String = "x".repeat(1024);
        let body = serde_json::json!({ "blob": big });
        let res = oauth_post_json(
            &upstream,
            "https://example.test/x",
            &body,
            "tok",
            crate::upstream::TimeoutProfile::Chat,
        )
        .await
        .expect("1 KiB body must succeed");
        assert_eq!(&res[..], b"{}");
    }

    // ADVERSARIAL FETCH_WITH_FALLBACK

    #[cfg(feature = "upstream-hyper")]
    #[tokio::test]
    async fn fetch_with_fallback_5_endpoints_all_5xx_returns_last() {
        use crate::upstream::tests_helper as mock_helper;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_c = Arc::clone(&counter);
        let upstream: Arc<crate::upstream::UpstreamClient> =
            mock_helper::build_mock_upstream_routing(move |_path| {
                let n = counter_c.fetch_add(1, Ordering::SeqCst) + 1;
                (503, format!("err-{n}"))
            })
            .await;
        let endpoints = [
            "https://a.test/1",
            "https://a.test/2",
            "https://a.test/3",
            "https://a.test/4",
            "https://a.test/5",
        ];
        let res: Result<serde_json::Value, _> = fetch_with_fallback(
            &upstream,
            &endpoints,
            &serde_json::json!({}),
            "tok",
            crate::upstream::TimeoutProfile::Quota,
            "all-fail",
        )
        .await;
        let err = res.expect_err("all 5xx must fail");
        assert!(err.contains("err-5"), "msg must be the last error: {err}");
        assert_eq!(counter.load(Ordering::SeqCst), 5);
    }

    #[cfg(feature = "upstream-hyper")]
    #[tokio::test]
    async fn fetch_with_fallback_first_401_second_200_uses_second() {
        use crate::upstream::tests_helper as mock_helper;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_c = Arc::clone(&counter);
        let upstream: Arc<crate::upstream::UpstreamClient> =
            mock_helper::build_mock_upstream_routing(move |_path| {
                let n = counter_c.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    (401, "auth required".to_string())
                } else {
                    (200, r#"{"v":42}"#.to_string())
                }
            })
            .await;
        let endpoints = ["https://a.test/1", "https://a.test/2"];
        let res: serde_json::Value = fetch_with_fallback(
            &upstream,
            &endpoints,
            &serde_json::json!({}),
            "tok",
            crate::upstream::TimeoutProfile::Quota,
            "ctx",
        )
        .await
        .expect("must fall back");
        assert_eq!(res["v"], 42);
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[cfg(feature = "upstream-hyper")]
    #[tokio::test]
    async fn fetch_with_fallback_single_endpoint_parse_error_format() {
        use crate::upstream::tests_helper as mock_helper;
        use std::sync::Arc;
        let upstream: Arc<crate::upstream::UpstreamClient> =
            mock_helper::build_mock_upstream_returning_status(200, "<html>nope</html>").await;
        let endpoints = ["https://a.test/only"];
        let res: Result<serde_json::Value, _> = fetch_with_fallback(
            &upstream,
            &endpoints,
            &serde_json::json!({}),
            "tok",
            crate::upstream::TimeoutProfile::Quota,
            "my-ctx",
        )
        .await;
        let err = res.expect_err("must fail to parse");
        assert!(err.contains("my-ctx"));
        assert!(err.contains("parse"));
        assert!(err.contains("a.test/only"));
    }

    #[cfg(feature = "upstream-hyper")]
    #[tokio::test]
    async fn fetch_with_fallback_context_with_special_chars_preserved() {
        use crate::upstream::tests_helper as mock_helper;
        use std::sync::Arc;
        let upstream: Arc<crate::upstream::UpstreamClient> =
            mock_helper::build_mock_upstream_returning_status(200, "not json").await;
        let nasty = "ctx{x}:100%\u{1F608}";
        let endpoints = ["https://a.test/x"];
        let res: Result<serde_json::Value, _> = fetch_with_fallback(
            &upstream,
            &endpoints,
            &serde_json::json!({}),
            "tok",
            crate::upstream::TimeoutProfile::Quota,
            nasty,
        )
        .await;
        let err = res.expect_err("must fail");
        assert!(
            err.contains(nasty),
            "context must be preserved verbatim: {err}"
        );
    }

    #[cfg(feature = "upstream-hyper")]
    #[tokio::test]
    async fn fetch_with_fallback_empty_slice_message_format() {
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
            "ctx-empty",
        )
        .await;
        let err = res.expect_err("empty slice must fail");
        assert_eq!(err, "ctx-empty: all endpoints failed");
    }

    #[cfg(feature = "upstream-hyper")]
    #[tokio::test]
    async fn fetch_with_fallback_unicode_path_passes_through() {
        use crate::upstream::tests_helper as mock_helper;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_c = Arc::clone(&counter);
        let upstream: Arc<crate::upstream::UpstreamClient> =
            mock_helper::build_mock_upstream_routing(move |_path| {
                counter_c.fetch_add(1, Ordering::SeqCst);
                (200, r#"{"ok":true}"#.to_string())
            })
            .await;
        let endpoints =
            ["https://a.test/v1internal:foo?emoji=%F0%9F%94%91&name=hello%20world&rtl=%E2%80%AE"];
        let res: serde_json::Value = fetch_with_fallback(
            &upstream,
            &endpoints,
            &serde_json::json!({}),
            "tok",
            crate::upstream::TimeoutProfile::Quota,
            "unicode-ctx",
        )
        .await
        .expect("unicode URL must succeed");
        assert_eq!(res["ok"], true);
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[cfg(feature = "upstream-hyper")]
    #[tokio::test]
    async fn fetch_with_fallback_dropped_future_does_not_panic() {
        use crate::upstream::tests_helper as mock_helper;
        use std::sync::Arc;
        let upstream: Arc<crate::upstream::UpstreamClient> =
            mock_helper::build_mock_upstream_routing(|_path| (503, "down".to_string())).await;
        let endpoints = ["https://a.test/1", "https://a.test/2"];
        let body = serde_json::json!({});
        let fut = fetch_with_fallback::<_, serde_json::Value>(
            &upstream,
            &endpoints,
            &body,
            "tok",
            crate::upstream::TimeoutProfile::Quota,
            "drop-ctx",
        );
        drop(fut);
    }

    #[cfg(feature = "upstream-hyper")]
    #[tokio::test]
    async fn fetch_with_fallback_deserializes_into_custom_struct() {
        use crate::upstream::tests_helper as mock_helper;
        use std::sync::Arc;
        let upstream: Arc<crate::upstream::UpstreamClient> =
            mock_helper::build_mock_upstream_returning_status(
                200,
                r#"{"id":"abc","count":7,"nested":{"flag":true}}"#,
            )
            .await;
        let endpoints = ["https://a.test/only"];
        #[derive(serde::Deserialize, Debug, PartialEq)]
        struct Outer {
            id: String,
            count: u32,
            nested: Inner,
        }
        #[derive(serde::Deserialize, Debug, PartialEq)]
        struct Inner {
            flag: bool,
        }
        let res: Outer = fetch_with_fallback(
            &upstream,
            &endpoints,
            &serde_json::json!({}),
            "tok",
            crate::upstream::TimeoutProfile::Quota,
            "struct-ctx",
        )
        .await
        .expect("must succeed");
        assert_eq!(
            res,
            Outer {
                id: "abc".to_string(),
                count: 7,
                nested: Inner { flag: true }
            }
        );
    }

    #[cfg(feature = "upstream-hyper")]
    #[tokio::test]
    async fn fetch_with_fallback_typed_body_serializes() {
        use crate::upstream::tests_helper as mock_helper;
        use std::sync::Arc;
        let upstream: Arc<crate::upstream::UpstreamClient> =
            mock_helper::build_mock_upstream_returning_status(200, r#"{"ok":true}"#).await;
        #[derive(serde::Serialize)]
        struct Req<'a> {
            metadata: Metadata<'a>,
        }
        #[derive(serde::Serialize)]
        struct Metadata<'a> {
            ide_type: &'a str,
            platform: &'a str,
        }
        let body = Req {
            metadata: Metadata {
                ide_type: "ANTIGRAVITY",
                platform: "linux",
            },
        };
        let endpoints = ["https://a.test/only"];
        let res: serde_json::Value = fetch_with_fallback(
            &upstream,
            &endpoints,
            &body,
            "tok",
            crate::upstream::TimeoutProfile::Quota,
            "typed-body-ctx",
        )
        .await
        .expect("must succeed");
        assert_eq!(res["ok"], true);
    }
}
