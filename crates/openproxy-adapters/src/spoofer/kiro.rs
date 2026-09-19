use super::{ClientSpoofer, DynamicHeaderOverrides, merge_header_refs, parse_env_extra_headers};

pub const DEFAULT_KIRO_UA: &str = "aws-sdk-js/3.0.0 kiro/0.1";
pub const DEFAULT_KIRO_SDK_REQUEST: &str = "attempt=1; max=3";
pub const DEFAULT_KIRO_BEDROCK_CACHE_CONTROL: &str = "enable";
pub const DEFAULT_KIRO_ANTHROPIC_BETA: &str = "prompt-caching-2024-07-31";

pub const KIRO_SPOOFING_HEADERS: &[(&str, &str)] = &[
    ("Content-Type", "application/json"),
    ("x-amz-user-agent", DEFAULT_KIRO_UA),
    ("Amz-Sdk-Request", DEFAULT_KIRO_SDK_REQUEST),
    (
        "x-amzn-bedrock-cache-control",
        DEFAULT_KIRO_BEDROCK_CACHE_CONTROL,
    ),
    ("anthropic-beta", DEFAULT_KIRO_ANTHROPIC_BETA),
];

static KIRO_OVERRIDES: DynamicHeaderOverrides = DynamicHeaderOverrides::new();

/// Current dynamic User-Agent of Kiro / AWS CodeWhisperer.
pub fn current_kiro_ua() -> String {
    KIRO_OVERRIDES.current_version("OPENPROXY_KIRO_USER_AGENT", DEFAULT_KIRO_UA)
}

/// Set dynamic User-Agent override for Kiro in memory at runtime without recompiling.
pub fn set_dynamic_kiro_ua(ua: impl Into<String>) {
    KIRO_OVERRIDES.set_version(ua);
}

/// Set dynamic extra header override for Kiro in memory at runtime without recompiling.
pub fn set_dynamic_kiro_extra_header(key: impl Into<String>, val: impl Into<String>) {
    KIRO_OVERRIDES.set_extra_header(key, val);
}

/// Reset dynamic in-memory overrides for Kiro (useful for tests and cleanup).
pub fn reset_dynamic_kiro_overrides() {
    KIRO_OVERRIDES.reset();
}

#[cfg(any(test, feature = "test-utils"))]
pub static KIRO_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

static KIRO_EXTRA_HEADERS: std::sync::LazyLock<Vec<(String, String)>> =
    std::sync::LazyLock::new(|| parse_env_extra_headers("OPENPROXY_KIRO_EXTRA_HEADERS"));

/// Preset for Kiro AI (AWS CodeWhisperer) client identity headers.
#[derive(Debug, Clone, Copy, Default)]
pub struct KiroSpoofer;

impl ClientSpoofer for KiroSpoofer {
    fn headers(&self) -> Vec<(String, String)> {
        let cur_ua = current_kiro_ua();
        let mut list: Vec<(String, String)> = KIRO_SPOOFING_HEADERS
            .iter()
            .map(|(k, v)| {
                if *k == "x-amz-user-agent" {
                    (k.to_string(), cur_ua.clone())
                } else {
                    (k.to_string(), v.to_string())
                }
            })
            .collect();

        // Dynamically inject unique invocation ID per request
        list.push((
            "Amz-Sdk-Invocation-Id".to_string(),
            uuid::Uuid::new_v4().to_string(),
        ));

        merge_header_refs(&mut list, &*KIRO_EXTRA_HEADERS);
        KIRO_OVERRIDES.apply_to_list(&mut list);
        list
    }
}
