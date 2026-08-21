//! Client spoofing traits and presets for upstream providers.
//!
//! Providers like Cline, OpenCode, and Antigravity require specific client
//! identity headers (User-Agent, machine fingerprint, editor metadata, etc.)
//! to accept requests. This module unifies spoofing into the [`ClientSpoofer`]
//! trait and provides standard presets.

use crate::upstream::UpstreamRequest;
use http::HeaderValue;

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
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenCodeSpoofer;

impl_static_spoofer!(OpenCodeSpoofer, OPENCODE_SPOOFING_HEADERS);

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

    #[test]
    fn test_opencode_spoofer() {
        let spoofer = OpenCodeSpoofer;
        let headers = spoofer.headers();
        assert_eq!(headers.len(), 3);
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "User-Agent" && v == "opencode/1.31.0")
        );
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "opencode-version" && v == "1.31.0")
        );
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "openai-beta" && v == "responses_websockets=2026-02-06")
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
    }
}
