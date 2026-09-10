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

pub const OPENCODE_SPOOFING_HEADERS: &[(&str, &str)] = &[
    ("User-Agent", "opencode/1.31.0"),
    ("opencode-version", "1.31.0"),
    ("openai-beta", "responses_websockets=2026-02-06"),
];

/// Preset for OpenCode client identity headers.
///
/// Three headers are static (`OPENCODE_SPOOFING_HEADERS`): `User-Agent`,
/// `opencode-version`, `openai-beta`.
///
/// Three additional headers are generated per request to satisfy upstream
/// session validation:
/// - `x-opencode-session`  — `"ses_" + 26 alphanumeric chars`
/// - `x-opencode-request`  — `"msg_" + 26 alphanumeric chars`
/// - `x-opencode-client`   — `"cli"`
///
/// See `other_projects_examples/opencode/packages/schema/src/identifier.ts`
/// and `other_projects_examples/opencode/packages/opencode/src/session/llm/request.ts`
/// (headers built in `prepare`) for the canonical format.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenCodeSpoofer;

/// Base62 alphabet used to generate opencode session/request IDs.
const OPENCODE_B62: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// Length of the random suffix after the prefix (`ses_` / `msg_`).
const OPENCODE_ID_SUFFIX_LEN: usize = 26;

fn generate_random_suffix(rng: &mut impl rand::Rng) -> String {
    (0..OPENCODE_ID_SUFFIX_LEN)
        .map(|_| {
            let idx = rng.random_range(0..OPENCODE_B62.len());
            OPENCODE_B62[idx] as char
        })
        .collect()
}

impl ClientSpoofer for OpenCodeSpoofer {
    fn headers(&self) -> Vec<(String, String)> {
        let mut list: Vec<(String, String)> = OPENCODE_SPOOFING_HEADERS
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();

        let mut rng = rand::rng();

        let session_suffix = generate_random_suffix(&mut rng);
        list.push(("x-opencode-session".into(), format!("ses_{session_suffix}")));

        let request_suffix = generate_random_suffix(&mut rng);
        list.push(("x-opencode-request".into(), format!("msg_{request_suffix}")));

        list.push(("x-opencode-client".into(), "cli".into()));

        list
    }

    // NOTE: `apply_to_header_map` intentionally NOT overridden.
    // The trait default (lines 21-30) already iterates `self.headers()`.
    // Avoid the duplication in FxSpoofer which duplicates static+dynamic
    // logic across both methods.
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

// =====================================================================
// Fx Spoofing Preset (for fx.sh web wasm gateway)
// =====================================================================

pub const FX_STATIC_SPOOFING_HEADERS: &[(&str, &str)] = &[
    ("origin", "https://fx.sh"),
    ("referer", "https://fx.sh/"),
    ("http-referer", "https://github.com/vercel-labs/fx"),
    ("x-title", "fx"),
    ("ai-gateway-protocol-version", "0.0.1"),
    ("ai-language-model-specification-version", "4"),
    ("ai-language-model-streaming", "true"),
    ("sec-fetch-dest", "empty"),
    ("sec-fetch-mode", "cors"),
    ("sec-fetch-site", "same-origin"),
    (
        "user-agent",
        "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/150.0.0.0 Safari/537.36",
    ),
];

/// Preset for fx.sh WebAssembly gateway client identity headers.
#[derive(Debug, Clone, Copy, Default)]
pub struct FxSpoofer;

impl ClientSpoofer for FxSpoofer {
    fn headers(&self) -> Vec<(String, String)> {
        let mut list: Vec<(String, String)> = FX_STATIC_SPOOFING_HEADERS
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let session_id = format!("{now_ms}-{now_ms}000000-59e35abc56800be2");
        list.push(("x-session-id".into(), session_id.clone()));
        list.push(("x-session-affinity".into(), session_id));
        list
    }

    fn apply_to_header_map(&self, headers: &mut http::HeaderMap) {
        for &(k, v) in FX_STATIC_SPOOFING_HEADERS {
            if let Ok(name) = http::header::HeaderName::try_from(k)
                && let Ok(val) = HeaderValue::try_from(v)
            {
                headers.insert(name, val);
            }
        }

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let session_id = format!("{now_ms}-{now_ms}000000-59e35abc56800be2");
        if let Ok(val) = HeaderValue::try_from(session_id.as_str()) {
            headers.insert(
                http::header::HeaderName::from_static("x-session-id"),
                val.clone(),
            );
            headers.insert(
                http::header::HeaderName::from_static("x-session-affinity"),
                val,
            );
        }
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
        let suffix = &id[prefix.len()..];
        assert_eq!(
            suffix.len(),
            OPENCODE_ID_SUFFIX_LEN,
            "id suffix length mismatch"
        );
        assert!(
            suffix.chars().all(|c| c.is_ascii_alphanumeric()),
            "id suffix must be base62 alphanumeric: {id}"
        );
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

        // Dynamic session/request IDs follow ses_/msg_ + 26 base62 chars.
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
        // Static headers still flow through the trait default.
        assert_eq!(
            req.headers.get("User-Agent").unwrap(),
            HeaderValue::from_str("opencode/1.31.0").unwrap()
        );
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

    #[test]
    fn test_fx_spoofer() {
        let spoofer = FxSpoofer;
        let headers = spoofer.headers();
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "origin" && v == "https://fx.sh")
        );
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "http-referer" && v == "https://github.com/vercel-labs/fx")
        );
        assert!(headers.iter().any(|(k, _)| k == "x-session-id"));

        let mut req = UpstreamRequest::get("https://fx.sh");
        spoofer.apply_to_request(&mut req);
        assert_eq!(
            req.headers.get("origin").unwrap(),
            HeaderValue::from_static("https://fx.sh")
        );
        assert!(req.headers.contains_key("x-session-id"));
        assert!(req.headers.contains_key("x-session-affinity"));
    }
}
