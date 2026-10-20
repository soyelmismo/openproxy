//! Antigravity (Google Cloud Code) client identity headers.
//!
//! cloudcode-pa.googleapis.com rejects requests that do not carry the
//! Antigravity client-identity headers, so the executor, quota fetch and OAuth
//! flow all build them here.
//!
//! From the Antigravity-Manager reference implementation:
//! - `User-Agent: Antigravity/{version} ({platform}) Chrome/{chrome} Electron/{electron}`
//! - `x-client-name: antigravity`
//! - `x-client-version: {version}`
//! - `x-machine-id: {persistent machine UID}`
//! - `x-vscode-sessionid: {per-launch UUID}`
//! - `x-goog-user-project: {project_id}` (when project_id is known)
//!
//! `x-machine-id` hashes hostname + OS once per process, since the API uses it
//! for rate limiting and session tracking. `x-vscode-sessionid` is a UUID
//! minted once per process launch.

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

/// Floor for the version Google's API accepts. Taken from the
/// Antigravity-Manager reference.
pub const KNOWN_STABLE_VERSION: &str = "4.3.0";
pub const KNOWN_STABLE_CHROME: &str = "132.0.6834.160";
pub const KNOWN_STABLE_ELECTRON: &str = "39.2.3";

fn platform_info() -> &'static str {
    match std::env::consts::OS {
        "macos" => "Macintosh; Intel Mac OS X 10_15_7",
        "windows" => "Windows NT 10.0; Win64; x64",
        _ => "X11; Linux x86_64",
    }
}

static DYNAMIC_VERSION: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);

pub fn set_dynamic_version(ver: impl Into<String>) {
    if let Ok(mut lock) = DYNAMIC_VERSION.write() {
        *lock = Some(ver.into());
    }
}

/// In-memory override > `OPENPROXY_ANTIGRAVITY_VERSION` > [`KNOWN_STABLE_VERSION`].
pub fn current_version() -> String {
    if let Ok(lock) = DYNAMIC_VERSION.read()
        && let Some(ref ver) = *lock
    {
        return ver.clone();
    }
    if let Ok(env_ver) = std::env::var("OPENPROXY_ANTIGRAVITY_VERSION")
        && !env_ver.is_empty()
    {
        return env_ver;
    }
    KNOWN_STABLE_VERSION.to_string()
}

static DYNAMIC_EXTRA_HEADERS: std::sync::RwLock<std::collections::BTreeMap<String, String>> =
    std::sync::RwLock::new(std::collections::BTreeMap::new());

pub fn set_dynamic_extra_header(key: impl Into<String>, val: impl Into<String>) {
    if let Ok(mut lock) = DYNAMIC_EXTRA_HEADERS.write() {
        lock.insert(key.into(), val.into());
    }
}

pub fn reset_dynamic_overrides() {
    if let Ok(mut lock) = DYNAMIC_VERSION.write() {
        *lock = None;
    }
    if let Ok(mut lock) = DYNAMIC_EXTRA_HEADERS.write() {
        lock.clear();
    }
}

#[cfg(test)]
pub(crate) static ANTIGRAVITY_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

static EXTRA_HEADERS: LazyLock<Vec<(HeaderName, HeaderValue)>> = LazyLock::new(|| {
    let Ok(env_str) = std::env::var("OPENPROXY_ANTIGRAVITY_EXTRA_HEADERS") else {
        return Vec::new();
    };
    let Ok(map) = serde_json::from_str::<std::collections::BTreeMap<String, String>>(&env_str)
    else {
        return Vec::new();
    };
    map.into_iter()
        .filter_map(|(k, v)| {
            let name = HeaderName::from_bytes(k.as_bytes()).ok()?;
            let val = HeaderValue::from_str(&v).ok()?;
            Some((name, val))
        })
        .collect()
});

/// SHA-256 of hostname + OS, minted once per process. Stands in for the
/// `machine_uid` crate, whose stable-per-machine id the API uses for rate
/// limiting and session tracking.
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
    LazyLock::new(|| match HeaderValue::from_str(&MACHINE_ID) {
        Ok(v) => v,
        Err(_) => HeaderValue::from_static("openproxy"),
    });

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

/// `None` when the hostname is undetermined, e.g. a container without one.
fn hostname() -> Option<&'static str> {
    HOSTNAME.as_deref()
}

static SESSION_ID: LazyLock<String> = LazyLock::new(|| Uuid::new_v4().to_string());

static HEADER_VAL_SESSION_ID: LazyLock<HeaderValue> =
    LazyLock::new(|| match HeaderValue::from_str(&SESSION_ID) {
        Ok(v) => v,
        Err(_) => HeaderValue::from_static("session"),
    });

#[cfg(test)]
fn session_id() -> &'static str {
    &SESSION_ID
}

