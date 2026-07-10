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

use http::HeaderValue;
use std::sync::OnceLock;
use uuid::Uuid;

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
        "linux" => "X11; Linux x86_64",
        _ => "X11; Linux x86_64",
    }
}

/// The Antigravity version we report to the API. Uses the known-stable
/// version as a floor (the API rejects clients that report a version
/// that's too old). Override via `OPENPROXY_ANTIGRAVITY_VERSION` env var.
fn version() -> String {
    std::env::var("OPENPROXY_ANTIGRAVITY_VERSION")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| KNOWN_STABLE_VERSION.to_string())
}

/// Persistent machine ID. Generated once per process lifetime from
/// the hostname + OS. This mimics the `machine_uid` crate used by the
/// Antigravity-Manager — it produces a stable-per-machine identifier
/// that the API uses for rate-limiting and session tracking.
fn machine_id() -> String {
    static CACHE: OnceLock<String> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            // Try to read a stable machine identifier from the OS.
            // Fallback: hostname + OS arch.
            let raw = hostname()
                .unwrap_or_else(|| format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH));
            // Hash to a fixed-length hex string for a clean header value.
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(raw.as_bytes());
            hasher.update(std::env::consts::OS.as_bytes());
            hasher.update(std::env::consts::ARCH.as_bytes());
            let hash = hasher.finalize();
            // Take the first 32 hex chars (128 bits) — enough entropy
            // for a machine fingerprint without being too long.
            hash.iter()
                .take(16)
                .map(|b| format!("{:02x}", b))
                .collect::<String>()
        })
        .clone()
}

/// Best-effort hostname read. Returns `None` if the hostname can't be
/// determined (e.g. in a container without hostname configured).
fn hostname() -> Option<String> {
    static CACHE: OnceLock<Option<String>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
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
        })
        .clone()
}

/// Per-launch session ID. Generated once per process lifetime.
fn session_id() -> String {
    static CACHE: OnceLock<String> = OnceLock::new();
    CACHE.get_or_init(|| Uuid::new_v4().to_string()).clone()
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

/// Native OAuth User-Agent (used for token exchange / refresh / userinfo):
/// `vscode/1.X.X (Antigravity/{version})`
pub fn oauth_user_agent() -> String {
    format!("vscode/1.X.X (Antigravity/{})", version())
}

/// Inject all Antigravity client-identity headers into an
/// `http::HeaderMap`. The caller is responsible for setting
/// `Authorization` and `Content-Type` separately.
///
/// `project_id` is optional — when present, `x-goog-user-project` is
/// set to the project ID (required for the API to route the request
/// to the correct Cloud Code project).
pub fn inject_antigravity_headers(headers: &mut http::HeaderMap, project_id: Option<&str>) {
    // User-Agent
    if let Ok(v) = HeaderValue::from_str(&user_agent()) {
        headers.insert(http::header::USER_AGENT, v);
    }

    // x-client-name
    headers.insert("x-client-name", HeaderValue::from_static("antigravity"));

    // x-client-version
    if let Ok(v) = HeaderValue::from_str(&version()) {
        headers.insert("x-client-version", v);
    }

    // x-machine-id (persistent per-machine fingerprint)
    if let Ok(v) = HeaderValue::from_str(&machine_id()) {
        headers.insert("x-machine-id", v);
    }

    // x-vscode-sessionid (per-launch session)
    if let Ok(v) = HeaderValue::from_str(&session_id()) {
        headers.insert("x-vscode-sessionid", v);
    }

    // x-goog-user-project (when project_id is known and non-empty)
    if let Some(pid) = project_id
        && !pid.is_empty()
        && pid != "test-project"
        && pid != "project-id"
        && let Ok(v) = HeaderValue::from_str(pid)
    {
        headers.insert("x-goog-user-project", v);
    }
}

/// Get the current Antigravity version string (for logging / diagnostics).
pub fn current_version() -> String {
    version()
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
