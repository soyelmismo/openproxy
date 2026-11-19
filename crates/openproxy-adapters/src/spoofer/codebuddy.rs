use super::{ClientSpoofer, DynamicHeaderOverrides, merge_header_refs, parse_env_extra_headers};

pub const DEFAULT_CODEBUDDY_VERSION: &str = "2.168.2";

pub const CODEBUDDY_SPOOFING_HEADERS: &[(&str, &str)] = &[
    ("X-IDE-Type", "CLI"),
    ("X-IDE-Name", "CLI"),
    ("X-IDE-Version", "2.168.2"),
    ("X-Product", "SaaS"),
    ("X-Agent-Intent", "craft"),
    ("x-codebuddy-request", "1"),
    ("X-Domain", "www.workbuddy.ai"),
    ("User-Agent", "CLI/2.168.2 CodeBuddy/2.168.2"),
];

static CODEBUDDY_OVERRIDES: DynamicHeaderOverrides = DynamicHeaderOverrides::new();

/// Current dynamic version of CodeBuddy.
pub fn current_codebuddy_version() -> String {
    CODEBUDDY_OVERRIDES.current_version("OPENPROXY_CODEBUDDY_VERSION", DEFAULT_CODEBUDDY_VERSION)
}

/// Dynamic CodeBuddy User-Agent.
pub fn current_codebuddy_ua() -> String {
    if let Some(ua) = CODEBUDDY_OVERRIDES.current_extra_header("user-agent") {
        return ua;
    }
    if let Ok(ua) = std::env::var("OPENPROXY_CODEBUDDY_USER_AGENT")
        && !ua.trim().is_empty()
    {
        return ua.trim().to_string();
    }
    let ver = current_codebuddy_version();
    format!("CLI/{ver} CodeBuddy/{ver}")
}

/// Set dynamic User-Agent override for CodeBuddy in memory at runtime without recompiling.
pub fn set_dynamic_codebuddy_ua(ua: impl Into<String>) {
    CODEBUDDY_OVERRIDES.set_extra_header("user-agent", ua);
}

/// Set dynamic version override for CodeBuddy in memory at runtime without recompiling.
pub fn set_dynamic_codebuddy_version(ver: impl Into<String>) {
    let ver_str = ver.into();
    let clean = ver_str.trim().trim_start_matches('v');
    let effective = if clean.is_empty() {
        DEFAULT_CODEBUDDY_VERSION.to_string()
    } else {
        clean.to_string()
    };
    CODEBUDDY_OVERRIDES.set_version(&effective);
    if let Some(ua) = CODEBUDDY_OVERRIDES.current_extra_header("user-agent") {
        if ua.starts_with("CLI/") {
            CODEBUDDY_OVERRIDES.set_extra_header(
                "user-agent",
                format!("CLI/{effective} CodeBuddy/{effective}"),
            );
        }
    } else {
        CODEBUDDY_OVERRIDES.set_extra_header(
            "user-agent",
            format!("CLI/{effective} CodeBuddy/{effective}"),
        );
    }
}

/// Set dynamic extra header override for CodeBuddy in memory at runtime without recompiling.
pub fn set_dynamic_codebuddy_extra_header(key: impl Into<String>, val: impl Into<String>) {
    CODEBUDDY_OVERRIDES.set_extra_header(key, val);
}

/// Reset dynamic in-memory overrides for CodeBuddy (useful for tests and cleanup).
pub fn reset_dynamic_codebuddy_overrides() {
    CODEBUDDY_OVERRIDES.reset();
}

#[cfg(any(test, feature = "test-utils"))]
pub static CODEBUDDY_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(any(test, feature = "test-utils"))]
pub static CODEBUDDY_ASYNC_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

static CODEBUDDY_EXTRA_HEADERS: std::sync::LazyLock<Vec<(String, String)>> =
    std::sync::LazyLock::new(|| parse_env_extra_headers("OPENPROXY_CODEBUDDY_EXTRA_HEADERS"));

/// Preset for CodeBuddy client identity headers.
#[derive(Debug, Clone, Copy, Default)]
pub struct CodeBuddySpoofer;

impl ClientSpoofer for CodeBuddySpoofer {
    fn headers(&self) -> Vec<(String, String)> {
        let cur_ver = current_codebuddy_version();
        let cur_ua = current_codebuddy_ua();
        let mut list: Vec<(String, String)> = CODEBUDDY_SPOOFING_HEADERS
            .iter()
            .map(|(k, v)| {
                if k.eq_ignore_ascii_case("user-agent") {
                    (k.to_string(), cur_ua.clone())
                } else if k.eq_ignore_ascii_case("x-ide-version") {
                    (k.to_string(), cur_ver.clone())
                } else {
                    (k.to_string(), v.to_string())
                }
            })
            .collect();

        merge_header_refs(&mut list, &*CODEBUDDY_EXTRA_HEADERS);
        CODEBUDDY_OVERRIDES.apply_to_list(&mut list);

        list
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_codebuddy_dynamic_ua_override() {
        let _guard = CODEBUDDY_TEST_LOCK.lock().unwrap();
        reset_dynamic_codebuddy_overrides();

        assert_eq!(current_codebuddy_ua(), "CLI/2.168.2 CodeBuddy/2.168.2");

        set_dynamic_codebuddy_ua("CustomCodeBuddyCLI/1.0");
        assert_eq!(current_codebuddy_ua(), "CustomCodeBuddyCLI/1.0");

        let headers = CodeBuddySpoofer.headers();
        let ua = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("user-agent"))
            .map(|(_, v)| v.as_str());
        assert_eq!(ua, Some("CustomCodeBuddyCLI/1.0"));

        reset_dynamic_codebuddy_overrides();
        assert_eq!(current_codebuddy_ua(), "CLI/2.168.2 CodeBuddy/2.168.2");

        unsafe {
            std::env::set_var("OPENPROXY_CODEBUDDY_USER_AGENT", "EnvCodeBuddy/9.9.9");
        }
        assert_eq!(current_codebuddy_ua(), "EnvCodeBuddy/9.9.9");
        unsafe {
            std::env::remove_var("OPENPROXY_CODEBUDDY_USER_AGENT");
        }
        assert_eq!(current_codebuddy_ua(), "CLI/2.168.2 CodeBuddy/2.168.2");
    }
}
