use super::{ClientSpoofer, DynamicHeaderOverrides, merge_header_refs, parse_env_extra_headers};

pub const DEFAULT_KILOCODE_VERSION: &str = "4.108.0";

pub const KILOCODE_SPOOFING_HEADERS: &[(&str, &str)] = &[
    ("http-referer", "https://kilocode.ai"),
    ("x-title", "Kilo Code"),
    ("user-agent", "Kilo-Code/4.108.0"),
    ("x-kilocode-version", "4.108.0"),
    ("x-client-type", "VSCode Extension"),
    ("x-client-version", "4.108.0"),
];

static KILOCODE_OVERRIDES: DynamicHeaderOverrides = DynamicHeaderOverrides::new();

/// Current dynamic version of Kilocode.
pub fn current_kilocode_version() -> String {
    KILOCODE_OVERRIDES.current_version("OPENPROXY_KILOCODE_VERSION", DEFAULT_KILOCODE_VERSION)
}

/// Dynamic Kilocode User-Agent.
pub fn current_kilocode_ua() -> String {
    format!("Kilo-Code/{}", current_kilocode_version())
}

/// Set dynamic version override for Kilocode in memory at runtime without recompiling.
pub fn set_dynamic_kilocode_version(ver: impl Into<String>) {
    KILOCODE_OVERRIDES.set_version(ver);
}

/// Set dynamic extra header override for Kilocode in memory at runtime without recompiling.
pub fn set_dynamic_kilocode_extra_header(key: impl Into<String>, val: impl Into<String>) {
    KILOCODE_OVERRIDES.set_extra_header(key, val);
}

/// Reset dynamic in-memory overrides for Kilocode (useful for tests and cleanup).
pub fn reset_dynamic_kilocode_overrides() {
    KILOCODE_OVERRIDES.reset();
}

#[cfg(any(test, feature = "test-utils"))]
pub static KILOCODE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

static KILOCODE_EXTRA_HEADERS: std::sync::LazyLock<Vec<(String, String)>> =
    std::sync::LazyLock::new(|| parse_env_extra_headers("OPENPROXY_KILOCODE_EXTRA_HEADERS"));

/// Preset for Kilocode client identity headers.
#[derive(Debug, Clone, Copy, Default)]
pub struct KilocodeSpoofer;

impl ClientSpoofer for KilocodeSpoofer {
    fn headers(&self) -> Vec<(String, String)> {
        let cur_ver = current_kilocode_version();
        let cur_ua = current_kilocode_ua();
        let mut list: Vec<(String, String)> = KILOCODE_SPOOFING_HEADERS
            .iter()
            .map(|(k, v)| {
                if *k == "user-agent" {
                    (k.to_string(), cur_ua.clone())
                } else if *k == "x-kilocode-version" || *k == "x-client-version" {
                    (k.to_string(), cur_ver.clone())
                } else {
                    (k.to_string(), v.to_string())
                }
            })
            .collect();

        merge_header_refs(&mut list, &*KILOCODE_EXTRA_HEADERS);
        KILOCODE_OVERRIDES.apply_to_list(&mut list);

        list
    }
}
