use super::{ClientSpoofer, DynamicHeaderOverrides, merge_header_refs, parse_env_extra_headers};

pub const DEFAULT_MINIMAX_UA: &str = "MiniMaxAgent";
pub const DEFAULT_MINIMAX_ANTHROPIC_VERSION: &str = "2023-06-01";

pub const MINIMAX_SPOOFING_HEADERS: &[(&str, &str)] = &[
    ("Content-Type", "application/json"),
    ("User-Agent", "MiniMaxAgent"),
    ("Anthropic-Version", "2023-06-01"),
    ("X-Mavis-Agent-Id", "main"),
    ("X-Mavis-Timezone-Offset", "0"),
];

static MINIMAX_OVERRIDES: DynamicHeaderOverrides = DynamicHeaderOverrides::new();

/// Current dynamic User-Agent of MiniMax.
pub fn current_minimax_ua() -> String {
    MINIMAX_OVERRIDES.current_version("OPENPROXY_MINIMAX_USER_AGENT", DEFAULT_MINIMAX_UA)
}

/// Current dynamic Anthropic-Version of MiniMax.
pub fn current_minimax_anthropic_version() -> String {
    if let Some(v) = MINIMAX_OVERRIDES.current_extra_header("Anthropic-Version") {
        return v;
    }
    std::env::var("OPENPROXY_MINIMAX_ANTHROPIC_VERSION")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_MINIMAX_ANTHROPIC_VERSION.to_string())
}

/// Set dynamic User-Agent override for MiniMax in memory at runtime.
pub fn set_dynamic_minimax_ua(ua: impl Into<String>) {
    MINIMAX_OVERRIDES.set_version(ua);
}

/// Set dynamic Anthropic-Version override for MiniMax in memory at runtime.
pub fn set_dynamic_minimax_anthropic_version(ver: impl Into<String>) {
    MINIMAX_OVERRIDES.set_extra_header("Anthropic-Version", ver);
}

/// Set dynamic extra header override for MiniMax in memory at runtime without recompiling.
pub fn set_dynamic_minimax_extra_header(key: impl Into<String>, val: impl Into<String>) {
    MINIMAX_OVERRIDES.set_extra_header(key, val);
}

/// Reset dynamic in-memory overrides for MiniMax (useful for tests and cleanup).
pub fn reset_dynamic_minimax_overrides() {
    MINIMAX_OVERRIDES.reset();
}

#[cfg(any(test, feature = "test-utils"))]
pub static MINIMAX_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

static MINIMAX_EXTRA_HEADERS: std::sync::LazyLock<Vec<(String, String)>> =
    std::sync::LazyLock::new(|| parse_env_extra_headers("OPENPROXY_MINIMAX_EXTRA_HEADERS"));

/// Preset for MiniMax Coding client identity headers.
#[derive(Debug, Clone, Copy, Default)]
pub struct MiniMaxSpoofer;

impl ClientSpoofer for MiniMaxSpoofer {
    fn headers(&self) -> Vec<(String, String)> {
        let cur_ua = current_minimax_ua();
        let cur_ver = current_minimax_anthropic_version();
        let mut list: Vec<(String, String)> = MINIMAX_SPOOFING_HEADERS
            .iter()
            .map(|(k, v)| {
                if *k == "User-Agent" {
                    (k.to_string(), cur_ua.clone())
                } else if *k == "Anthropic-Version" {
                    (k.to_string(), cur_ver.clone())
                } else {
                    (k.to_string(), v.to_string())
                }
            })
            .collect();

        list.push((
            "X-Mavis-Session-Id".to_string(),
            format!("session_{}", uuid::Uuid::new_v4().simple()),
        ));

        merge_header_refs(&mut list, &*MINIMAX_EXTRA_HEADERS);
        MINIMAX_OVERRIDES.apply_to_list(&mut list);

        list
    }
}
