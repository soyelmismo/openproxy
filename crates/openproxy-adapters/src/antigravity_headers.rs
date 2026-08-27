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

/// The full User-Agent string:
/// `Antigravity/{version} ({platform}) Chrome/{chrome} Electron/{electron}`
fn user_agent() -> String {
    format!(
        "Antigravity/{} ({}) Chrome/{} Electron/{}",
        version(),
        platform_info(),
        KNOWN_STABLE_CHROME,
        KNOWN_STABLE_ELECTRON,
    )
}

static HEADER_VAL_USER_AGENT: LazyLock<HeaderValue> =
    LazyLock::new(|| HeaderValue::from_str(&user_agent()).expect("user_agent must be valid ascii"));

/// Native OAuth User-Agent (used for token exchange / refresh / userinfo):
/// `vscode/1.X.X (Antigravity/{version})`
pub fn oauth_user_agent() -> String {
    format!("vscode/1.X.X (Antigravity/{})", version())
}

fn insert_header_str(headers: &mut http::HeaderMap, name: &'static str, val: &str) {
    if let Ok(v) = HeaderValue::from_str(val) {
        headers.insert(name, v);
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_agent_contains_antigravity_and_version() {
        let ua = user_agent();
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
}
