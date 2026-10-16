use super::{ClientSpoofer, DynamicHeaderOverrides, merge_header_refs, parse_env_extra_headers};

pub const DEFAULT_CLAUDE_CODE_CLI_VERSION: &str = "2.1.293";
pub const DEFAULT_CLAUDE_CODE_PACKAGE_VERSION: &str = "0.128.0";
pub const DEFAULT_CLAUDE_CODE_RUNTIME_VERSION: &str = "v26.3.0";
pub const DEFAULT_CLAUDE_CODE_UA: &str = "claude-cli/2.1.293 (external, cli)";
pub const DEFAULT_CLAUDE_CODE_BETA: &str = "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,mid-conversation-tool-changes-2026-07-01,dangerous-tool-use-2026-09-03,thinking-display-updates-2026-08-18,afk-mode-2026-01-31";
pub const DEFAULT_CLAUDE_CODE_ANTHROPIC_VERSION: &str = "2023-06-01";

static CLAUDE_CODE_OVERRIDES: DynamicHeaderOverrides = DynamicHeaderOverrides::new();

/// A coherent profile tuple. Manual overrides are deliberately kept authoritative.
pub fn current_claude_code_identity() -> (String, String, String, String) {
    current_claude_code_identity_from(&super::claude_code_profile::current_profile())
}

/// El mismo tuple coherente anclado a un perfil concreto: el snapshot pinneado
/// de esta petición. Los overrides manuales siguen ganando, porque la decisión
/// explícita del despliegue no la cambia una captura.
pub fn current_claude_code_identity_from(
    profile: &super::claude_code_profile::ClaudeCodeProfile,
) -> (String, String, String, String) {
    let version = CLAUDE_CODE_OVERRIDES
        .current_version("OPENPROXY_CLAUDE_CODE_CLI_VERSION", &profile.cli_version);
    let value = |header: &str, env: &str, default: String| {
        CLAUDE_CODE_OVERRIDES
            .current_extra_header(header)
            .or_else(|| std::env::var(env).ok().filter(|v| !v.trim().is_empty()))
            .unwrap_or(default)
    };
    let ua = value(
        "user-agent",
        "OPENPROXY_CLAUDE_CODE_USER_AGENT",
        format!("claude-cli/{version} (external, cli)"),
    );
    let package = value(
        "x-stainless-package-version",
        "OPENPROXY_CLAUDE_CODE_PACKAGE_VERSION",
        profile.package_version.clone(),
    );
    let runtime = value(
        "x-stainless-runtime-version",
        "OPENPROXY_CLAUDE_CODE_RUNTIME_VERSION",
        profile.runtime_version.clone(),
    );
    (ua, version, package, runtime)
}

/// Current dynamic version of Claude Code CLI.
pub fn current_claude_code_version() -> String {
    current_claude_code_version_from(&super::claude_code_profile::current_profile())
}

/// La versión de CLI del perfil pinneado de la petición.
pub fn current_claude_code_version_from(
    profile: &super::claude_code_profile::ClaudeCodeProfile,
) -> String {
    CLAUDE_CODE_OVERRIDES.current_version("OPENPROXY_CLAUDE_CODE_CLI_VERSION", &profile.cli_version)
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

/// Current dynamic package version of Claude Code CLI.
pub fn current_claude_code_package_version() -> String {
    if let Some(v) = CLAUDE_CODE_OVERRIDES.current_extra_header("x-stainless-package-version") {
        return v;
    }
    std::env::var("OPENPROXY_CLAUDE_CODE_PACKAGE_VERSION")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| super::claude_code_profile::current_profile().package_version)
}

/// Current dynamic runtime version of Claude Code CLI.
pub fn current_claude_code_runtime_version() -> String {
    current_claude_code_runtime_version_from(&super::claude_code_profile::current_profile())
}

/// La versión de runtime stainless del perfil pinneado de la petición.
pub fn current_claude_code_runtime_version_from(
    profile: &super::claude_code_profile::ClaudeCodeProfile,
) -> String {
    if let Some(v) = CLAUDE_CODE_OVERRIDES.current_extra_header("x-stainless-runtime-version") {
        return v;
    }
    std::env::var("OPENPROXY_CLAUDE_CODE_RUNTIME_VERSION")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| profile.runtime_version.clone())
}

/// Current dynamic Anthropic API version.
pub fn current_claude_code_anthropic_version() -> String {
    if let Some(v) = CLAUDE_CODE_OVERRIDES.current_extra_header("anthropic-version") {
        return v;
    }
    std::env::var("OPENPROXY_CLAUDE_CODE_ANTHROPIC_VERSION")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_CLAUDE_CODE_ANTHROPIC_VERSION.to_string())
}

/// Set dynamic version override for Claude Code CLI in memory at runtime without recompiling.
pub fn set_dynamic_claude_code_version(ver: impl Into<String>) {
    CLAUDE_CODE_OVERRIDES.set_version(ver);
}

/// Set dynamic User-Agent override for Claude Code CLI in memory at runtime without recompiling.
pub fn set_dynamic_claude_code_ua(ua: impl Into<String>) {
    CLAUDE_CODE_OVERRIDES.set_extra_header("user-agent", ua);
}

