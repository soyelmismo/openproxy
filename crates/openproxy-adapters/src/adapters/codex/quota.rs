use crate::adapters::codex::apply_codex_spoofing_headers;
use crate::upstream::UpstreamRequest;
use openproxy_types::{AccountQuota, CoreError, ModelQuotaDetail, Result};

pub fn build_codex_quota_request(
    access_token: &str,
    workspace_id: Option<&str>,
) -> UpstreamRequest {
    let url = "https://chatgpt.com/backend-api/wham/usage";
    let mut req = UpstreamRequest::get(url);
    req.headers.insert(
        http::header::AUTHORIZATION,
        http::HeaderValue::from_str(&format!("Bearer {access_token}"))
            .unwrap_or_else(|_| http::HeaderValue::from_static("")),
    );
    req.headers.insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("application/json"),
    );
    req.headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    apply_codex_spoofing_headers(&mut req);
    let workspace_header = workspace_id.and_then(codex_workspace_header);
    if let Some(ws) = workspace_header.as_deref()
        && let Ok(val) = http::HeaderValue::from_str(ws)
    {
        req.headers
            .insert(http::HeaderName::from_static("chatgpt-account-id"), val);
    }
    req
}

pub fn build_codex_error_quota(status: u16, snippet: &str) -> AccountQuota {
    let fetch_error = if snippet.is_empty() {
        format!("Codex quota check failed: HTTP {status}")
    } else {
        format!("Codex quota check failed: HTTP {status}: {snippet}")
    };
    AccountQuota::with_error(fetch_error)
}

pub fn codex_workspace_header(provider_specific: &str) -> Option<String> {
    let raw = provider_specific.trim();
    if raw.is_empty() {
        return None;
    }
    if !raw.starts_with('{') {
        return Some(raw.to_string());
    }
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|v| {
            v.get("workspaceId")
                .or_else(|| v.get("workspace_id"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(ToString::to_string)
        })
}

pub fn parse_codex_usage_quota(body: &serde_json::Value) -> Result<AccountQuota> {
    let rate_limit = body
        .get("rate_limit")
        .or_else(|| body.get("rateLimit"))
        .and_then(|v| v.as_object())
        .ok_or_else(|| CoreError::Parse("codex quota missing rate_limit".into()))?;

    let primary = rate_limit
        .get("primary_window")
        .or_else(|| rate_limit.get("primaryWindow"));
    let secondary = rate_limit
        .get("secondary_window")
        .or_else(|| rate_limit.get("secondaryWindow"));
    let (session_used, session_reset_at) = parse_codex_usage_window(primary);
    let (weekly_used, weekly_reset_at) = parse_codex_usage_window(secondary);
    let model_details = parse_codex_additional_rate_limits(body);

    Ok(AccountQuota {
        session_used,
        session_limit: session_used.map(|_| 100),
        session_reset_at,
        weekly_used,
        weekly_limit: weekly_used.map(|_| 100),
        weekly_reset_at,
        plan_name: Some("Codex / ChatGPT".into()),
        last_fetched_at: openproxy_types::now_unix_secs_str(),
        fetch_error: None,
        model_details,
    })
}

fn parse_codex_additional_rate_limits(body: &serde_json::Value) -> Option<Box<[ModelQuotaDetail]>> {
    let list = body
        .get("additional_rate_limits")
        .or_else(|| body.get("additionalRateLimits"))?
        .as_array()?;

    let mut details = Vec::new();
    for entry in list {
        let Some(name) = entry
            .get("limit_name")
            .or_else(|| entry.get("limitName"))
            .or_else(|| entry.get("name"))
            .or_else(|| entry.get("model"))
            .or_else(|| entry.get("metered_feature"))
            .or_else(|| entry.get("meteredFeature"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
        else {
            continue;
        };

        let rate_limit = entry
            .get("rate_limit")
            .or_else(|| entry.get("rateLimit"))
            .and_then(|v| v.as_object());

        let window = rate_limit.and_then(|rl| {
            rl.get("primary_window")
                .or_else(|| rl.get("primaryWindow"))
                .or_else(|| rl.get("secondary_window"))
                .or_else(|| rl.get("secondaryWindow"))
        });

        if let (Some(used), reset_at) = parse_codex_usage_window(window) {
            let clamped_used = used.clamp(0, 100);
            details.push(ModelQuotaDetail {
                model_id: name.to_string(),
                session_used: clamped_used,
                session_limit: 100,
                session_reset_at: reset_at,
                remaining_fraction: ((100 - clamped_used) as f64) / 100.0,
            });
        }
    }

    if details.is_empty() {
        None
    } else {
        Some(details.into_boxed_slice())
    }
}

fn parse_codex_usage_window(window: Option<&serde_json::Value>) -> (Option<i64>, Option<String>) {
    let Some(window) = window.and_then(|v| v.as_object()) else {
        return (None, None);
    };
    let used = window
        .get("used_percent")
        .or_else(|| window.get("usedPercent"))
        .and_then(json_f64)
        .map(|v| v.round().clamp(0.0, 100.0) as i64);
    let reset_at = window
        .get("reset_at")
        .or_else(|| window.get("resetAt"))
        .and_then(json_f64)
        .filter(|v| *v > 0.0)
        .map(|v| (v.ceil() as u64).to_string())
        .or_else(|| {
            window
                .get("reset_after_seconds")
                .or_else(|| window.get("resetAfterSeconds"))
                .and_then(json_f64)
                .filter(|v| *v > 0.0)
                .map(|v| {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_secs());
                    (now + v.ceil() as u64).to_string()
                })
        });
    (used, reset_at)
}

fn json_f64(value: &serde_json::Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.parse::<f64>().ok()))
}