pub fn build_user_agent(ver: &str) -> HeaderValue {
    let mut bytes = bytes::BytesMut::with_capacity(128);
    bytes.extend_from_slice(b"Antigravity/");
    bytes.extend_from_slice(ver.as_bytes());
    bytes.extend_from_slice(b" (");
    bytes.extend_from_slice(platform_info().as_bytes());
    bytes.extend_from_slice(b") Chrome/");
    bytes.extend_from_slice(KNOWN_STABLE_CHROME.as_bytes());
    bytes.extend_from_slice(b" Electron/");
    bytes.extend_from_slice(KNOWN_STABLE_ELECTRON.as_bytes());
    HeaderValue::from_maybe_shared(bytes.freeze())
        .unwrap_or_else(|_| HeaderValue::from_static("Antigravity/4.3.0"))
}

pub fn user_agent() -> HeaderValue {
    build_user_agent(&current_version())
}

/// Hub fingerprint pinned together to Antigravity-Manager 9f8e4f24.
/// This is the language-service identity, NOT the IDE/OAuth version.
/// Never combine a dynamically detected IDE version with this Hub changelist.
pub const OFFICIAL_HUB_VERSION: &str = "2.23.1";
pub const OFFICIAL_HUB_CL: &str = "998881661";

pub fn hub_user_agent() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    format!(
        "antigravity/hub/{OFFICIAL_HUB_VERSION} (aidev_client; os_type={os}; arch={arch}; cl={OFFICIAL_HUB_CL})"
    )
}

/// Last-mile generation headers, after client/account overrides. OAuth and
/// metadata keep their existing IDE contract. Content requests retain their
/// project in the JSON envelope, never in Google's quota-project header.
pub fn finalize_antigravity_content_headers(headers: &mut Vec<(String, String)>, model: &str) {
    headers.retain(|(key, _)| {
        let key = key.to_ascii_lowercase();
        !matches!(
            key.as_str(),
            "user-agent"
                | "x-session-id"
                | "x-client-name"
                | "x-client-version"
                | "x-machine-id"
                | "x-vscode-sessionid"
                | "x-goog-user-project"
                | "x-goog-api-client"
                | "x-anthropic-billing-header"
                | "anthropic-dangerous-direct-browser-access"
        ) && !key.starts_with("x-stainless-")
            && !key.starts_with("x-claude-code-")
            && !key.starts_with("x-jeikcode")
            && !key.starts_with("cc_")
    });
    headers.push(("User-Agent".into(), hub_user_agent()));
    if model.to_ascii_lowercase().contains("claude")
        && !headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("anthropic-beta"))
    {
        headers.push(("anthropic-beta".into(), "claude-code-20250219".into()));
    }
}

/// `vscode/1.X.X (Antigravity/{version})`, the User-Agent Google's native
/// OAuth endpoints expect for token exchange, refresh and userinfo.
pub fn oauth_user_agent() -> String {
    let mut out = String::with_capacity(64);
    let _ = write!(out, "vscode/1.X.X (Antigravity/{})", current_version());
    out
}

fn is_valid_project_id(pid: &str) -> bool {
    !pid.is_empty() && pid != "test-project" && pid != "project-id"
}

/// Inject the client-identity headers. The caller sets `Authorization` and
/// `Content-Type`.
///
/// `x-goog-user-project` is set only when `project_id` is present, since the
/// API needs it to route the request to the right Cloud Code project.
pub fn inject_antigravity_headers(headers: &mut http::HeaderMap, project_id: Option<&str>) {
    let ver = current_version();
    headers.insert(http::header::USER_AGENT, build_user_agent(&ver));
    headers.insert(
        &HEADER_X_CLIENT_NAME,
        HeaderValue::from_static("antigravity"),
    );
    if let Ok(val) = HeaderValue::from_str(&ver) {
        headers.insert(&HEADER_X_CLIENT_VERSION, val);
    }
    headers.insert(&HEADER_X_MACHINE_ID, HEADER_VAL_MACHINE_ID.clone());
    headers.insert(&HEADER_X_VSCODE_SESSIONID, HEADER_VAL_SESSION_ID.clone());

    if let Some(pid) = project_id.filter(|p| is_valid_project_id(p))
        && let Ok(v) = HeaderValue::from_str(pid)
    {
        headers.insert(&HEADER_X_GOOG_USER_PROJECT, v);
    }

    for (k, v) in EXTRA_HEADERS.iter() {
        headers.insert(k.clone(), v.clone());
    }

    if let Ok(lock) = DYNAMIC_EXTRA_HEADERS.read() {
        for (k, v) in lock.iter() {
            if let Ok(name) = HeaderName::from_bytes(k.as_bytes())
                && let Ok(val) = HeaderValue::from_str(v)
            {
                headers.insert(name, val);
            }
        }
    }
}

pub fn build_bearer_header(
    token: &str,
) -> std::result::Result<HeaderValue, http::header::InvalidHeaderValue> {
    let mut buf = bytes::BytesMut::with_capacity(7 + token.len());
    buf.extend_from_slice(b"Bearer ");
    buf.extend_from_slice(token.as_bytes());
    let mut value = http::HeaderValue::from_maybe_shared(buf.freeze())?;
    value.set_sensitive(true);
    Ok(value)
}

pub fn insert_bearer(
    req: &mut crate::upstream::UpstreamRequest,
    token: &str,
) -> std::result::Result<(), http::header::InvalidHeaderValue> {
    req.headers
        .insert(http::header::AUTHORIZATION, build_bearer_header(token)?);
    Ok(())
}

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
