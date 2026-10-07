use super::{ClientSpoofer, DynamicHeaderOverrides, merge_header_refs, parse_env_extra_headers};

pub const DEFAULT_CLAUDE_CODE_CLI_VERSION: &str = "2.1.280";
pub const DEFAULT_CLAUDE_CODE_PACKAGE_VERSION: &str = "0.112.1";
pub const DEFAULT_CLAUDE_CODE_RUNTIME_VERSION: &str = "v26.3.0";
pub const DEFAULT_CLAUDE_CODE_UA: &str = "claude-cli/2.1.280 (external, cli)";
pub const DEFAULT_CLAUDE_CODE_BETA: &str = "oauth-2025-04-20,claude-code-20250219,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05";
pub const DEFAULT_CLAUDE_CODE_ANTHROPIC_VERSION: &str = "2023-06-01";

static CLAUDE_CODE_OVERRIDES: DynamicHeaderOverrides = DynamicHeaderOverrides::new();

/// Current dynamic version of Claude Code CLI.
pub fn current_claude_code_version() -> String {
    CLAUDE_CODE_OVERRIDES.current_version(
        "OPENPROXY_CLAUDE_CODE_CLI_VERSION",
        DEFAULT_CLAUDE_CODE_CLI_VERSION,
    )
}

/// Dynamic Claude Code User-Agent string.
pub fn current_claude_code_ua() -> String {
    if let Some(ua) = CLAUDE_CODE_OVERRIDES.current_extra_header("user-agent") {
        return ua;
    }
    std::env::var("OPENPROXY_CLAUDE_CODE_USER_AGENT")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| {
            let ver = current_claude_code_version();
            format!("claude-cli/{ver} (external, cli)")
        })
}

/// Set dynamic version override for Claude Code CLI in memory at runtime without recompiling.
pub fn set_dynamic_claude_code_version(ver: impl Into<String>) {
    CLAUDE_CODE_OVERRIDES.set_version(ver);
}

/// Set dynamic User-Agent override for Claude Code CLI in memory at runtime without recompiling.
pub fn set_dynamic_claude_code_ua(ua: impl Into<String>) {
    CLAUDE_CODE_OVERRIDES.set_extra_header("user-agent", ua);
}

/// Set dynamic extra header override for Claude Code CLI in memory at runtime without recompiling.
pub fn set_dynamic_claude_code_extra_header(key: impl Into<String>, val: impl Into<String>) {
    CLAUDE_CODE_OVERRIDES.set_extra_header(key, val);
}

/// Reset dynamic in-memory overrides for Claude Code CLI.
pub fn reset_dynamic_claude_code_overrides() {
    CLAUDE_CODE_OVERRIDES.reset();
}

#[cfg(any(test, feature = "test-utils"))]
pub static CLAUDE_CODE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

static CLAUDE_CODE_EXTRA_HEADERS: std::sync::LazyLock<Vec<(String, String)>> =
    std::sync::LazyLock::new(|| parse_env_extra_headers("OPENPROXY_CLAUDE_CODE_EXTRA_HEADERS"));

fn detect_os() -> &'static str {
    match std::env::consts::OS {
        "macos" => "MacOS",
        "windows" => "Windows",
        _ => "Linux",
    }
}

fn detect_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        arch => arch,
    }
}

/// Preset for Claude Code client identity and fingerprint headers.
#[derive(Debug, Clone, Default)]
pub struct ClaudeCodeSpoofer {
    pub session_id: Option<String>,
}

impl ClaudeCodeSpoofer {
    pub fn new() -> Self {
        Self { session_id: None }
    }

    pub fn with_session_id(session_id: impl Into<String>) -> Self {
        Self {
            session_id: Some(session_id.into()),
        }
    }
}

