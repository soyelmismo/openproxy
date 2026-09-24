use super::{ClientSpoofer, DynamicHeaderOverrides, merge_header_refs, parse_env_extra_headers};
use crate::upstream::{CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest};
use http::HeaderValue;
use std::sync::Arc;

pub const DEFAULT_CODEX_VERSION: &str = "0.156.1";

pub const CODEX_SPOOFING_HEADERS: &[(&str, &str)] = &[
    ("origin", "https://chatgpt.com"),
    ("originator", "codex_cli_rs"),
    ("version", "0.156.1"),
    ("user-agent", "codex-cli/0.156.1 (Windows 10.0.26200; x64)"),
];

pub const CODEX_LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/openai/codex/releases/latest";

static CODEX_OVERRIDES: DynamicHeaderOverrides = DynamicHeaderOverrides::new();

fn safe_env_value(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// Canonical release URL for Codex CLI on GitHub, configurable via `OPENPROXY_CODEX_LATEST_RELEASE_URL`.
pub fn codex_latest_release_url() -> String {
    safe_env_value("OPENPROXY_CODEX_LATEST_RELEASE_URL")
        .unwrap_or_else(|| CODEX_LATEST_RELEASE_URL.to_string())
}

/// Current dynamic version of Codex CLI.
pub fn current_codex_version() -> String {
    let ver = CODEX_OVERRIDES.current_version("OPENPROXY_CODEX_CLIENT_VERSION", "");
    if !ver.is_empty() {
        return ver;
    }
    safe_env_value("CODEX_CLIENT_VERSION").unwrap_or_else(|| DEFAULT_CODEX_VERSION.to_string())
}

/// Dynamic Codex User-Agent string.
pub fn current_codex_ua() -> String {
    if let Some(override_ua) = CODEX_OVERRIDES.current_extra_header("user-agent") {
        return override_ua;
    }
    if let Some(env_ua) =
        safe_env_value("OPENPROXY_CODEX_USER_AGENT").or_else(|| safe_env_value("CODEX_USER_AGENT"))
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
    CODEX_OVERRIDES.set_version(ver);
}

/// Set dynamic User-Agent override for Codex in memory at runtime without recompiling.
pub fn set_dynamic_codex_ua(ua: impl Into<String>) {
    CODEX_OVERRIDES.set_extra_header("user-agent", ua);
}

/// Set dynamic extra header override for Codex in memory at runtime without recompiling.
pub fn set_dynamic_codex_extra_header(key: impl Into<String>, val: impl Into<String>) {
    CODEX_OVERRIDES.set_extra_header(key, val);
}

/// Reset dynamic in-memory overrides for Codex (useful for tests and cleanup).
pub fn reset_dynamic_codex_overrides() {
    CODEX_OVERRIDES.reset();
}

/// Parse `(major, minor, patch)` from a Codex User-Agent string.
pub fn parse_codex_version(ua: &str) -> Option<(u32, u32, u32)> {
    let lower = ua.to_ascii_lowercase();
    let idx = if let Some(i) = lower.find("codex-cli/") {
        i + "codex-cli/".len()
    } else {
        let i = lower.find("codex/")?;
        i + "codex/".len()
    };

    let rest = &ua[idx..];
    let mut parts = rest.split('.');
    let major = parts.next()?.trim().parse::<u32>().ok()?;
    let minor = parts.next()?.trim().parse::<u32>().ok()?;
    let patch_part = parts.next().unwrap_or("0");
    let patch_digits: String = patch_part
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let patch = patch_digits.parse::<u32>().unwrap_or(0);

    Some((major, minor, patch))
}

/// Check if User-Agent specifies a valid and supported Codex version (>= 0.156.0).
pub fn has_valid_codex_version(ua: &str) -> bool {
    parse_codex_version(ua)
        .is_some_and(|(major, minor, _)| major > 0 || (major == 0 && minor >= 156))
}

/// Asynchronously queries GitHub releases for the latest Codex CLI version
/// and updates the in-memory dynamic version state if changed.
pub async fn refresh_codex_version(upstream_client: &Arc<UpstreamClient>) -> Option<String> {
    let url = codex_latest_release_url();
    let mut req = UpstreamRequest::get(&url);
    if let Ok(accept) = HeaderValue::from_str("application/vnd.github.v3+json, application/json") {
        req.headers.insert(http::header::ACCEPT, accept);
    }
    if let Ok(ua) = HeaderValue::from_str(&current_codex_ua()) {
        req.headers.insert(http::header::USER_AGENT, ua);
    }

    let cancel = CancellationToken::new();
    let resp = upstream_client
        .call(req, TimeoutProfile::OAuth, cancel)
        .await
        .ok()?;

    if !resp.status.is_success() {
        return None;
    }

    let body = resp.collect().await.ok()?;
    let json: serde_json::Value = serde_json::from_slice(&body).ok()?;

    let tag_or_version = json
        .get("tag_name")
        .or_else(|| json.get("name"))
        .or_else(|| json.get("version"))
        .and_then(|v| v.as_str())?;

    let clean_ver = tag_or_version
        .trim()
        .trim_start_matches("rust-v")
        .trim_start_matches('v')
        .split('-')
        .next()?
        .trim();

    if !clean_ver.is_empty() && has_valid_codex_version(&format!("codex-cli/{clean_ver}")) {
        set_dynamic_codex_version(clean_ver);
        return Some(clean_ver.to_string());
    }

    None
}

#[cfg(any(test, feature = "test-utils"))]
pub static CODEX_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(any(test, feature = "test-utils"))]
pub static CODEX_ASYNC_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

