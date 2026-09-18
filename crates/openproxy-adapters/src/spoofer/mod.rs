//! Client spoofing traits and presets for upstream providers.
//!
//! Providers like Cline, OpenCode, and Antigravity require specific client
//! identity headers (User-Agent, machine fingerprint, editor metadata, etc.)
//! to accept requests. This module unifies spoofing into the [`ClientSpoofer`]
//! trait and provides standard presets.

use crate::upstream::UpstreamRequest;
use http::HeaderValue;
use rand::RngExt;

/// Trait for injecting client identity and spoofing headers into requests.
pub trait ClientSpoofer: Send + Sync {
    /// Return the full list of spoofed headers as `(name, value)` pairs.
    fn headers(&self) -> Vec<(String, String)>;

    /// Apply the spoofed headers to an [`UpstreamRequest`].
    fn apply_to_request(&self, req: &mut UpstreamRequest) {
        self.apply_to_header_map(&mut req.headers);
    }

    /// Apply the spoofed headers to an [`http::HeaderMap`].
    fn apply_to_header_map(&self, headers: &mut http::HeaderMap) {
        for (k, v) in self.headers() {
            if let Ok(name) = http::header::HeaderName::try_from(k.as_str())
                && let Ok(val) = HeaderValue::try_from(v.as_str())
            {
                headers.insert(name, val);
            }
        }
    }
}

// =====================================================================
// Cline Preset
// =====================================================================

pub const CLINE_SPOOFING_HEADERS: &[(&str, &str)] = &[
    ("http-referer", "https://cline.bot"),
    ("x-title", "Cline"),
    ("user-agent", "Cline/4.1.3"),
    ("x-is-multiroot", "false"),
    ("x-client-type", "VSCode Extension"),
    ("x-client-version", "4.1.3"),
    ("x-platform", "Visual Studio Code"),
    ("x-platform-version", "1.96.0"),
    ("x-core-version", "4.1.3"),
];

pub const DEFAULT_CLINE_VERSION: &str = "4.1.3";

static CLINE_DYNAMIC_VERSION: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);
static CLINE_DYNAMIC_EXTRA_HEADERS: std::sync::RwLock<std::collections::BTreeMap<String, String>> =
    std::sync::RwLock::new(std::collections::BTreeMap::new());

/// Current dynamic version of Cline.
pub fn current_cline_version() -> String {
    if let Ok(lock) = CLINE_DYNAMIC_VERSION.read()
        && let Some(ref ver) = *lock
    {
        return ver.clone();
    }
    if let Ok(env_ver) = std::env::var("OPENPROXY_CLINE_VERSION")
        && !env_ver.trim().is_empty()
    {
        return env_ver.trim().to_string();
    }
    DEFAULT_CLINE_VERSION.to_string()
}

/// Current dynamic User-Agent of Cline.
pub fn current_cline_ua() -> String {
    format!("Cline/{}", current_cline_version())
}

/// Set dynamic version override for Cline in memory at runtime without recompiling.
pub fn set_dynamic_cline_version(ver: impl Into<String>) {
    if let Ok(mut lock) = CLINE_DYNAMIC_VERSION.write() {
        *lock = Some(ver.into());
    }
}

/// Set dynamic extra header override for Cline in memory at runtime without recompiling.
pub fn set_dynamic_cline_extra_header(key: impl Into<String>, val: impl Into<String>) {
    if let Ok(mut lock) = CLINE_DYNAMIC_EXTRA_HEADERS.write() {
        lock.insert(key.into(), val.into());
    }
}

/// Reset dynamic in-memory overrides for Cline (useful for tests and cleanup).
pub fn reset_dynamic_cline_overrides() {
    if let Ok(mut lock) = CLINE_DYNAMIC_VERSION.write() {
        *lock = None;
    }
    if let Ok(mut lock) = CLINE_DYNAMIC_EXTRA_HEADERS.write() {
        lock.clear();
    }
}

#[cfg(test)]
pub(crate) static CLINE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

static CLINE_EXTRA_HEADERS: std::sync::LazyLock<Vec<(String, String)>> = std::sync::LazyLock::new(|| {
    let Ok(env_str) = std::env::var("OPENPROXY_CLINE_EXTRA_HEADERS") else {
        return Vec::new();
    };
    let Ok(map) = serde_json::from_str::<std::collections::BTreeMap<String, String>>(&env_str) else {
        return Vec::new();
    };
    map.into_iter().collect()
});

