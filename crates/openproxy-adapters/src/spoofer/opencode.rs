use super::{ClientSpoofer, DynamicHeaderOverrides, merge_header_refs, parse_env_extra_headers};
use crate::upstream::UpstreamClient;
use http::HeaderValue;
use rand::RngExt;
use std::sync::Arc;

pub const OPENCODE_UA: &str = "opencode/latest/2.0.22/cli";

/// Default OpenCode CLI version used for UA generation and validation.
pub const DEFAULT_OPENCODE_VERSION: &str = "2.0.22";

/// npm registry URL for the latest opencode CLI version.
/// Configurable via `OPENPROXY_OPENCODE_LATEST_VERSION_URL`.
pub const OPENCODE_LATEST_VERSION_URL: &str =
    "https://registry.npmjs.org/opencode-ai/latest";

static OPENCODE_OVERRIDES: DynamicHeaderOverrides = DynamicHeaderOverrides::new();

fn safe_env_value(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// Canonical npm registry URL for OpenCode CLI, configurable via
/// `OPENPROXY_OPENCODE_LATEST_VERSION_URL`.
pub fn opencode_latest_version_url() -> String {
    safe_env_value("OPENPROXY_OPENCODE_LATEST_VERSION_URL")
        .unwrap_or_else(|| OPENCODE_LATEST_VERSION_URL.to_string())
}

/// Set dynamic version override for OpenCode in memory at runtime without recompiling.
pub fn set_dynamic_opencode_version(ver: impl Into<String>) {
    OPENCODE_OVERRIDES.set_version(ver);
}

/// Resolve current OpenCode version string.
pub fn current_opencode_version() -> String {
    let ver = OPENCODE_OVERRIDES.current_version("OPENPROXY_OPENCODE_VERSION", "");
    if !ver.is_empty() {
        return ver;
    }
    safe_env_value("OPENCODE_CLIENT_VERSION")
        .unwrap_or_else(|| DEFAULT_OPENCODE_VERSION.to_string())
}

/// Dynamic OpenCode User-Agent (4-segment format matching the real CLI).
pub fn current_opencode_ua() -> String {
    if let Some(override_ua) = OPENCODE_OVERRIDES.current_extra_header("user-agent") {
        return override_ua;
    }
    if let Some(env_ua) = safe_env_value("OPENPROXY_OPENCODE_USER_AGENT")
        .or_else(|| safe_env_value("OPENCODE_USER_AGENT"))
    {
        return env_ua;
    }
    format!("opencode/latest/{}/cli", current_opencode_version())
}

/// Asynchronously queries the npm registry for the latest OpenCode CLI version
/// and updates the in-memory dynamic version state if changed.
pub async fn refresh_opencode_version(upstream_client: &Arc<UpstreamClient>) -> Option<String> {
    use crate::upstream::{CancellationToken, TimeoutProfile, UpstreamRequest};

    let url = opencode_latest_version_url();
    let mut req = UpstreamRequest::get(&url);
    if let Ok(accept) = HeaderValue::from_str("application/json") {
        req.headers.insert(http::header::ACCEPT, accept);
    }
    if let Ok(ua) = HeaderValue::from_str(&current_opencode_ua()) {
        req.headers.insert(http::header::USER_AGENT, ua);
    }

    let cancel = CancellationToken::new();
    let resp = upstream_client
        .call(req, TimeoutProfile::OAuth, cancel)
        .await
        .ok()?;

    if !resp.status.is_success() {
        return None;
    }

    let body = resp.collect().await.ok()?;
    let json: serde_json::Value = serde_json::from_slice(&body).ok()?;

    let raw_ver = json.get("version").and_then(|v| v.as_str())?;
    let clean_ver = raw_ver.trim().trim_start_matches('v').split('-').next()?.trim();

    if !clean_ver.is_empty() {
        set_dynamic_opencode_version(clean_ver);
        return Some(clean_ver.to_string());
    }

    None
}

/// Generate a random 32-hex-char W3C trace ID.
fn generate_trace_id() -> String {
    let mut rng = rand::rng();
    (0..32)
        .map(|_| format!("{:x}", rng.random_range(0..16)))
        .collect()
}

/// Generate a random 16-hex-char W3C span ID.
fn generate_span_id() -> String {
    let mut rng = rand::rng();
    (0..16)
        .map(|_| format!("{:x}", rng.random_range(0..16)))
        .collect()
}

/// Generate W3C `traceparent` header value: `00-{trace}-{span}-01`.
fn generate_traceparent() -> String {
    format!("00-{}-{}-01", generate_trace_id(), generate_span_id())
}

/// Generate B3 `b3` header value: `{trace}-{span}-1-{parent}`.
fn generate_b3() -> String {
    format!("{}-{}-1", generate_trace_id(), generate_span_id())
}

/// Set dynamic extra header override for OpenCode in memory at runtime without recompiling.
pub fn set_dynamic_opencode_extra_header(key: impl Into<String>, val: impl Into<String>) {
    OPENCODE_OVERRIDES.set_extra_header(key, val);
}

/// Reset dynamic in-memory overrides for OpenCode (useful for tests and cleanup).
pub fn reset_dynamic_opencode_overrides() {
    OPENCODE_OVERRIDES.reset();
}

#[cfg(any(test, feature = "test-utils"))]
pub static OPENCODE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

static OPENCODE_EXTRA_HEADERS: std::sync::LazyLock<Vec<(String, String)>> =
    std::sync::LazyLock::new(|| parse_env_extra_headers("OPENPROXY_OPENCODE_EXTRA_HEADERS"));

pub const OPENCODE_SPOOFING_HEADERS: &[(&str, &str)] = &[
    ("User-Agent", OPENCODE_UA),
    ("x-opencode-client", "cli"),
    ("x-opencode-project", "global"),
];

/// Preset for OpenCode client identity headers.
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

        let session_id = generate_session_id();
        list.push(("x-opencode-session".into(), session_id.clone()));
        list.push(("x-opencode-session-id".into(), session_id.clone()));
        list.push(("x-session-affinity".into(), session_id.clone()));
        list.push(("x-session-id".into(), session_id));
        list.push(("b3".into(), generate_b3()));
        list.push(("traceparent".into(), generate_traceparent()));

        merge_header_refs(&mut list, &*OPENCODE_EXTRA_HEADERS);
        OPENCODE_OVERRIDES.apply_to_list(&mut list);

        list
    }

    fn apply_to_header_map(&self, headers: &mut http::HeaderMap) {
        // 1. User-Agent: preserve valid downstream opencode, else upgrade to current_opencode_ua()
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
        OPENCODE_OVERRIDES.apply_to_header_map(headers);

        // 4. x-opencode-session + aliases: preserve valid, translate candidate, or generate new
        let session_header_name = http::header::HeaderName::from_static("x-opencode-session");
        let candidate_session = headers
            .get(&session_header_name)
            .or_else(|| headers.get("x-opencode-session-id"))
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

        if let Some(ref session_val) = resolved_session {
            // Set x-opencode-session
            if let Ok(val) = HeaderValue::try_from(session_val.as_str()) {
                headers.insert(session_header_name.clone(), val);
            }
            // Set aliases (same value)
            for alias in ["x-opencode-session-id", "x-session-affinity", "x-session-id"] {
                if let Ok(name) = http::header::HeaderName::try_from(alias)
                    && !headers.contains_key(&name)
                    && let Ok(val) = HeaderValue::try_from(session_val.as_str())
                {
                    headers.insert(name, val);
                }
            }
        }

        // 5. Remove x-opencode-request (not sent by the real CLI)
        headers.remove("x-opencode-request");

        // 6. W3C tracing headers
        if !headers.contains_key("b3")
            && let Ok(val) = HeaderValue::try_from(generate_b3().as_str())
        {
            headers.insert("b3", val);
        }
        if !headers.contains_key("traceparent")
            && let Ok(val) = HeaderValue::try_from(generate_traceparent().as_str())
        {
            headers.insert("traceparent", val);
        }
    }
}
