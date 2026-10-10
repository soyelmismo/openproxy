use crate::adapters::codex::apply_codex_spoofing_headers;
use crate::upstream::UpstreamRequest;
use openproxy_types::{AccountQuota, CoreError, ModelQuotaDetail, Result};

fn build_codex_base_request(
    url: &str,
    method: http::Method,
    access_token: &str,
    workspace_id: Option<&str>,
) -> UpstreamRequest {
    let mut req = UpstreamRequest::get(url);
    req.method = method;
    req.headers.insert(
        http::header::AUTHORIZATION,
        crate::antigravity_headers::build_bearer_header(access_token)
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

pub fn build_codex_quota_request(
    access_token: &str,
    workspace_id: Option<&str>,
) -> UpstreamRequest {
    build_codex_base_request(
        "https://chatgpt.com/backend-api/wham/usage",
        http::Method::GET,
        access_token,
        workspace_id,
    )
}

pub fn build_codex_reset_credits_request(
    access_token: &str,
    workspace_id: Option<&str>,
) -> UpstreamRequest {
    build_codex_base_request(
        "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits",
        http::Method::GET,
        access_token,
        workspace_id,
    )
}

pub fn build_codex_consume_reset_request(
    access_token: &str,
    workspace_id: Option<&str>,
    redeem_request_id: &str,
    credit_id: &str,
) -> UpstreamRequest {
    let mut req = build_codex_base_request(
        "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume",
        http::Method::POST,
        access_token,
        workspace_id,
    );
    let payload = serde_json::json!({
        "redeem_request_id": redeem_request_id,
        "credit_id": credit_id,
    });
    req.body = Some(bytes::Bytes::from(
        serde_json::to_vec(&payload).unwrap_or_default(),
    ));
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
        pools: None,
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

        let window = rate_limit
            .and_then(|rl| {
                rl.get("primary_window")
                    .or_else(|| rl.get("primaryWindow"))
                    .or_else(|| rl.get("secondary_window"))
                    .or_else(|| rl.get("secondaryWindow"))
            })
            .or_else(|| {
                entry
                    .get("primary_window")
                    .or_else(|| entry.get("primaryWindow"))
                    .or_else(|| entry.get("secondary_window"))
                    .or_else(|| entry.get("secondaryWindow"))
                    .or_else(|| entry.get("window"))
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

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CodexResetCredit {
    pub id: String,
    pub reset_type: Option<String>,
    pub status: Option<String>,
    pub expires_at: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexResetOutcome {
    Reset,
    AlreadyRedeemed,
    NoCredit,
    NothingToReset,
}

impl CodexResetOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Reset => "reset",
            Self::AlreadyRedeemed => "alreadyRedeemed",
            Self::NoCredit => "no_credit",
            Self::NothingToReset => "nothing_to_reset",
        }
    }
}

pub fn parse_codex_reset_credits_count(body: &serde_json::Value) -> Option<u32> {
    body.get("rate_limit_reset_credits")
        .or_else(|| body.get("rateLimitResetCredits"))
        .and_then(|v| {
            v.get("available_count")
                .or_else(|| v.get("availableCount"))
                .and_then(|c| c.as_u64())
                .map(|c| c as u32)
        })
}

pub fn parse_codex_reset_credits(body: &serde_json::Value) -> Result<(Vec<CodexResetCredit>, u32)> {
    let candidates = if let Some(arr) = body.as_array() {
        arr.as_slice()
    } else {
        const CANDIDATE_KEYS: &[&str] = &[
            "credits",
            "reset_credits",
            "resetCredits",
            "rate_limit_reset_credits",
            "rateLimitResetCredits",
            "items",
            "data",
        ];
        CANDIDATE_KEYS
            .iter()
            .find_map(|k| body.get(*k).and_then(|v| v.as_array()))
            .map_or(&[] as &[serde_json::Value], Vec::as_slice)
    };

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());

    let mut credits = Vec::new();
    for item in candidates {
        let Some(item_obj) = item.as_object() else {
            continue;
        };
        let Some(id) = item_obj
            .get("credit_id")
            .or_else(|| item_obj.get("creditId"))
            .or_else(|| item_obj.get("id"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
        else {
            continue;
        };

        let status = item_obj
            .get("status")
            .or_else(|| item_obj.get("state"))
            .or_else(|| item_obj.get("outcome"))
            .or_else(|| item_obj.get("result"))
            .or_else(|| item_obj.get("code"))
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_lowercase());

        if let Some(ref st) = status
            && matches!(
                st.as_str(),
                "consumed" | "redeeming" | "redeemed" | "used" | "expired" | "unavailable"
            )
        {
            continue;
        }
        if item_obj.get("consumed").and_then(|v| v.as_bool()) == Some(true)
            || item_obj.get("redeemed").and_then(|v| v.as_bool()) == Some(true)
            || item_obj.get("available").and_then(|v| v.as_bool()) == Some(false)
        {
            continue;
        }

        let expires_at = item_obj
            .get("expires_at")
            .or_else(|| item_obj.get("expiresAt"))
            .or_else(|| item_obj.get("expiration_at"))
            .or_else(|| item_obj.get("expirationAt"))
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string());

        if let Some(ref exp) = expires_at {
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(exp) {
                if dt.timestamp() as u64 <= now_secs {
                    continue;
                }
            } else if let Ok(exp_secs) = exp.parse::<u64>()
                && exp_secs <= now_secs
            {
                continue;
            }
        }

        let reset_type = item_obj
            .get("reset_type")
            .or_else(|| item_obj.get("resetType"))
            .and_then(|v| v.as_str())
            .map(ToString::to_string);
        let title = item_obj
            .get("title")
            .and_then(|v| v.as_str())
            .map(ToString::to_string);
        let description = item_obj
            .get("description")
            .and_then(|v| v.as_str())
            .map(ToString::to_string);

        credits.push(CodexResetCredit {
            id: id.to_string(),
            reset_type,
            status,
            expires_at,
            title,
            description,
        });
    }

    credits.sort_by(|a, b| match (&a.expires_at, &b.expires_at) {
        (Some(ea), Some(eb)) => ea.cmp(eb),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });

    let reported_count = body
        .get("available_count")
        .or_else(|| body.get("availableCount"))
        .and_then(|v| v.as_u64())
        .map_or(credits.len() as u32, |v| v as u32);

    Ok((credits, reported_count))
}

pub fn parse_codex_consume_response(
    status: u16,
    body: &serde_json::Value,
) -> Result<CodexResetOutcome> {
    fn normalize_outcome(s: &str) -> String {
        s.chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_lowercase()
    }

    let direct = body.as_str().map(normalize_outcome);
    let extracted = direct.or_else(|| {
        const OUTCOME_KEYS: &[&str] = &["code", "outcome", "status", "result", "type"];
        OUTCOME_KEYS
            .iter()
            .find_map(|k| body.get(*k).and_then(|v| v.as_str()).map(normalize_outcome))
    });

    if let Some(ref outcome) = extracted {
        match outcome.as_str() {
            "reset" => return Ok(CodexResetOutcome::Reset),
            "alreadyredeemed" => return Ok(CodexResetOutcome::AlreadyRedeemed),
            "nocredit" | "nocredits" => return Ok(CodexResetOutcome::NoCredit),
            "nothingtoreset" => return Ok(CodexResetOutcome::NothingToReset),
            _ => {}
        }
    }

    if status == 409
        && let Some(err) = body
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_str())
    {
        let norm = normalize_outcome(err);
        if norm.contains("nocredit") {
            return Ok(CodexResetOutcome::NoCredit);
        }
        if norm.contains("nothingtoreset") {
            return Ok(CodexResetOutcome::NothingToReset);
        }
    }

    if (200..300).contains(&status) {
        Ok(CodexResetOutcome::Reset)
    } else {
        Err(CoreError::upstream_error(
            status,
            "codex",
            "codex-reset",
            body.to_string(),
            false,
        ))
    }
}