/// Preset for Cline client identity headers.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClineSpoofer;

impl ClientSpoofer for ClineSpoofer {
    fn headers(&self) -> Vec<(String, String)> {
        let cur_ver = current_cline_version();
        let cur_ua = current_cline_ua();
        let mut list: Vec<(String, String)> = CLINE_SPOOFING_HEADERS
            .iter()
            .map(|(k, v)| {
                if *k == "user-agent" {
                    (k.to_string(), cur_ua.clone())
                } else if *k == "x-client-version" || *k == "x-core-version" {
                    (k.to_string(), cur_ver.clone())
                } else {
                    (k.to_string(), v.to_string())
                }
            })
            .collect();

        for (k, v) in CLINE_EXTRA_HEADERS.iter() {
            if let Some(pos) = list.iter().position(|(hk, _)| hk.eq_ignore_ascii_case(k)) {
                list[pos].1 = v.clone();
            } else {
                list.push((k.clone(), v.clone()));
            }
        }

        if let Ok(lock) = CLINE_DYNAMIC_EXTRA_HEADERS.read() {
            for (k, v) in lock.iter() {
                if let Some(pos) = list.iter().position(|(hk, _)| hk.eq_ignore_ascii_case(k)) {
                    list[pos].1 = v.clone();
                } else {
                    list.push((k.clone(), v.clone()));
                }
            }
        }

        list
    }

    fn apply_to_header_map(&self, headers: &mut http::HeaderMap) {
        for (k, v) in self.headers() {
            if let Ok(name) = http::header::HeaderName::try_from(k.as_str())
                && let Ok(val) = HeaderValue::try_from(v.as_str())
            {
                headers.insert(name, val);
            }
        }
    }
}

// =====================================================================
// OpenCode Preset
// =====================================================================

pub const OPENCODE_UA: &str = "opencode/1.19.0";

static OPENCODE_DYNAMIC_VERSION: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);

/// Set dynamic version override for OpenCode in memory at runtime without recompiling.
pub fn set_dynamic_opencode_version(ver: impl Into<String>) {
    if let Ok(mut lock) = OPENCODE_DYNAMIC_VERSION.write() {
        *lock = Some(ver.into());
    }
}

/// Resolve current OpenCode version string.
pub fn current_opencode_version() -> String {
    if let Ok(lock) = OPENCODE_DYNAMIC_VERSION.read()
        && let Some(ref ver) = *lock
    {
        return ver.clone();
    }
    if let Ok(env_ver) = std::env::var("OPENPROXY_OPENCODE_VERSION")
        && !env_ver.is_empty()
    {
        return env_ver;
    }
    "1.19.0".to_string()
}

/// Dynamic OpenCode User-Agent.
pub fn current_opencode_ua() -> String {
    format!("opencode/{}", current_opencode_version())
}

static OPENCODE_DYNAMIC_EXTRA_HEADERS: std::sync::RwLock<std::collections::BTreeMap<String, String>> =
    std::sync::RwLock::new(std::collections::BTreeMap::new());

/// Set dynamic extra header override for OpenCode in memory at runtime without recompiling.
pub fn set_dynamic_opencode_extra_header(key: impl Into<String>, val: impl Into<String>) {
    if let Ok(mut lock) = OPENCODE_DYNAMIC_EXTRA_HEADERS.write() {
        lock.insert(key.into(), val.into());
    }
}

/// Reset dynamic in-memory overrides for OpenCode (useful for tests and cleanup).
pub fn reset_dynamic_opencode_overrides() {
    if let Ok(mut lock) = OPENCODE_DYNAMIC_VERSION.write() {
        *lock = None;
    }
    if let Ok(mut lock) = OPENCODE_DYNAMIC_EXTRA_HEADERS.write() {
        lock.clear();
    }
}

#[cfg(test)]
pub(crate) static OPENCODE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