static CODEX_EXTRA_HEADERS: std::sync::LazyLock<Vec<(String, String)>> =
    std::sync::LazyLock::new(|| parse_env_extra_headers("OPENPROXY_CODEX_EXTRA_HEADERS"));

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

        merge_header_refs(&mut list, &*CODEX_EXTRA_HEADERS);
        CODEX_OVERRIDES.apply_to_list(&mut list);

        list
    }

    fn apply_to_header_map(&self, headers: &mut http::HeaderMap) {
        // 1. User-Agent: preserve valid downstream codex >= 0.156, else upgrade to current_codex_ua()
        let preserve_ua = headers
            .get(http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .is_some_and(has_valid_codex_version);

        let cur_ua = current_codex_ua();
        if !preserve_ua && let Ok(val) = HeaderValue::try_from(cur_ua.as_str()) {
            headers.insert(http::header::USER_AGENT, val);
        }

        // 2. Identity headers
        if let Ok(name) = http::header::HeaderName::try_from("origin")
            && !headers.contains_key(&name)
            && let Ok(val) = HeaderValue::try_from("https://chatgpt.com")
        {
            headers.insert(name, val);
        }
        if let Ok(name) = http::header::HeaderName::try_from("originator")
            && !headers.contains_key(&name)
            && let Ok(val) = HeaderValue::try_from("codex_cli_rs")
        {
            headers.insert(name, val);
        }

        // 3. Version header: upgrade if missing or outdated (< 0.156.0)
        let cur_ver = current_codex_version();
        let need_version = headers
            .get("version")
            .and_then(|v| v.to_str().ok())
            .is_none_or(|v| !has_valid_codex_version(&format!("codex-cli/{v}")));

        if need_version
            && let Ok(val) = HeaderValue::try_from(cur_ver.as_str())
            && let Ok(name) = http::header::HeaderName::try_from("version")
        {
            headers.insert(name, val);
        }

        // 4. Extra headers (static + dynamic)
        for (k, v) in CODEX_EXTRA_HEADERS.iter() {
            if let Ok(name) = http::header::HeaderName::try_from(k.as_str())
                && !headers.contains_key(&name)
                && let Ok(val) = HeaderValue::try_from(v.as_str())
            {
                headers.insert(name, val);
            }
        }
        CODEX_OVERRIDES.apply_to_header_map(headers);
    }
}
