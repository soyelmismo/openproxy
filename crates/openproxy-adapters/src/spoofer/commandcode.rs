use super::{ClientSpoofer, DynamicHeaderOverrides, merge_header_refs, parse_env_extra_headers};

pub const DEFAULT_COMMANDCODE_CLI_VERSION: &str = "1.54.0";
pub const DEFAULT_COMMANDCODE_CLI_ENVIRONMENT: &str = "production";
pub const DEFAULT_COMMANDCODE_PROJECT_SLUG: &str = "project";
pub const DEFAULT_COMMANDCODE_TASTE_LEARNING: &str = "true";
pub const DEFAULT_COMMANDCODE_UA: &str = "cli";

pub const COMMANDCODE_SPOOFING_HEADERS: &[(&str, &str)] = &[
    ("Content-Type", "application/json"),
    ("user-agent", DEFAULT_COMMANDCODE_UA),
    ("x-command-code-version", DEFAULT_COMMANDCODE_CLI_VERSION),
    ("x-cli-environment", DEFAULT_COMMANDCODE_CLI_ENVIRONMENT),
    ("x-project-slug", DEFAULT_COMMANDCODE_PROJECT_SLUG),
    ("x-taste-learning", DEFAULT_COMMANDCODE_TASTE_LEARNING),
];

static COMMANDCODE_OVERRIDES: DynamicHeaderOverrides = DynamicHeaderOverrides::new();

/// Current dynamic version of Command Code CLI.
pub fn current_commandcode_version() -> String {
    COMMANDCODE_OVERRIDES.current_version(
        "OPENPROXY_COMMANDCODE_CLI_VERSION",
        DEFAULT_COMMANDCODE_CLI_VERSION,
    )
}

/// Dynamic Command Code User-Agent string.
pub fn current_commandcode_ua() -> String {
    if let Some(ua) = COMMANDCODE_OVERRIDES.current_extra_header("user-agent") {
        return ua;
    }
    std::env::var("OPENPROXY_COMMANDCODE_USER_AGENT")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_COMMANDCODE_UA.to_string())
}

/// Set dynamic version override for Command Code in memory at runtime without recompiling.
pub fn set_dynamic_commandcode_version(ver: impl Into<String>) {
    COMMANDCODE_OVERRIDES.set_version(ver);
}

/// Set dynamic User-Agent override for Command Code in memory at runtime without recompiling.
pub fn set_dynamic_commandcode_ua(ua: impl Into<String>) {
    COMMANDCODE_OVERRIDES.set_extra_header("user-agent", ua);
}

/// Set dynamic extra header override for Command Code in memory at runtime without recompiling.
pub fn set_dynamic_commandcode_extra_header(key: impl Into<String>, val: impl Into<String>) {
    COMMANDCODE_OVERRIDES.set_extra_header(key, val);
}

/// Reset dynamic in-memory overrides for Command Code (useful for tests and cleanup).
pub fn reset_dynamic_commandcode_overrides() {
    COMMANDCODE_OVERRIDES.reset();
}

#[cfg(any(test, feature = "test-utils"))]
pub static COMMANDCODE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

static COMMANDCODE_EXTRA_HEADERS: std::sync::LazyLock<Vec<(String, String)>> =
    std::sync::LazyLock::new(|| parse_env_extra_headers("OPENPROXY_COMMANDCODE_EXTRA_HEADERS"));

/// Preset for Command Code client identity headers.
#[derive(Debug, Clone, Copy, Default)]
pub struct CommandCodeSpoofer;

impl ClientSpoofer for CommandCodeSpoofer {
    fn headers(&self) -> Vec<(String, String)> {
        let cur_ver = current_commandcode_version();
        let cur_ua = current_commandcode_ua();
        let mut list: Vec<(String, String)> = COMMANDCODE_SPOOFING_HEADERS
            .iter()
            .map(|(k, v)| {
                if *k == "x-command-code-version" {
                    (k.to_string(), cur_ver.clone())
                } else if *k == "user-agent" {
                    (k.to_string(), cur_ua.clone())
                } else {
                    (k.to_string(), v.to_string())
                }
            })
            .collect();

        if !list
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("x-session-id"))
        {
            let session_id = uuid::Uuid::new_v4().to_string();
            list.push(("x-session-id".to_string(), session_id.clone()));
            list.push(("x-session-affinity".to_string(), session_id));
        }

        merge_header_refs(&mut list, &*COMMANDCODE_EXTRA_HEADERS);
        COMMANDCODE_OVERRIDES.apply_to_list(&mut list);
        list
    }
}
