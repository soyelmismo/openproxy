use super::ClientSpoofer;
use http::HeaderValue;

pub const DEFAULT_KILOCODE_VERSION: &str = "4.108.0";

pub const KILOCODE_SPOOFING_HEADERS: &[(&str, &str)] = &[
    ("http-referer", "https://kilocode.ai"),
    ("x-title", "Kilo Code"),
    ("user-agent", "Kilo-Code/4.108.0"),
    ("x-kilocode-version", "4.108.0"),
    ("x-client-type", "VSCode Extension"),
    ("x-client-version", "4.108.0"),
];

static KILOCODE_DYNAMIC_VERSION: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);
static KILOCODE_DYNAMIC_EXTRA_HEADERS: std::sync::RwLock<std::collections::BTreeMap<String, String>> =
    std::sync::RwLock::new(std::collections::BTreeMap::new());

/// Current dynamic version of Kilocode.
pub fn current_kilocode_version() -> String {
    if let Ok(lock) = KILOCODE_DYNAMIC_VERSION.read()
        && let Some(ref ver) = *lock
    {
        return ver.clone();
    }
    if let Ok(env_ver) = std::env::var("OPENPROXY_KILOCODE_VERSION")
        && !env_ver.is_empty()
    {
        return env_ver;
    }
    DEFAULT_KILOCODE_VERSION.to_string()
}

/// Dynamic Kilocode User-Agent.
pub fn current_kilocode_ua() -> String {
    format!("Kilo-Code/{}", current_kilocode_version())
}

/// Set dynamic version override for Kilocode in memory at runtime without recompiling.
pub fn set_dynamic_kilocode_version(ver: impl Into<String>) {
    if let Ok(mut lock) = KILOCODE_DYNAMIC_VERSION.write() {
        *lock = Some(ver.into());
    }
}

/// Set dynamic extra header override for Kilocode in memory at runtime without recompiling.
pub fn set_dynamic_kilocode_extra_header(key: impl Into<String>, val: impl Into<String>) {
    if let Ok(mut lock) = KILOCODE_DYNAMIC_EXTRA_HEADERS.write() {
        lock.insert(key.into(), val.into());
    }
}

/// Reset dynamic in-memory overrides for Kilocode (useful for tests and cleanup).
pub fn reset_dynamic_kilocode_overrides() {
    if let Ok(mut lock) = KILOCODE_DYNAMIC_VERSION.write() {
        *lock = None;
    }
    if let Ok(mut lock) = KILOCODE_DYNAMIC_EXTRA_HEADERS.write() {
        lock.clear();
    }
}

#[cfg(test)]
pub(crate) static KILOCODE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

static KILOCODE_EXTRA_HEADERS: std::sync::LazyLock<Vec<(String, String)>> = std::sync::LazyLock::new(|| {
    let Ok(env_str) = std::env::var("OPENPROXY_KILOCODE_EXTRA_HEADERS") else {
        return Vec::new();
    };
    let Ok(map) = serde_json::from_str::<std::collections::BTreeMap<String, String>>(&env_str) else {
        return Vec::new();
    };
    map.into_iter().collect()
});

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

        for (k, v) in KILOCODE_EXTRA_HEADERS.iter() {
            if let Some(pos) = list.iter().position(|(hk, _)| hk.eq_ignore_ascii_case(k)) {
                list[pos].1 = v.clone();
            } else {
                list.push((k.clone(), v.clone()));
            }
        }

        if let Ok(lock) = KILOCODE_DYNAMIC_EXTRA_HEADERS.read() {
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
