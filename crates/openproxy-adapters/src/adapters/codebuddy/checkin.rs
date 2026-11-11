use crate::adapters::{
    Arc, CancellationToken, CoreError, Result, TimeoutProfile, UpstreamClient, UpstreamRequest,
};
use super::apply_codebuddy_spoofing_headers;
use http::HeaderValue;

pub const CODEBUDDY_CHECKIN_STATUS_URL: &str =
    "https://copilot.tencent.com/v2/billing/meter/checkin-activity-status";

pub const CODEBUDDY_DAILY_CHECKIN_URL: &str =
    "https://copilot.tencent.com/v2/billing/meter/daily-checkin";

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CodeBuddyCheckinStatus {
    AlreadyCheckedIn,
    CanCheckIn,
    ActivityInactive,
    Other { code: i64, message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CodeBuddyActivityStatus {
    pub status: CodeBuddyCheckinStatus,
    pub raw_code: i64,
    pub message: String,
    pub has_checked_in: bool,
    pub activity_active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CodeBuddyClaimResult {
    pub success: bool,
    pub newly_claimed: bool,
    pub credits_granted: Option<i64>,
    pub status: CodeBuddyCheckinStatus,
    pub raw_code: i64,
    pub message: String,
}

/// Builds an UpstreamRequest for CodeBuddy check-in or activity status.
#[must_use]
pub fn build_codebuddy_checkin_request(
    url: &str,
    token: &str,
    proxy_url: Option<&str>,
) -> UpstreamRequest {
    let mut req = UpstreamRequest::post_json(url, bytes::Bytes::from_static(b"{}"));
    req.proxy = proxy_url.map(ToString::to_string);

    let trimmed = token.trim();
    let auth_val = if trimmed.starts_with("Bearer ") {
        trimmed.to_string()
    } else {
        format!("Bearer {trimmed}")
    };
    if let Ok(hv) = HeaderValue::from_str(&auth_val) {
        req.headers.insert(http::header::AUTHORIZATION, hv);
    }
    req.headers.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    req.headers.insert(
        http::header::ACCEPT,
        HeaderValue::from_static("application/json"),
    );

    // Apply baseline CodeBuddy spoofing headers
    apply_codebuddy_spoofing_headers(&mut req);

    // Enforce required check-in identity headers
    req.headers.insert(
        http::HeaderName::from_static("x-product"),
        HeaderValue::from_static("codebuddy"),
    );
    req.headers.insert(
        http::HeaderName::from_static("x-product-code"),
        HeaderValue::from_static("codebuddy"),
    );

    if let Ok(uri) = url.parse::<http::Uri>()
        && let Some(host) = uri.host()
        && let Ok(val) = HeaderValue::from_str(host)
    {
        req.headers.insert(http::HeaderName::from_static("x-domain"), val);
    } else {
        req.headers.insert(
            http::HeaderName::from_static("x-domain"),
            HeaderValue::from_static("copilot.tencent.com"),
        );
    }

    req
}

/// Parses daily-checkin claim response.
#[must_use]
pub fn parse_codebuddy_claim_response(
    http_status: u16,
    json: &serde_json::Value,
) -> CodeBuddyClaimResult {
    let code = json
        .get("code")
        .and_then(serde_json::Value::as_i64)
        .or_else(|| {
            json.pointer("/Response/code")
                .or_else(|| json.pointer("/data/code"))
                .and_then(serde_json::Value::as_i64)
        })
        .unwrap_or(if http_status == 200 { 0 } else { http_status as i64 });

    let message = json
        .get("msg")
        .or_else(|| json.get("message"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();

    match code {
        0 => {
            let credits = json
                .pointer("/data/credits")
                .or_else(|| json.pointer("/data/score"))
                .or_else(|| json.pointer("/data/reward"))
                .or_else(|| json.pointer("/data/amount"))
                .and_then(serde_json::Value::as_i64)
                .or(Some(100));

            CodeBuddyClaimResult {
                success: true,
                newly_claimed: true,
                credits_granted: credits,
                status: CodeBuddyCheckinStatus::CanCheckIn,
                raw_code: 0,
                message: if message.is_empty() {
                    "ok".to_string()
                } else {
                    message
                },
            }
        }
        10001 => {
            let is_already = message.contains("已签到") || message.to_ascii_lowercase().contains("already");
            if is_already {
                CodeBuddyClaimResult {
                    success: true,
                    newly_claimed: false,
                    credits_granted: None,
                    status: CodeBuddyCheckinStatus::AlreadyCheckedIn,
                    raw_code: 10001,
                    message: if message.is_empty() {
                        "already checked in today".to_string()
                    } else {
                        message
                    },
                }
            } else {
                CodeBuddyClaimResult {
                    success: false,
                    newly_claimed: false,
                    credits_granted: None,
                    status: CodeBuddyCheckinStatus::ActivityInactive,
                    raw_code: 10001,
                    message,
                }
            }
        }
        1003 => CodeBuddyClaimResult {
            success: false,
            newly_claimed: false,
            credits_granted: None,
            status: CodeBuddyCheckinStatus::ActivityInactive,
            raw_code: 1003,
            message: if message.is_empty() {
                "activity inactive or ended".to_string()
            } else {
                message
            },
        },
        other => CodeBuddyClaimResult {
            success: false,
            newly_claimed: false,
            credits_granted: None,
            status: CodeBuddyCheckinStatus::Other {
                code: other,
                message: message.clone(),
            },
            raw_code: other,
            message,
        },
    }
}

/// Parses checkin-activity-status response.
#[must_use]
pub fn parse_codebuddy_status_response(
    http_status: u16,
    json: &serde_json::Value,
) -> CodeBuddyActivityStatus {
    let code = json
        .get("code")
        .and_then(serde_json::Value::as_i64)
        .or_else(|| {
            json.pointer("/Response/code")
                .or_else(|| json.pointer("/data/code"))
                .and_then(serde_json::Value::as_i64)
        })
        .unwrap_or(if http_status == 200 { 0 } else { http_status as i64 });

    let message = json
        .get("msg")
        .or_else(|| json.get("message"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();

    match code {
        0 => {
            let active = json
                .pointer("/data/active")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true);

            let has_checked_in = json
                .pointer("/data/today_checked_in")
                .or_else(|| json.pointer("/data/has_checked_in"))
                .or_else(|| json.pointer("/data/is_checked_in"))
                .or_else(|| json.pointer("/data/hasCheckedIn"))
                .or_else(|| json.pointer("/data/isCheckedIn"))
                .or_else(|| json.pointer("/data/checked_in"))
                .and_then(serde_json::Value::as_bool)
                .or_else(|| {
                    json.pointer("/data/status")
                        .and_then(serde_json::Value::as_i64)
                        .map(|s| s == 1)
                })
                .unwrap_or(false);

            let status = if !active {
                CodeBuddyCheckinStatus::ActivityInactive
            } else if has_checked_in {
                CodeBuddyCheckinStatus::AlreadyCheckedIn
            } else {
                CodeBuddyCheckinStatus::CanCheckIn
            };

            CodeBuddyActivityStatus {
                status,
                raw_code: 0,
                message: if message.is_empty() {
                    "ok".to_string()
                } else {
                    message
                },
                has_checked_in,
                activity_active: active,
            }
        }
        10001 => CodeBuddyActivityStatus {
            status: CodeBuddyCheckinStatus::AlreadyCheckedIn,
            raw_code: 10001,
            message: if message.is_empty() {
                "already checked in today".to_string()
            } else {
                message
            },
            has_checked_in: true,
            activity_active: true,
        },
        1003 => CodeBuddyActivityStatus {
            status: CodeBuddyCheckinStatus::ActivityInactive,
            raw_code: 1003,
            message: if message.is_empty() {
                "activity inactive or ended".to_string()
            } else {
                message
            },
            has_checked_in: false,
            activity_active: false,
        },
        other => CodeBuddyActivityStatus {
            status: CodeBuddyCheckinStatus::Other {
                code: other,
                message: message.clone(),
            },
            raw_code: other,
            message,
            has_checked_in: false,
            activity_active: false,
        },
    }
}

/// Queries check-in activity status for the authenticated account.
pub async fn query_codebuddy_checkin_status(
    upstream: &Arc<UpstreamClient>,
    token: &str,
    proxy_url: Option<&str>,
) -> Result<CodeBuddyActivityStatus> {
    if let Ok(custom) = std::env::var("OPENPROXY_CODEBUDDY_CHECKIN_STATUS_URL")
        && !custom.trim().is_empty()
    {
        let req = build_codebuddy_checkin_request(&custom, token, proxy_url);
        let cancel = CancellationToken::new();
        let resp = upstream
            .call(req, TimeoutProfile::Quota, cancel)
            .await
            .map_err(|e| CoreError::UpstreamConnection(format!("codebuddy checkin status: {e}")))?;
        let status = resp.status;
        let body = resp
            .collect()
            .await
            .map_err(|e| CoreError::UpstreamConnection(format!("codebuddy checkin status read: {e}")))?;
        let json: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|e| CoreError::Parse(format!("codebuddy checkin status parse: {e}")))?;
        return Ok(parse_codebuddy_status_response(status.as_u16(), &json));
    }

    let candidate_origins = super::quota::codebuddy_candidate_origins();
    let mut last_status = None;

    for origin in &candidate_origins {
        for path in ["/v2/billing/meter/checkin-activity-status", "/billing/meter/checkin-activity-status"] {
            let url = format!("{origin}{path}");
            let req = build_codebuddy_checkin_request(&url, token, proxy_url);
            let cancel = CancellationToken::new();
            if let Ok(resp) = upstream.call(req, TimeoutProfile::Quota, cancel).await {
                let status_code = resp.status.as_u16();
                if let Ok(body) = resp.collect().await
                    && let Ok(json) = serde_json::from_slice::<serde_json::Value>(&body)
                {
                    let parsed = parse_codebuddy_status_response(status_code, &json);
                    if parsed.raw_code == 0 {
                        return Ok(parsed);
                    }
                    last_status = Some(parsed);
                }
            }
        }
    }

    Ok(last_status.unwrap_or(CodeBuddyActivityStatus {
        status: CodeBuddyCheckinStatus::ActivityInactive,
        raw_code: -1,
        message: "checkin activity unavailable".into(),
        has_checked_in: false,
        activity_active: false,
    }))
}

/// Claims daily check-in credits for the authenticated account.
pub async fn claim_codebuddy_daily_checkin(
    upstream: &Arc<UpstreamClient>,
    token: &str,
    proxy_url: Option<&str>,
) -> Result<CodeBuddyClaimResult> {
    if let Ok(custom) = std::env::var("OPENPROXY_CODEBUDDY_DAILY_CHECKIN_URL")
        && !custom.trim().is_empty()
    {
        let req = build_codebuddy_checkin_request(&custom, token, proxy_url);
        let cancel = CancellationToken::new();
        let resp = upstream
            .call(req, TimeoutProfile::Quota, cancel)
            .await
            .map_err(|e| CoreError::UpstreamConnection(format!("codebuddy daily checkin: {e}")))?;
        let status = resp.status;
        let body = resp
            .collect()
            .await
            .map_err(|e| CoreError::UpstreamConnection(format!("codebuddy daily checkin read: {e}")))?;
        let json: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|e| CoreError::Parse(format!("codebuddy daily checkin parse: {e}")))?;
        return Ok(parse_codebuddy_claim_response(status.as_u16(), &json));
    }

    let candidate_origins = super::quota::codebuddy_candidate_origins();
    let mut last_result = None;

    for origin in &candidate_origins {
        for path in ["/v2/billing/meter/daily-checkin", "/billing/meter/daily-checkin"] {
            let url = format!("{origin}{path}");
            let req = build_codebuddy_checkin_request(&url, token, proxy_url);
            let cancel = CancellationToken::new();
            if let Ok(resp) = upstream.call(req, TimeoutProfile::Quota, cancel).await {
                let status_code = resp.status.as_u16();
                if let Ok(body) = resp.collect().await
                    && let Ok(json) = serde_json::from_slice::<serde_json::Value>(&body)
                {
                    let parsed = parse_codebuddy_claim_response(status_code, &json);
                    if parsed.success || parsed.raw_code == 10001 || parsed.raw_code == 1003 {
                        return Ok(parsed);
                    }
                    last_result = Some(parsed);
                }
            }
        }
    }

    Ok(last_result.unwrap_or(CodeBuddyClaimResult {
        success: false,
        newly_claimed: false,
        credits_granted: None,
        status: CodeBuddyCheckinStatus::ActivityInactive,
        raw_code: -1,
        message: "no candidate origins accepted daily checkin".into(),
    }))
}

/// Calculates the updated consecutive check-in streak based on the previous check-in date
/// and reference date `today`.
///
/// Rules:
/// - If `last_checkin_date` is today: returns existing `current_streak` (or at least 1).
/// - If `last_checkin_date` is yesterday: returns `current_streak.unwrap_or(0) + 1`.
/// - If `last_checkin_date` is missing, older than yesterday, or invalid: returns `1`.
#[must_use]
pub fn calculate_codebuddy_streak(
    last_checkin_date: Option<&str>,
    current_streak: Option<i64>,
    today: chrono::NaiveDate,
) -> i64 {
    let Some(last_str) = last_checkin_date.map(str::trim).filter(|s| !s.is_empty()) else {
        return 1;
    };
    let Ok(last_date) = chrono::NaiveDate::parse_from_str(last_str, "%Y-%m-%d") else {
        return 1;
    };

    let streak = current_streak.unwrap_or(0).max(0);

    if last_date == today {
        streak.max(1)
    } else if let Some(yesterday) = today.pred_opt()
        && last_date == yesterday
    {
        streak + 1
    } else {
        1
    }
}

/// Convenience helper to calculate streak against today's UTC date.
#[must_use]
pub fn update_codebuddy_streak_today(
    last_checkin_date: Option<&str>,
    current_streak: Option<i64>,
) -> (String, i64) {
    let today = chrono::Utc::now().date_naive();
    let streak = calculate_codebuddy_streak(last_checkin_date, current_streak, today);
    (today.format("%Y-%m-%d").to_string(), streak)
}

/// Queries CodeBuddy meter or accounts endpoints to fetch current `(credit_balance, total_credits)`.
pub async fn fetch_codebuddy_credits(
    upstream: &Arc<UpstreamClient>,
    token: &str,
    proxy_url: Option<&str>,
) -> Result<Option<(i64, i64)>> {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let candidate_origins = super::quota::codebuddy_candidate_origins();

    for origin in &candidate_origins {
        let resource_url = format!("{origin}/billing/meter/get-user-resource");
        let summary_url = format!("{origin}/billing/meter/get-user-resource-summary");
        let accounts_url = format!("{origin}/v2/accounts");

        for meter_url in [&resource_url, &summary_url] {
            let req =
                super::quota::build_codebuddy_resource_request(meter_url, trimmed, proxy_url);
            let cancel = CancellationToken::new();
            if let Ok(response) = upstream.call(req, TimeoutProfile::Quota, cancel).await
                && response.status.is_success()
                && let Ok(body) = response.collect().await
                && let Ok(json) = serde_json::from_slice::<serde_json::Value>(&body)
                && !super::quota::is_codebuddy_auth_error(&json)
                && let Some(credits) = super::quota::parse_codebuddy_credit_balance(&json)
            {
                return Ok(Some(credits));
            }
        }

        let req_accounts = super::quota::build_codebuddy_accounts_request_with_url(
            &accounts_url,
            trimmed,
            proxy_url,
        );
        let cancel = CancellationToken::new();
        if let Ok(response) = upstream.call(req_accounts, TimeoutProfile::Quota, cancel).await
            && response.status.is_success()
            && let Ok(body) = response.collect().await
            && let Ok(json) = serde_json::from_slice::<serde_json::Value>(&body)
            && !super::quota::is_codebuddy_auth_error(&json)
            && let Some(credits) = super::quota::parse_codebuddy_credit_balance(&json)
        {
            return Ok(Some(credits));
        }
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_codebuddy_checkin_request_headers() {
        let req = build_codebuddy_checkin_request(
            CODEBUDDY_DAILY_CHECKIN_URL,
            "test_token_123",
            Some("http://proxy.local:8080"),
        );

        assert_eq!(req.proxy.as_deref(), Some("http://proxy.local:8080"));
        assert_eq!(
            req.headers.get("authorization").and_then(|h| h.to_str().ok()),
            Some("Bearer test_token_123")
        );
        assert_eq!(
            req.headers.get("content-type").and_then(|h| h.to_str().ok()),
            Some("application/json")
        );
        assert_eq!(
            req.headers.get("x-domain").and_then(|h| h.to_str().ok()),
            Some("copilot.tencent.com")
        );
        assert_eq!(
            req.headers.get("x-product").and_then(|h| h.to_str().ok()),
            Some("codebuddy")
        );
        assert_eq!(
            req.headers.get("x-product-code").and_then(|h| h.to_str().ok()),
            Some("codebuddy")
        );
        assert_eq!(req.body.as_deref(), Some(&b"{}"[..]));
    }

    #[test]
    fn test_parse_codebuddy_claim_response_success() {
        let json = serde_json::json!({
            "code": 0,
            "msg": "success",
            "data": {
                "credits": 100
            }
        });
        let res = parse_codebuddy_claim_response(200, &json);
        assert!(res.success);
        assert!(res.newly_claimed);
        assert_eq!(res.credits_granted, Some(100));
        assert_eq!(res.raw_code, 0);
        assert_eq!(res.status, CodeBuddyCheckinStatus::CanCheckIn);
    }

    #[test]
    fn test_parse_codebuddy_claim_response_already_claimed_idempotent() {
        // HTTP 400 with code 10001
        let json = serde_json::json!({
            "code": 10001,
            "msg": "今天已签到"
        });
        let res = parse_codebuddy_claim_response(400, &json);
        assert!(res.success, "already checked in must be treated as idempotent success");
        assert!(!res.newly_claimed);
        assert_eq!(res.credits_granted, None);
        assert_eq!(res.raw_code, 10001);
        assert_eq!(res.status, CodeBuddyCheckinStatus::AlreadyCheckedIn);
    }

    #[test]
    fn test_parse_codebuddy_claim_response_activity_inactive() {
        let json = serde_json::json!({
            "code": 1003,
            "msg": "活动已结束"
        });
        let res = parse_codebuddy_claim_response(200, &json);
        assert!(!res.success);
        assert!(!res.newly_claimed);
        assert_eq!(res.raw_code, 1003);
        assert_eq!(res.status, CodeBuddyCheckinStatus::ActivityInactive);
    }

    #[test]
    fn test_parse_codebuddy_status_response_scenarios() {
        let pending_json = serde_json::json!({
            "code": 0,
            "data": {
                "has_checked_in": false
            }
        });
        let pending = parse_codebuddy_status_response(200, &pending_json);
        assert_eq!(pending.status, CodeBuddyCheckinStatus::CanCheckIn);
        assert!(!pending.has_checked_in);
        assert!(pending.activity_active);

        let checked_json = serde_json::json!({
            "code": 0,
            "data": {
                "has_checked_in": true
            }
        });
        let checked = parse_codebuddy_status_response(200, &checked_json);
        assert_eq!(checked.status, CodeBuddyCheckinStatus::AlreadyCheckedIn);
        assert!(checked.has_checked_in);

        let inactive_json = serde_json::json!({
            "code": 1003,
            "msg": "activity ended"
        });
        let inactive = parse_codebuddy_status_response(200, &inactive_json);
        assert_eq!(inactive.status, CodeBuddyCheckinStatus::ActivityInactive);
        assert!(!inactive.activity_active);
    }

    #[test]
    fn test_calculate_codebuddy_streak() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 10, 9).unwrap();

        // 1. New or empty streak: returns 1
        assert_eq!(calculate_codebuddy_streak(None, None, today), 1);
        assert_eq!(calculate_codebuddy_streak(Some(""), Some(0), today), 1);
        assert_eq!(calculate_codebuddy_streak(Some("invalid"), Some(5), today), 1);

        // 2. Last checkin yesterday: streak + 1
        assert_eq!(
            calculate_codebuddy_streak(Some("2026-10-08"), Some(3), today),
            4
        );
        assert_eq!(
            calculate_codebuddy_streak(Some("2026-10-08"), None, today),
            1
        );

        // 3. Last checkin today: keep existing streak
        assert_eq!(
            calculate_codebuddy_streak(Some("2026-10-09"), Some(4), today),
            4
        );
        assert_eq!(
            calculate_codebuddy_streak(Some("2026-10-09"), Some(0), today),
            1
        );

        // 4. Discontinuous: older than yesterday resets to 1
        assert_eq!(
            calculate_codebuddy_streak(Some("2026-10-07"), Some(10), today),
            1
        );

        // 5. Future date resets to 1
        assert_eq!(
            calculate_codebuddy_streak(Some("2026-10-10"), Some(5), today),
            1
        );
    }
}
