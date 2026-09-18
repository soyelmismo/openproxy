//! Client spoofing traits and presets for upstream providers.
//!
//! Providers like Cline, Kilocode, Codex, OpenCode, and Antigravity require specific client
//! identity headers (User-Agent, machine fingerprint, editor metadata, etc.)
//! to accept requests. This module unifies spoofing into the [`ClientSpoofer`]
//! trait and provides standard presets.

use crate::upstream::UpstreamRequest;
use http::HeaderValue;

/// Trait for injecting client identity and spoofing headers into requests.
pub trait ClientSpoofer: Send + Sync {
    /// Return the full list of spoofed headers as `(name, value)` pairs.
    fn headers(&self) -> Vec<(String, String)>;

    /// Apply the spoofed headers to an [`UpstreamRequest`].
    fn apply_to_request(&self, req: &mut UpstreamRequest) {
        self.apply_to_header_map(&mut req.headers);
    }

    /// Apply the spoofed headers to an [`http::HeaderMap`].
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

/// Helper to parse JSON-encoded extra headers from an environment variable.
pub fn parse_env_extra_headers(env_var: &str) -> Vec<(String, String)> {
    let Ok(env_str) = std::env::var(env_var) else {
        return Vec::new();
    };
    let Ok(map) = serde_json::from_str::<std::collections::BTreeMap<String, String>>(&env_str) else {
        return Vec::new();
    };
    map.into_iter().collect()
}

/// Dynamic in-memory overrides for spoofer version and extra headers.
#[derive(Default, Debug)]
pub struct DynamicHeaderOverrides {
    version: std::sync::RwLock<Option<String>>,
    extra_headers: std::sync::RwLock<std::collections::BTreeMap<String, String>>,
}

impl DynamicHeaderOverrides {
    pub const fn new() -> Self {
        Self {
            version: std::sync::RwLock::new(None),
            extra_headers: std::sync::RwLock::new(std::collections::BTreeMap::new()),
        }
    }

    pub fn current_version(&self, env_var: &str, default_val: &str) -> String {
        if let Ok(guard) = self.version.read()
            && let Some(ref ver) = *guard
        {
            return ver.clone();
        }
        if let Ok(env_ver) = std::env::var(env_var)
            && !env_ver.trim().is_empty()
        {
            return env_ver;
        }
        default_val.to_string()
    }

    pub fn set_version(&self, ver: impl Into<String>) {
        if let Ok(mut guard) = self.version.write() {
            *guard = Some(ver.into());
        }
    }

    pub fn set_extra_header(&self, key: impl Into<String>, val: impl Into<String>) {
        if let Ok(mut guard) = self.extra_headers.write() {
            guard.insert(key.into(), val.into());
        }
    }

    pub fn current_extra_header(&self, key: &str) -> Option<String> {
        if let Ok(guard) = self.extra_headers.read() {
            guard.get(key).cloned()
        } else {
            None
        }
    }

    pub fn reset(&self) {
        if let Ok(mut guard) = self.version.write() {
            *guard = None;
        }
        if let Ok(mut guard) = self.extra_headers.write() {
            guard.clear();
        }
    }

    pub fn apply_to_list(&self, list: &mut Vec<(String, String)>) {
        if let Ok(lock) = self.extra_headers.read() {
            for (k, v) in lock.iter() {
                upsert_header(list, k, v.clone());
            }
        }
    }

    pub fn apply_to_header_map(&self, headers: &mut http::HeaderMap) {
        if let Ok(lock) = self.extra_headers.read() {
            for (k, v) in lock.iter() {
                if let Ok(name) = http::header::HeaderName::try_from(k.as_str())
                    && let Ok(val) = HeaderValue::try_from(v.as_str())
                {
                    headers.insert(name, val);
                }
            }
        }
    }
}

/// Case-insensitively insert or update a header in a list of `(String, String)` pairs.
/// Avoids allocating a new `String` for the key if it is already present in the list.
pub fn upsert_header(headers: &mut Vec<(String, String)>, key: &str, val: impl Into<String>) {
    if let Some(pos) = headers.iter().position(|(hk, _)| hk.eq_ignore_ascii_case(key)) {
        headers[pos].1 = val.into();
    } else {
        headers.push((key.to_string(), val.into()));
    }
}

/// Case-insensitively merge borrowed header pairs into an existing list.
pub fn merge_header_refs<'a, I>(headers: &mut Vec<(String, String)>, extra: I)
where
    I: IntoIterator<Item = &'a (String, String)>,
{
    for (k, v) in extra {
        upsert_header(headers, k, v.clone());
    }
}

pub mod antigravity;
pub mod cline;
pub mod codex;
pub mod kilocode;
pub mod minimax;
pub mod opencode;

pub use antigravity::*;
pub use cline::*;
pub use codex::*;
pub use kilocode::*;
pub use minimax::*;
pub use opencode::*;

#[cfg(test)]
mod tests;
