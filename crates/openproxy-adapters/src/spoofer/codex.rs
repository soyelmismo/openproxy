use super::ClientSpoofer;
use http::HeaderValue;

pub const DEFAULT_CODEX_VERSION: &str = "0.144.0";

pub const CODEX_SPOOFING_HEADERS: &[(&str, &str)] = &[
    ("origin", "https://chatgpt.com"),
    ("originator", "codex_cli_rs"),
    ("version", "0.144.0"),
    ("user-agent", "codex-cli/0.144.0 (Windows 10.0.26200; x64)"),
];

static CODEX_DYNAMIC_VERSION: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);
static CODEX_DYNAMIC_EXTRA_HEADERS: std::sync::RwLock<std::collections::BTreeMap<String, String>> =
    std::sync::RwLock::new(std::collections::BTreeMap::new());

fn safe_env_value(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// Current dynamic version of Codex CLI.
pub fn current_codex_version() -> String {
    if let Ok(lock) = CODEX_DYNAMIC_VERSION.read()
        && let Some(ref ver) = *lock
    {
        return ver.clone();
    }
    if let Some(env_ver) = safe_env_value("OPENPROXY_CODEX_CLIENT_VERSION")
        .or_else(|| safe_env_value("CODEX_CLIENT_VERSION"))
    {
        return env_ver;
    }
    DEFAULT_CODEX_VERSION.to_string()
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
    if let Ok(mut lock) = CODEX_DYNAMIC_VERSION.write() {
        *lock = Some(ver.into());
    }
}

/// Set dynamic extra header override for Codex in memory at runtime without recompiling.
pub fn set_dynamic_codex_extra_header(key: impl Into<String>, val: impl Into<String>) {
    if let Ok(mut lock) = CODEX_DYNAMIC_EXTRA_HEADERS.write() {
        lock.insert(key.into(), val.into());
    }
}

/// Reset dynamic in-memory overrides for Codex (useful for tests and cleanup).
pub fn reset_dynamic_codex_overrides() {
    if let Ok(mut lock) = CODEX_DYNAMIC_VERSION.write() {
        *lock = None;
    }
    if let Ok(mut lock) = CODEX_DYNAMIC_EXTRA_HEADERS.write() {
        lock.clear();
    }
}

#[cfg(test)]
pub(crate) static CODEX_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

static CODEX_EXTRA_HEADERS: std::sync::LazyLock<Vec<(String, String)>> = std::sync::LazyLock::new(|| {
    let Ok(env_str) = std::env::var("OPENPROXY_CODEX_EXTRA_HEADERS") else {
        return Vec::new();
    };
    let Ok(map) = serde_json::from_str::<std::collections::BTreeMap<String, String>>(&env_str) else {
        return Vec::new();
    };
    map.into_iter().collect()
});

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

        for (k, v) in CODEX_EXTRA_HEADERS.iter() {
            if let Some(pos) = list.iter().position(|(hk, _)| hk.eq_ignore_ascii_case(k)) {
                list[pos].1 = v.clone();
            } else {
                list.push((k.clone(), v.clone()));
            }
        }

        if let Ok(lock) = CODEX_DYNAMIC_EXTRA_HEADERS.read() {
            for (k, v) in lock.iter() {
                if let Some(pos) = list.iter().position(|(hk, _)| hk.eq_ignore_ascii_case(k)) {
                    list[pos].1 = v.clone();
                } else {
                    list.push((k.clone(), v.clone()));
                }
            }
        }

        list
    }

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