static OPENCODE_EXTRA_HEADERS: std::sync::LazyLock<Vec<(String, String)>> = std::sync::LazyLock::new(|| {
    let Ok(env_str) = std::env::var("OPENPROXY_OPENCODE_EXTRA_HEADERS") else {
        return Vec::new();
    };
    let Ok(map) = serde_json::from_str::<std::collections::BTreeMap<String, String>>(&env_str) else {
        return Vec::new();
    };
    map.into_iter().collect()
});

pub const OPENCODE_SPOOFING_HEADERS: &[(&str, &str)] = &[
    ("User-Agent", OPENCODE_UA),
    ("x-opencode-client", "cli"),
    ("x-opencode-project", "global"),
];

/// Preset for OpenCode client identity headers.
///
/// Three headers are static (`OPENCODE_SPOOFING_HEADERS`): `User-Agent`,
/// `x-opencode-client`, `x-opencode-project`.
///
/// Two additional headers are generated per request following OpenCode's
/// canonical descending/ascending identifier formats to satisfy upstream
/// Console free-tier verification (`/^ses_[0-9a-f]{12}[0-9A-Za-z]{14}$/`):
/// - `x-opencode-session`  — `"ses_" + 12 hex + 14 Base62 chars (descending)`
/// - `x-opencode-request`  — `"msg_" + 12 hex + 14 Base62 chars (ascending)`
///
/// See `other_projects_examples/opencode/packages/schema/src/identifier.ts`,
/// `other_projects_examples/opencode/packages/schema/src/session-id.ts`,
/// and `other_projects_examples/opencode/packages/opencode/src/session/llm/request.ts`.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenCodeSpoofer;

/// Base62 alphabet used to generate opencode session/request IDs.
const OPENCODE_B62: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// Length of the random suffix after the prefix (`ses_` / `msg_`).
pub const OPENCODE_ID_SUFFIX_LEN: usize = 26;

/// Total length of a canonical OpenCode session/request identifier (4-char prefix + 26-char suffix).
pub const OPENCODE_ID_TOTAL_LEN: usize = 30;

/// Global tracker for timestamp + counter matching OpenCode's `identifier.ts`.
static OPENCODE_ID_STATE: std::sync::Mutex<(u64, u64)> = std::sync::Mutex::new((0, 0));

fn next_canonical_id_parts(timestamp: Option<u64>) -> (u64, u64) {
    let now = timestamp.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    });
    let mut guard = OPENCODE_ID_STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.0 != now {
        guard.0 = now;
        guard.1 = 0;
    }
    guard.1 += 1;
    (now, guard.1)
}

/// Create a 26-character canonical identifier (12 hex digits + 14 Base62 characters).
///
/// If `descending` is true, the timestamp-counter is inverted (`!current`), exactly
/// matching OpenCode's `descending()` implementation for sessions.
/// If false, it uses `current`, matching OpenCode's `ascending()` for messages/requests.
pub fn create_canonical_id(
    descending: bool,
    timestamp: Option<u64>,
    rng: &mut impl rand::Rng,
) -> String {
    let (ts, counter) = next_canonical_id_parts(timestamp);
    let current = ((ts as u128) * 0x1000) + (counter as u128);
    let value = if descending { !current } else { current };

    let mut out = String::with_capacity(OPENCODE_ID_SUFFIX_LEN);
    use std::fmt::Write;
    for i in 0..6 {
        let shift = 40 - 8 * i;
        let byte = ((value >> shift) & 0xff) as u8;
        let _ = write!(out, "{byte:02x}");
    }

    for _ in 0..14 {
        let idx = rng.random_range(0..OPENCODE_B62.len());
        out.push(OPENCODE_B62[idx] as char);
    }

    out
}

/// Generate a canonical OpenCode session identifier (`ses_` + 12 lowercase hex + 14 Base62 chars).
pub fn generate_session_id() -> String {
    let mut rng = rand::rng();
    let suffix = create_canonical_id(true, None, &mut rng);
    format!("ses_{suffix}")
}

/// Generate a canonical OpenCode request identifier (`msg_` + 12 lowercase hex + 14 Base62 chars).
pub fn generate_request_id() -> String {
    let mut rng = rand::rng();
    let suffix = create_canonical_id(false, None, &mut rng);
    format!("msg_{suffix}")
}

