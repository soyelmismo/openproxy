use super::{
    ClientSpoofer, DynamicHeaderOverrides, merge_header_refs, parse_env_extra_headers,
};

pub const DEFAULT_CODEX_VERSION: &str = "0.144.0";

pub const CODEX_SPOOFING_HEADERS: &[(&str, &str)] = &[
    ("origin", "https://chatgpt.com"),
    ("originator", "codex_cli_rs"),
    ("version", "0.144.0"),
    ("user-agent", "codex-cli/0.144.0 (Windows 10.0.26200; x64)"),
];

static CODEX_OVERRIDES: DynamicHeaderOverrides = DynamicHeaderOverrides::new();

fn safe_env_value(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// Current dynamic version of Codex CLI.
pub fn current_codex_version() -> String {
    let ver = CODEX_OVERRIDES.current_version("OPENPROXY_CODEX_CLIENT_VERSION", "");
    if !ver.is_empty() {
        return ver;
    }
    safe_env_value("CODEX_CLIENT_VERSION").unwrap_or_else(|| DEFAULT_CODEX_VERSION.to_string())
}

/// Dynamic Codex User-Agent string.
pub fn current_codex_ua() -> String {
    if let Some(env_ua) = safe_env_value("OPENPROXY_CODEX_USER_AGENT")
        .or_else(|| safe_env_value("CODEX_USER_AGENT"))
    {
        return env_ua;
    }
    format!(
        "codex-cli/{} (Windows 10.0.26200; x64)",
        current_codex_version()
    )
}

/// Set dynamic version override for Codex in memory at runtime without recompiling.
pub fn set_dynamic_codex_version(ver: impl Into<String>) {
    CODEX_OVERRIDES.set_version(ver);
}

/// Set dynamic extra header override for Codex in memory at runtime without recompiling.
pub fn set_dynamic_codex_extra_header(key: impl Into<String>, val: impl Into<String>) {
    CODEX_OVERRIDES.set_extra_header(key, val);
}

/// Reset dynamic in-memory overrides for Codex (useful for tests and cleanup).
pub fn reset_dynamic_codex_overrides() {
    CODEX_OVERRIDES.reset();
}

#[cfg(any(test, feature = "test-utils"))]
pub static CODEX_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

static CODEX_EXTRA_HEADERS: std::sync::LazyLock<Vec<(String, String)>> =
    std::sync::LazyLock::new(|| parse_env_extra_headers("OPENPROXY_CODEX_EXTRA_HEADERS"));

/// Preset for Codex client identity headers.
#[derive(Debug, Clone, Copy, Default)]
pub struct CodexSpoofer;

impl ClientSpoofer for CodexSpoofer {
    fn headers(&self) -> Vec<(String, String)> {
        let cur_ver = current_codex_version();
        let cur_ua = current_codex_ua();
        let mut list: Vec<(String, String)> = CODEX_SPOOFING_HEADERS
            .iter()
            .map(|(k, v)| {
                if *k == "user-agent" {
                    (k.to_string(), cur_ua.clone())
                } else if *k == "version" {
                    (k.to_string(), cur_ver.clone())
                } else {
                    (k.to_string(), v.to_string())
                }
            })
            .collect();

        merge_header_refs(&mut list, &*CODEX_EXTRA_HEADERS);
        CODEX_OVERRIDES.apply_to_list(&mut list);

        list
    }
}
