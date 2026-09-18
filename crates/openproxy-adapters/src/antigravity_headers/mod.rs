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
pub const KNOWN_STABLE_VERSION: &str = "4.3.0";
pub const KNOWN_STABLE_CHROME: &str = "132.0.6834.160";
pub const KNOWN_STABLE_ELECTRON: &str = "39.2.3";

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
    let raw = hostname().map_or_else(
        || format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        String::from,
    );
    let mut hasher = Sha256::new();
    hasher.update(raw.as_bytes());
    hasher.update(std::env::consts::OS.as_bytes());
    hasher.update(std::env::consts::ARCH.as_bytes());
    let hash = hasher.finalize();
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
pub fn build_bearer_header(
    token: &str,
) -> std::result::Result<HeaderValue, http::header::InvalidHeaderValue> {
    let mut buf = bytes::BytesMut::with_capacity(7 + token.len());
    buf.extend_from_slice(b"Bearer ");
    buf.extend_from_slice(token.as_bytes());
    http::HeaderValue::from_maybe_shared(buf.freeze())
}

/// Convenience: insert a Bearer `Authorization` header into a request.
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
/// auth + Antigravity headers.
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
mod tests;

#[cfg(test)]
mod adversarial_tests;