/// Validate whether a session identifier matches OpenCode's canonical format:
/// `/^ses_[0-9a-f]{12}[0-9A-Za-z]{14}$/` (30 chars total).
pub fn is_valid_opencode_session_id(id: &str) -> bool {
    let trimmed = id.trim();
    if trimmed.len() != OPENCODE_ID_TOTAL_LEN || !trimmed.starts_with("ses_") {
        return false;
    }
    let suffix = &trimmed[4..];
    let (hex_part, b62_part) = suffix.split_at(12);
    hex_part
        .chars()
        .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        && b62_part.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Validate whether a request identifier matches OpenCode's canonical format:
/// `/^msg_[0-9a-f]{12}[0-9A-Za-z]{14}$/` (30 chars total).
pub fn is_valid_opencode_request_id(id: &str) -> bool {
    let trimmed = id.trim();
    if trimmed.len() != OPENCODE_ID_TOTAL_LEN || !trimmed.starts_with("msg_") {
        return false;
    }
    let suffix = &trimmed[4..];
    let (hex_part, b62_part) = suffix.split_at(12);
    hex_part
        .chars()
        .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        && b62_part.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Deterministically translate an arbitrary session ID into a valid OpenCode canonical session ID.
///
/// If already valid, returns it as-is. Otherwise hashes it via SHA-256 to ensure
/// prompt caching and session affinity work consistently across turns.
pub fn translate_session_id(session_id: &str, client_tool: Option<&str>) -> String {
    let trimmed = session_id.trim();
    if is_valid_opencode_session_id(trimmed) {
        return trimmed.to_string();
    }

    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"opencode\0");
    hasher.update(client_tool.unwrap_or("generic").as_bytes());
    hasher.update(b"\0");
    hasher.update(trimmed.as_bytes());
    let digest = hasher.finalize();

    let mut out = String::with_capacity(OPENCODE_ID_TOTAL_LEN);
    out.push_str("ses_");
    use std::fmt::Write;
    for &b in &digest[..6] {
        let _ = write!(out, "{b:02x}");
    }
    for &b in &digest[6..20] {
        let idx = (b as usize) % OPENCODE_B62.len();
        out.push(OPENCODE_B62[idx] as char);
    }

    out
}

/// Check if User-Agent specifies an OpenCode version >= 1.17.0.
pub fn has_valid_opencode_version(ua: &str) -> bool {
    let lower = ua.to_ascii_lowercase();
    let Some(idx) = lower.find("opencode/") else {
        return false;
    };
    let rest = &ua[idx + "opencode/".len()..];
    let mut parts = rest.split('.');
    let Some(major_str) = parts.next() else {
        return false;
    };
    let Ok(major) = major_str.trim().parse::<u32>() else {
        return false;
    };
    let Some(minor_str) = parts.next() else {
        return false;
    };
    let minor_digits: String = minor_str
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let Ok(minor) = minor_digits.parse::<u32>() else {
        return false;
    };

    major > 1 || (major == 1 && minor >= 17)
}

impl ClientSpoofer for OpenCodeSpoofer {
    fn headers(&self) -> Vec<(String, String)> {
        let mut list: Vec<(String, String)> = OPENCODE_SPOOFING_HEADERS
            .iter()
            .map(|(k, v)| {
                if *k == "User-Agent" {
                    (k.to_string(), current_opencode_ua())
                } else {
                    (k.to_string(), v.to_string())
                }
            })
            .collect();

        list.push(("x-opencode-session".into(), generate_session_id()));
        list.push(("x-opencode-request".into(), generate_request_id()));

        for (k, v) in OPENCODE_EXTRA_HEADERS.iter() {
            if !list.iter().any(|(hk, _)| hk.eq_ignore_ascii_case(k)) {
                list.push((k.clone(), v.clone()));
            }
        }

        if let Ok(lock) = OPENCODE_DYNAMIC_EXTRA_HEADERS.read() {
            for (k, v) in lock.iter() {
                if let Some(pos) = list.iter().position(|(hk, _)| hk.eq_ignore_ascii_case(k)) {
                    list[pos].1 = v.clone();
                } else {
                    list.push((k.clone(), v.clone()));
                }
            }
        }

        list
    }

    fn apply_to_header_map(&self, headers: &mut http::HeaderMap) {
        // 1. User-Agent: preserve valid downstream opencode >= 1.17, else upgrade to current_opencode_ua()
        let preserve_ua = headers
            .get(http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .is_some_and(has_valid_opencode_version);

        let cur_ua = current_opencode_ua();
        if !preserve_ua && let Ok(val) = HeaderValue::try_from(cur_ua.as_str()) {
            headers.insert(http::header::USER_AGENT, val);
        }

        // 2. Static identity headers
        if let Ok(name) = http::header::HeaderName::try_from("x-opencode-client")
            && !headers.contains_key(&name)
            && let Ok(val) = HeaderValue::try_from("cli")
        {
            headers.insert(name, val);
        }
        if let Ok(name) = http::header::HeaderName::try_from("x-opencode-project")
            && !headers.contains_key(&name)
            && let Ok(val) = HeaderValue::try_from("global")
        {
            headers.insert(name, val);
        }

        // 3. Extra headers (static + dynamic)
        for (k, v) in OPENCODE_EXTRA_HEADERS.iter() {
            if let Ok(name) = http::header::HeaderName::try_from(k.as_str())
                && !headers.contains_key(&name)
                && let Ok(val) = HeaderValue::try_from(v.as_str())
            {
                headers.insert(name, val);
            }
        }
        if let Ok(lock) = OPENCODE_DYNAMIC_EXTRA_HEADERS.read() {
            for (k, v) in lock.iter() {
                if let Ok(name) = http::header::HeaderName::try_from(k.as_str())
                    && let Ok(val) = HeaderValue::try_from(v.as_str())
                {
                    headers.insert(name, val);
                }
            }
        }

        // 4. x-opencode-session: preserve valid, translate candidate, or generate new
        let session_header_name = http::header::HeaderName::from_static("x-opencode-session");
        let candidate_session = headers
            .get(&session_header_name)
            .or_else(|| headers.get("x-session-affinity"))
            .or_else(|| headers.get("x-session-id"));

        let resolved_session = if let Some(existing) = candidate_session {
            let existing_str = existing.to_str().unwrap_or("");
            if is_valid_opencode_session_id(existing_str) {
                if headers.contains_key(&session_header_name) {
                    None
                } else {
                    Some(existing_str.to_string())
                }
            } else {
                Some(translate_session_id(existing_str, None))
            }
        } else {
            Some(generate_session_id())
        };

        if let Some(session_val) = resolved_session
            && let Ok(val) = HeaderValue::try_from(session_val)
        {
            headers.insert(session_header_name, val);
        }

        // 5. x-opencode-request: preserve valid, or generate new
        let request_header_name = http::header::HeaderName::from_static("x-opencode-request");
        let need_request_id = headers
            .get(&request_header_name)
            .and_then(|v| v.to_str().ok())
            .is_none_or(|s| !is_valid_opencode_request_id(s));

        if need_request_id && let Ok(val) = HeaderValue::try_from(generate_request_id()) {
            headers.insert(request_header_name, val);
        }
    }
}

// =====================================================================
// Antigravity Preset
// =====================================================================

/// Preset for Google Antigravity (Cloud Code) client identity headers.
#[derive(Debug, Clone, Default)]
pub struct AntigravitySpoofer {
    pub project_id: Option<String>,
}

impl AntigravitySpoofer {
    pub fn new() -> Self {
        Self { project_id: None }
    }

    pub fn with_project(project_id: impl Into<String>) -> Self {
        Self {
            project_id: Some(project_id.into()),
        }
    }
}

impl ClientSpoofer for AntigravitySpoofer {
    fn headers(&self) -> Vec<(String, String)> {
        let mut hm = http::HeaderMap::new();
        self.apply_to_header_map(&mut hm);
        hm.into_iter()
            .filter_map(|(k, v)| {
                k.map(|name| {
                    (
                        name.as_str().to_string(),
                        v.to_str().unwrap_or("").to_string(),
                    )
                })
            })
            .collect()
    }

    fn apply_to_header_map(&self, headers: &mut http::HeaderMap) {
        crate::antigravity_headers::inject_antigravity_headers(headers, self.project_id.as_deref());
    }
}

#[cfg(test)]
mod tests;
