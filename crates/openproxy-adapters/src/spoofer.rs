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

macro_rules! impl_static_spoofer {
    ($struct_name:ident, $headers:ident) => {
        impl ClientSpoofer for $struct_name {
            fn headers(&self) -> Vec<(String, String)> {
                $headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect()
            }

            fn apply_to_header_map(&self, headers: &mut http::HeaderMap) {
                for &(k, v) in $headers {
                    if let Ok(name) = http::header::HeaderName::try_from(k)
                        && let Ok(val) = HeaderValue::try_from(v)
                    {
                        headers.insert(name, val);
                    }
                }
            }
        }
    };
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

/// Preset for Cline client identity headers.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClineSpoofer;

impl_static_spoofer!(ClineSpoofer, CLINE_SPOOFING_HEADERS);

// =====================================================================
// OpenCode Preset
// =====================================================================

pub const OPENCODE_UA: &str = "opencode/1.18.31";

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
pub fn create_canonical_id(descending: bool, timestamp: Option<u64>, rng: &mut impl rand::Rng) -> String {
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
    let minor_digits: String = minor_str.chars().take_while(|c| c.is_ascii_digit()).collect();
    let Ok(minor) = minor_digits.parse::<u32>() else {
        return false;
    };

    major > 1 || (major == 1 && minor >= 17)
}

impl ClientSpoofer for OpenCodeSpoofer {
    fn headers(&self) -> Vec<(String, String)> {
        let mut list: Vec<(String, String)> = OPENCODE_SPOOFING_HEADERS
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();

        list.push(("x-opencode-session".into(), generate_session_id()));
        list.push(("x-opencode-request".into(), generate_request_id()));

        list
    }

    fn apply_to_header_map(&self, headers: &mut http::HeaderMap) {
        // 1. User-Agent: preserve valid downstream opencode >= 1.17, else upgrade to OPENCODE_UA
        let preserve_ua = headers
            .get(http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .is_some_and(has_valid_opencode_version);

        if !preserve_ua
            && let Ok(val) = HeaderValue::try_from(OPENCODE_UA)
        {
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

        // 3. x-opencode-session: preserve valid, translate candidate, or generate new
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

        // 4. x-opencode-request: preserve valid, or generate new
        let request_header_name = http::header::HeaderName::from_static("x-opencode-request");
        let need_request_id = headers
            .get(&request_header_name)
            .and_then(|v| v.to_str().ok())
            .is_none_or(|s| !is_valid_opencode_request_id(s));

        if need_request_id
            && let Ok(val) = HeaderValue::try_from(generate_request_id())
        {
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
mod tests {
    use super::*;

    #[test]
    fn test_cline_spoofer() {
        let spoofer = ClineSpoofer;
        let mut req = UpstreamRequest::get("https://dummy.url");
        spoofer.apply_to_request(&mut req);

        for &(k, v) in CLINE_SPOOFING_HEADERS {
            let header_val = req.headers.get(k).expect("header missing");
            assert_eq!(header_val, HeaderValue::from_str(v).unwrap());
        }
    }

    fn assert_opencode_id(prefix: &str, id: &str) {
        assert!(id.starts_with(prefix), "expected prefix {prefix}, got {id}");
        assert_eq!(
            id.len(),
            OPENCODE_ID_TOTAL_LEN,
            "total id length mismatch: {id}"
        );
        let suffix = &id[prefix.len()..];
        assert_eq!(
            suffix.len(),
            OPENCODE_ID_SUFFIX_LEN,
            "id suffix length mismatch"
        );
        let (hex_part, b62_part) = suffix.split_at(12);
        assert!(
            hex_part.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "first 12 chars of suffix must be lowercase hex: {id}"
        );
        assert!(
            b62_part.chars().all(|c| c.is_ascii_alphanumeric()),
            "trailing 14 chars must be base62 alphanumeric: {id}"
        );
        if prefix == "ses_" {
            assert!(is_valid_opencode_session_id(id), "invalid session id: {id}");
        } else if prefix == "msg_" {
            assert!(is_valid_opencode_request_id(id), "invalid request id: {id}");
        }
    }

    #[test]
    fn test_opencode_spoofer() {
        let spoofer = OpenCodeSpoofer;
        let headers = spoofer.headers();

        // Static headers preserved as-is.
        for &(k, v) in OPENCODE_SPOOFING_HEADERS {
            assert!(
                headers.iter().any(|(name, val)| name == k && val == v),
                "missing static header {k}={v}"
            );
        }

        // Dynamic session/request IDs follow ses_/msg_ + 12 hex + 14 Base62 chars.
        let session_id = headers
            .iter()
            .find(|(k, _)| k == "x-opencode-session")
            .map(|(_, v)| v.as_str())
            .expect("missing x-opencode-session");
        assert_opencode_id("ses_", session_id);

        let request_id = headers
            .iter()
            .find(|(k, _)| k == "x-opencode-request")
            .map(|(_, v)| v.as_str())
            .expect("missing x-opencode-request");
        assert_opencode_id("msg_", request_id);

        // Client flag is fixed to "cli".
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "x-opencode-client" && v == "cli")
        );

        // Two successive calls produce different session IDs.
        let second_headers = spoofer.headers();
        let second_session = second_headers
            .iter()
            .find(|(k, _)| k == "x-opencode-session")
            .map(|(_, v)| v.as_str())
            .expect("missing x-opencode-session");
        assert_ne!(session_id, second_session);
    }

    #[test]
    fn test_opencode_spoofer_apply_to_request() {
        let spoofer = OpenCodeSpoofer;
        let mut req = UpstreamRequest::get("https://dummy.url");
        spoofer.apply_to_request(&mut req);

        let session_id = req
            .headers
            .get("x-opencode-session")
            .expect("missing dynamic x-opencode-session");
        assert_opencode_id("ses_", session_id.to_str().expect("session id not ascii"));

        let request_id = req
            .headers
            .get("x-opencode-request")
            .expect("missing dynamic x-opencode-request");
        assert_opencode_id("msg_", request_id.to_str().expect("request id not ascii"));

        assert_eq!(
            req.headers.get("x-opencode-client").unwrap(),
            HeaderValue::from_str("cli").unwrap()
        );
        assert_eq!(
            req.headers.get("x-opencode-project").unwrap(),
            HeaderValue::from_str("global").unwrap()
        );
        assert_eq!(
            req.headers.get("User-Agent").unwrap(),
            HeaderValue::from_str(OPENCODE_UA).unwrap()
        );
        assert!(req.headers.get("opencode-version").is_none());
        assert!(req.headers.get("openai-beta").is_none());
    }

    #[test]
    fn test_opencode_translate_session_id() {
        // Preserves already-valid OpenCode session IDs.
        let valid = "ses_f534dfae8ffeCy4Ee4tLWNygDc";
        assert_eq!(translate_session_id(valid, None), valid);
        assert_eq!(translate_session_id(&format!("  {valid}  "), None), valid);

        // Translates foreign / UUID identities deterministically into valid canonical IDs.
        let raw_uuid = "claude:550e8400-e29b-41d4-a716-446655440000";
        let translated1 = translate_session_id(raw_uuid, Some("claude"));
        let translated2 = translate_session_id(raw_uuid, Some("claude"));
        assert_eq!(translated1, translated2);
        assert_opencode_id("ses_", &translated1);

        // Different tools or session inputs produce distinct sessions.
        let different_tool = translate_session_id(raw_uuid, Some("cursor"));
        assert_ne!(translated1, different_tool);
        assert_opencode_id("ses_", &different_tool);
    }

    #[test]
    fn test_opencode_version_validation() {
        assert!(has_valid_opencode_version("opencode/1.18.31"));
        assert!(has_valid_opencode_version("opencode/1.17.0"));
        assert!(has_valid_opencode_version("opencode/1.19.0"));
        assert!(has_valid_opencode_version("opencode/2.0.0"));
        assert!(has_valid_opencode_version(
            "opencode/1.18.31 ai-sdk/provider-utils/4.0.40 runtime/bun/1.3.14"
        ));

        // Outdated (< 1.17) or non-OpenCode versions return false.
        assert!(!has_valid_opencode_version("opencode/1.16.9"));
        assert!(!has_valid_opencode_version("opencode/1.15.0"));
        assert!(!has_valid_opencode_version("opencode"));
        assert!(!has_valid_opencode_version("Claude-Code/1.0"));
        assert!(!has_valid_opencode_version("curl/7.68.0"));
    }

    #[test]
    fn test_opencode_apply_to_header_map_upgrades_and_preserves() {
        let spoofer = OpenCodeSpoofer;

        // Upgrades foreign User-Agent and translates foreign session.
        let mut req1 = UpstreamRequest::get("https://dummy.url");
        req1.headers
            .insert(http::header::USER_AGENT, HeaderValue::from_static("Claude-Code/1.0"));
        req1.headers.insert(
            http::header::HeaderName::from_static("x-opencode-session"),
            HeaderValue::from_static("uuid-1234-5678"),
        );
        spoofer.apply_to_request(&mut req1);

        assert_eq!(
            req1.headers.get(http::header::USER_AGENT).unwrap(),
            HeaderValue::from_static(OPENCODE_UA)
        );
        let session1 = req1.headers.get("x-opencode-session").unwrap().to_str().unwrap();
        assert_opencode_id("ses_", session1);
        assert_ne!(session1, "uuid-1234-5678");

        // Preserves authentic downstream OpenCode User-Agent and valid session.
        let mut req2 = UpstreamRequest::get("https://dummy.url");
        req2.headers.insert(
            http::header::USER_AGENT,
            HeaderValue::from_static("opencode/1.19.0 custom-flag"),
        );
        let valid_session = "ses_f534dfae8ffeCy4Ee4tLWNygDc";
        req2.headers.insert(
            http::header::HeaderName::from_static("x-opencode-session"),
            HeaderValue::from_static(valid_session),
        );
        spoofer.apply_to_request(&mut req2);

        assert_eq!(
            req2.headers.get(http::header::USER_AGENT).unwrap(),
            HeaderValue::from_static("opencode/1.19.0 custom-flag")
        );
        assert_eq!(
            req2.headers.get("x-opencode-session").unwrap(),
            HeaderValue::from_static(valid_session)
        );

        // Translates candidate x-session-id when x-opencode-session is absent.
        let mut req3 = UpstreamRequest::get("https://dummy.url");
        req3.headers.insert(
            http::header::HeaderName::from_static("x-session-id"),
            HeaderValue::from_static("foreign-session-abc"),
        );
        spoofer.apply_to_request(&mut req3);
        let session3 = req3.headers.get("x-opencode-session").unwrap().to_str().unwrap();
        assert_opencode_id("ses_", session3);
        assert_eq!(session3, translate_session_id("foreign-session-abc", None));
    }

    #[test]
    fn test_antigravity_spoofer() {
        let spoofer = AntigravitySpoofer::with_project("project-xyz");
        let headers = spoofer.headers();
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "x-client-name" && v == "antigravity")
        );
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "x-goog-user-project" && v == "project-xyz")
        );
    }

    #[test]
    fn test_antigravity_spoofer_default() {
        let spoofer = AntigravitySpoofer::new();
        assert!(spoofer.project_id.is_none());
        let headers = spoofer.headers();
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "x-client-name" && v == "antigravity")
        );
        assert!(!headers.iter().any(|(k, _)| k == "x-goog-user-project"));
    }

    #[test]
    fn test_antigravity_spoofer_invalid_project_skips_header() {
        for invalid_pid in ["", "test-project", "project-id"] {
            let spoofer = AntigravitySpoofer::with_project(invalid_pid);
            let headers = spoofer.headers();
            assert!(
                !headers.iter().any(|(k, _)| k == "x-goog-user-project"),
                "x-goog-user-project header should be omitted for invalid project_id '{invalid_pid}'"
            );

            let mut req = UpstreamRequest::get("https://dummy.url");
            spoofer.apply_to_request(&mut req);
            assert!(
                req.headers.get("x-goog-user-project").is_none(),
                "x-goog-user-project header should be omitted from request for invalid project_id '{invalid_pid}'"
            );
        }
    }

    #[test]
    fn test_antigravity_spoofer_apply_to_request() {
        let spoofer = AntigravitySpoofer::with_project("project-xyz");
        let mut req = UpstreamRequest::get("https://dummy.url");
        spoofer.apply_to_request(&mut req);

        assert_eq!(
            req.headers.get("x-client-name").unwrap(),
            HeaderValue::from_static("antigravity")
        );
        assert_eq!(
            req.headers.get("x-goog-user-project").unwrap(),
            HeaderValue::from_static("project-xyz")
        );
        assert!(req.headers.contains_key("x-client-version"));
        assert!(req.headers.contains_key("x-machine-id"));
        assert!(req.headers.contains_key("x-vscode-sessionid"));
    }
}