/// Set dynamic package version override for Claude Code CLI in memory at runtime without recompiling.
pub fn set_dynamic_claude_code_package_version(ver: impl Into<String>) {
    CLAUDE_CODE_OVERRIDES.set_extra_header("x-stainless-package-version", ver);
}

/// Set dynamic runtime version override for Claude Code CLI in memory at runtime without recompiling.
pub fn set_dynamic_claude_code_runtime_version(ver: impl Into<String>) {
    CLAUDE_CODE_OVERRIDES.set_extra_header("x-stainless-runtime-version", ver);
}

/// Set dynamic Anthropic API version override in memory at runtime without recompiling.
pub fn set_dynamic_claude_code_anthropic_version(ver: impl Into<String>) {
    CLAUDE_CODE_OVERRIDES.set_extra_header("anthropic-version", ver);
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
#[derive(Debug, Clone)]
pub struct ClaudeCodeSpoofer {
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub is_oauth: bool,
}

impl Default for ClaudeCodeSpoofer {
    fn default() -> Self {
        Self {
            session_id: None,
            model: None,
            is_oauth: true,
        }
    }
}

impl ClaudeCodeSpoofer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_session_id(session_id: impl Into<String>) -> Self {
        Self {
            session_id: Some(session_id.into()),
            model: None,
            is_oauth: true,
        }
    }

    pub fn session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn with_oauth(mut self, is_oauth: bool) -> Self {
        self.is_oauth = is_oauth;
        self
    }
}

impl ClientSpoofer for ClaudeCodeSpoofer {
    fn headers(&self) -> Vec<(String, String)> {
        self.headers_for_profile(&crate::spoofer::claude_code_profile::current_profile())
    }
}

impl ClaudeCodeSpoofer {
    /// Los headers del spoofer anclados al perfil pinneado de la petición: UA,
    /// betas y versiones stainless salen de ESA generación, la misma que el
    /// body y los headers de billing.
    #[must_use]
    pub fn headers_for_profile(
        &self,
        profile: &crate::spoofer::claude_code_profile::ClaudeCodeProfile,
    ) -> Vec<(String, String)> {
        let (ua, _version, package, runtime) = current_claude_code_identity_from(profile);
        let session = self
            .session_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let request_id = uuid::Uuid::new_v4().to_string();

        let betas = if let Some(ref m) = self.model {
            crate::adapters::claude_code::cloaking::build_claude_code_betas_from_profile(
                m,
                false,
                self.is_oauth,
                profile,
            )
        } else {
            DEFAULT_CLAUDE_CODE_BETA.to_string()
        };

        let mut list = vec![
            ("User-Agent".into(), ua),
            ("x-app".into(), "cli".into()),
            (
                "anthropic-version".into(),
                current_claude_code_anthropic_version(),
            ),
            (
                "anthropic-dangerous-direct-browser-access".into(),
                "true".into(),
            ),
            ("anthropic-beta".into(), betas),
            ("X-Stainless-Retry-Count".into(), "0".into()),
            ("X-Stainless-Runtime".into(), "node".into()),
            ("X-Stainless-Lang".into(), "js".into()),
            ("X-Stainless-Timeout".into(), "600".into()),
            ("X-Stainless-Runtime-Version".into(), runtime),
            ("X-Stainless-Package-Version".into(), package),
            ("X-Stainless-OS".into(), detect_os().into()),
            ("X-Stainless-Arch".into(), detect_arch().into()),
            ("X-Claude-Code-Session-Id".into(), session),
            ("x-client-request-id".into(), request_id),
            ("Accept".into(), "application/json".into()),
            ("Connection".into(), "keep-alive".into()),
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

        set_dynamic_claude_code_package_version("0.120.0");
        assert_eq!(current_claude_code_package_version(), "0.120.0");

        set_dynamic_claude_code_runtime_version("v28.0.0");
        assert_eq!(current_claude_code_runtime_version(), "v28.0.0");

        set_dynamic_claude_code_anthropic_version("2024-05-01");
        assert_eq!(current_claude_code_anthropic_version(), "2024-05-01");

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
        assert_eq!(get("X-Stainless-Package-Version"), Some("0.120.0"));
        assert_eq!(get("X-Stainless-Runtime-Version"), Some("v28.0.0"));
        assert_eq!(get("Anthropic-Version"), Some("2024-05-01"));

        reset_dynamic_claude_code_overrides();
        assert_eq!(
            current_claude_code_version(),
            DEFAULT_CLAUDE_CODE_CLI_VERSION
        );
        assert_eq!(
            current_claude_code_package_version(),
            DEFAULT_CLAUDE_CODE_PACKAGE_VERSION
        );
        assert_eq!(
            current_claude_code_runtime_version(),
            DEFAULT_CLAUDE_CODE_RUNTIME_VERSION
        );
        assert_eq!(
            current_claude_code_anthropic_version(),
            DEFAULT_CLAUDE_CODE_ANTHROPIC_VERSION
        );
    }
}
