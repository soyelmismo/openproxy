use super::{
    ClientSpoofer, DynamicHeaderOverrides, merge_header_refs, parse_env_extra_headers,
};

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

static CLINE_OVERRIDES: DynamicHeaderOverrides = DynamicHeaderOverrides::new();

/// Current dynamic version of Cline.
pub fn current_cline_version() -> String {
    CLINE_OVERRIDES.current_version("OPENPROXY_CLINE_VERSION", DEFAULT_CLINE_VERSION)
}

/// Dynamic Cline User-Agent.
pub fn current_cline_ua() -> String {
    format!("Cline/{}", current_cline_version())
}

/// Set dynamic version override for Cline in memory at runtime without recompiling.
pub fn set_dynamic_cline_version(ver: impl Into<String>) {
    CLINE_OVERRIDES.set_version(ver);
}

/// Set dynamic extra header override for Cline in memory at runtime without recompiling.
pub fn set_dynamic_cline_extra_header(key: impl Into<String>, val: impl Into<String>) {
    CLINE_OVERRIDES.set_extra_header(key, val);
}

/// Reset dynamic in-memory overrides for Cline (useful for tests and cleanup).
pub fn reset_dynamic_cline_overrides() {
    CLINE_OVERRIDES.reset();
}

#[cfg(any(test, feature = "test-utils"))]
pub static CLINE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

static CLINE_EXTRA_HEADERS: std::sync::LazyLock<Vec<(String, String)>> =
    std::sync::LazyLock::new(|| parse_env_extra_headers("OPENPROXY_CLINE_EXTRA_HEADERS"));

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

        merge_header_refs(&mut list, &*CLINE_EXTRA_HEADERS);
        CLINE_OVERRIDES.apply_to_list(&mut list);

        list
    }
}
