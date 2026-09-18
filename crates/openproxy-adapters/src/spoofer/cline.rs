use super::ClientSpoofer;
use http::HeaderValue;

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

static CLINE_DYNAMIC_VERSION: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);
static CLINE_DYNAMIC_EXTRA_HEADERS: std::sync::RwLock<std::collections::BTreeMap<String, String>> =
    std::sync::RwLock::new(std::collections::BTreeMap::new());

/// Current dynamic version of Cline.
pub fn current_cline_version() -> String {
    if let Ok(lock) = CLINE_DYNAMIC_VERSION.read()
        && let Some(ref ver) = *lock
    {
        return ver.clone();
    }
    if let Ok(env_ver) = std::env::var("OPENPROXY_CLINE_VERSION")
        && !env_ver.is_empty()
    {
        return env_ver;
    }
    DEFAULT_CLINE_VERSION.to_string()
}

/// Dynamic Cline User-Agent.
pub fn current_cline_ua() -> String {
    format!("Cline/{}", current_cline_version())
}

/// Set dynamic version override for Cline in memory at runtime without recompiling.
pub fn set_dynamic_cline_version(ver: impl Into<String>) {
    if let Ok(mut lock) = CLINE_DYNAMIC_VERSION.write() {
        *lock = Some(ver.into());
    }
}

/// Set dynamic extra header override for Cline in memory at runtime without recompiling.
pub fn set_dynamic_cline_extra_header(key: impl Into<String>, val: impl Into<String>) {
    if let Ok(mut lock) = CLINE_DYNAMIC_EXTRA_HEADERS.write() {
        lock.insert(key.into(), val.into());
    }
}

/// Reset dynamic in-memory overrides for Cline (useful for tests and cleanup).
pub fn reset_dynamic_cline_overrides() {
    if let Ok(mut lock) = CLINE_DYNAMIC_VERSION.write() {
        *lock = None;
    }
    if let Ok(mut lock) = CLINE_DYNAMIC_EXTRA_HEADERS.write() {
        lock.clear();
    }
}

#[cfg(test)]
pub(crate) static CLINE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

static CLINE_EXTRA_HEADERS: std::sync::LazyLock<Vec<(String, String)>> = std::sync::LazyLock::new(|| {
    let Ok(env_str) = std::env::var("OPENPROXY_CLINE_EXTRA_HEADERS") else {
        return Vec::new();
    };
    let Ok(map) = serde_json::from_str::<std::collections::BTreeMap<String, String>>(&env_str) else {
        return Vec::new();
    };
    map.into_iter().collect()
});

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

        for (k, v) in CLINE_EXTRA_HEADERS.iter() {
            if let Some(pos) = list.iter().position(|(hk, _)| hk.eq_ignore_ascii_case(k)) {
                list[pos].1 = v.clone();
            } else {
                list.push((k.clone(), v.clone()));
            }
        }

        if let Ok(lock) = CLINE_DYNAMIC_EXTRA_HEADERS.read() {
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
