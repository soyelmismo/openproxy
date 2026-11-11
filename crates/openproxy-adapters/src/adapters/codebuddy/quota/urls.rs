//! Base URL, origin and endpoint constants for the CodeBuddy upstream.

pub const CODEBUDDY_ACCOUNTS_URL: &str = "https://www.workbuddy.ai/v2/accounts";

pub const CODEBUDDY_GET_USER_RESOURCE_URL: &str =
    "https://www.workbuddy.ai/billing/meter/get-user-resource";

pub const CODEBUDDY_GET_USER_RESOURCE_SUMMARY_URL: &str =
    "https://www.workbuddy.ai/billing/meter/get-user-resource-summary";

pub const DEFAULT_CODEBUDDY_BASE_URL: &str = "https://www.workbuddy.ai/v2";

pub const CN_CODEBUDDY_BASE_URL: &str = "https://www.workbuddy.cn/v2";

pub const MIRROR_CODEBUDDY_BASE_URL: &str = "https://copilot.tencent.com/v2";

pub const LEGACY_CODEBUDDY_BASE_URL: &str = "https://www.workbuddy.ai/v2";

/// `OPENPROXY_CODEBUDDY_BASE_URL`, then `OPENPROXY_CODEBUDDY_AUTH_BASE_URL`,
/// then [`DEFAULT_CODEBUDDY_BASE_URL`].
#[must_use]
pub fn codebuddy_base_url() -> String {
    std::env::var("OPENPROXY_CODEBUDDY_BASE_URL")
        .or_else(|_| std::env::var("OPENPROXY_CODEBUDDY_AUTH_BASE_URL"))
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_CODEBUDDY_BASE_URL.to_string())
}

/// Strips the `/v2` suffix, e.g. `https://www.workbuddy.ai/v2` ->
/// `https://www.workbuddy.ai`.
#[must_use]
pub fn codebuddy_origin_from_base_url(base: &str) -> String {
    let trimmed = base.trim_end_matches('/');
    if let Some(prefix) = trimmed.strip_suffix("/v2") {
        prefix.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Candidate origins in fallback order: configured, then `.ai`, `.cn` and
/// `copilot.tencent.com`. A localhost base URL suppresses the fallbacks.
#[must_use]
pub fn codebuddy_candidate_origins() -> Vec<String> {
    let configured_origin = codebuddy_origin_from_base_url(&codebuddy_base_url());
    let mut origins = vec![configured_origin.clone()];

    if configured_origin.contains("127.0.0.1") || configured_origin.contains("localhost") {
        return origins;
    }

    for fallback in [
        "https://www.workbuddy.ai",
        "https://www.workbuddy.cn",
        "https://copilot.tencent.com",
    ] {
        if !origins.iter().any(|o| o == fallback) {
            origins.push(fallback.to_string());
        }
    }

    origins
}