impl ClientSpoofer for ClaudeCodeSpoofer {
    fn headers(&self) -> Vec<(String, String)> {
        let ua = current_claude_code_ua();
        let session = self
            .session_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let request_id = uuid::Uuid::new_v4().to_string();

        let mut list = vec![
            ("User-Agent".into(), ua),
            ("X-App".into(), "cli".into()),
            (
                "Anthropic-Version".into(),
                DEFAULT_CLAUDE_CODE_ANTHROPIC_VERSION.into(),
            ),
            (
                "Anthropic-Dangerous-Direct-Browser-Access".into(),
                "true".into(),
            ),
            ("Anthropic-Beta".into(), DEFAULT_CLAUDE_CODE_BETA.into()),
            ("X-Stainless-Retry-Count".into(), "0".into()),
            ("X-Stainless-Runtime".into(), "node".into()),
            ("X-Stainless-Lang".into(), "js".into()),
            ("X-Stainless-Timeout".into(), "600".into()),
            (
                "X-Stainless-Runtime-Version".into(),
                DEFAULT_CLAUDE_CODE_RUNTIME_VERSION.into(),
            ),
            (
                "X-Stainless-Package-Version".into(),
                DEFAULT_CLAUDE_CODE_PACKAGE_VERSION.into(),
            ),
            ("X-Stainless-Os".into(), detect_os().into()),
            ("X-Stainless-Arch".into(), detect_arch().into()),
            ("X-Claude-Code-Session-Id".into(), session),
            ("x-client-request-id".into(), request_id),
        ];

        merge_header_refs(&mut list, &*CLAUDE_CODE_EXTRA_HEADERS);
        CLAUDE_CODE_OVERRIDES.apply_to_list(&mut list);
        list
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_claude_code_spoofer_headers() {
        let _guard = CLAUDE_CODE_TEST_LOCK.lock().unwrap();
        reset_dynamic_claude_code_overrides();

        let spoofer = ClaudeCodeSpoofer::with_session_id("fixed-session-123");
        let headers = spoofer.headers();

        let get = |k: &str| {
            headers
                .iter()
                .find(|(hk, _)| hk.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.as_str())
        };

        assert_eq!(get("User-Agent"), Some(DEFAULT_CLAUDE_CODE_UA));
        assert_eq!(get("X-App"), Some("cli"));
        assert_eq!(
            get("Anthropic-Version"),
            Some(DEFAULT_CLAUDE_CODE_ANTHROPIC_VERSION)
        );
        assert_eq!(
            get("Anthropic-Dangerous-Direct-Browser-Access"),
            Some("true")
        );
        assert_eq!(get("Anthropic-Beta"), Some(DEFAULT_CLAUDE_CODE_BETA));
        assert_eq!(get("X-Stainless-Retry-Count"), Some("0"));
        assert_eq!(get("X-Stainless-Runtime"), Some("node"));
        assert_eq!(get("X-Stainless-Lang"), Some("js"));
        assert_eq!(get("X-Stainless-Timeout"), Some("600"));
        assert_eq!(
            get("X-Stainless-Runtime-Version"),
            Some(DEFAULT_CLAUDE_CODE_RUNTIME_VERSION)
        );
        assert_eq!(
            get("X-Stainless-Package-Version"),
            Some(DEFAULT_CLAUDE_CODE_PACKAGE_VERSION)
        );
        assert_eq!(get("X-Claude-Code-Session-Id"), Some("fixed-session-123"));
        assert!(get("x-client-request-id").is_some());
    }

    #[test]
    fn test_claude_code_dynamic_version_and_ua() {
        let _guard = CLAUDE_CODE_TEST_LOCK.lock().unwrap();
        reset_dynamic_claude_code_overrides();

        set_dynamic_claude_code_version("2.1.290");
        assert_eq!(current_claude_code_version(), "2.1.290");
        assert_eq!(
            current_claude_code_ua(),
            "claude-cli/2.1.290 (external, cli)"
        );

        set_dynamic_claude_code_ua("custom-claude-agent/1.0");
        assert_eq!(current_claude_code_ua(), "custom-claude-agent/1.0");

        set_dynamic_claude_code_extra_header("x-custom-test", "val123");

        let spoofer = ClaudeCodeSpoofer::new();
        let headers = spoofer.headers();
        let get = |k: &str| {
            headers
                .iter()
                .find(|(hk, _)| hk.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.as_str())
        };

        assert_eq!(get("User-Agent"), Some("custom-claude-agent/1.0"));
        assert_eq!(get("x-custom-test"), Some("val123"));

        reset_dynamic_claude_code_overrides();
        assert_eq!(
            current_claude_code_version(),
            DEFAULT_CLAUDE_CODE_CLI_VERSION
        );
    }
}
