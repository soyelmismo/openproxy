use super::apply_codebuddy_spoofing_headers;
use crate::adapters::{
    Arc, CancellationToken, CoreError, Result, TimeoutProfile, UpstreamClient, UpstreamRequest,
};
use openproxy_types::quota::{AccountQuota, ModelQuotaDetail, now_unix_secs_str};

/// Base URL for CodeBuddy accounts endpoint.
pub const CODEBUDDY_ACCOUNTS_URL: &str = "https://www.codebuddy.ai/v2/accounts";

/// Default daily credits allocated to CodeBuddy free-tier accounts.
pub const CODEBUDDY_DEFAULT_DAILY_CREDITS: i64 = 30;

/// Model credit cost configuration from CodeBuddy's product.json.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CodeBuddyModelCreditCost {
    pub model_id: &'static str,
    pub display_name: &'static str,
    pub credit_cost: f64,
}

/// Known model credit multipliers extracted directly from product.json.
pub const CODEBUDDY_MODEL_CREDIT_COSTS: &[CodeBuddyModelCreditCost] = &[
    CodeBuddyModelCreditCost {
        model_id: "minimax-m3",
        display_name: "MiniMax-M3",
        credit_cost: 0.25,
    },
    CodeBuddyModelCreditCost {
        model_id: "gemini-3.1-flash-lite",
        display_name: "Gemini-3.1-flash-lite",
        credit_cost: 0.17,
    },
    CodeBuddyModelCreditCost {
        model_id: "gemini-3.0-flash",
        display_name: "Gemini-3.0-Flash",
        credit_cost: 0.33,
    },
    CodeBuddyModelCreditCost {
        model_id: "gemini-2.5-flash",
        display_name: "Gemini-2.5-Flash",
        credit_cost: 0.22,
    },
    CodeBuddyModelCreditCost {
        model_id: "fast-model",
        display_name: "Fast",
        credit_cost: 0.21,
    },
    CodeBuddyModelCreditCost {
        model_id: "balanced-model",
        display_name: "Balanced",
        credit_cost: 0.65,
    },
    CodeBuddyModelCreditCost {
        model_id: "deep-model",
        display_name: "Deep",
        credit_cost: 1.20,
    },
    CodeBuddyModelCreditCost {
        model_id: "gpt-5.1-codex-mini",
        display_name: "GPT-5.1-Codex-Mini",
        credit_cost: 0.18,
    },
    CodeBuddyModelCreditCost {
        model_id: "deepseek-v3-2-volc",
        display_name: "DeepSeek-V3.2",
        credit_cost: 0.29,
    },
    CodeBuddyModelCreditCost {
        model_id: "kimi-k2.5",
        display_name: "Kimi-K2.5",
        credit_cost: 0.45,
    },
    CodeBuddyModelCreditCost {
        model_id: "kimi-k2.6",
        display_name: "Kimi-K2.6",
        credit_cost: 0.52,
    },
    CodeBuddyModelCreditCost {
        model_id: "glm-5.0",
        display_name: "GLM-5.0",
        credit_cost: 0.80,
    },
    CodeBuddyModelCreditCost {
        model_id: "glm-5.2",
        display_name: "GLM-5.2",
        credit_cost: 0.79,
    },
    CodeBuddyModelCreditCost {
        model_id: "glm-5.3",
        display_name: "GLM-5.3",
        credit_cost: 0.79,
    },
    CodeBuddyModelCreditCost {
        model_id: "gpt-5.1-codex",
        display_name: "GPT-5.1-Codex",
        credit_cost: 0.90,
    },
    CodeBuddyModelCreditCost {
        model_id: "gpt-5.3-codex",
        display_name: "GPT-5.3-Codex",
        credit_cost: 1.25,
    },
    CodeBuddyModelCreditCost {
        model_id: "gemini-2.5-pro",
        display_name: "Gemini-2.5-Pro",
        credit_cost: 0.90,
    },
    CodeBuddyModelCreditCost {
        model_id: "gemini-3.5-flash",
        display_name: "Gemini-3.5-Flash",
        credit_cost: 0.99,
    },
    CodeBuddyModelCreditCost {
        model_id: "gemini-3.1-pro",
        display_name: "Gemini-3.1-Pro",
        credit_cost: 1.32,
    },
    CodeBuddyModelCreditCost {
        model_id: "gpt-5.4",
        display_name: "GPT-5.4",
        credit_cost: 1.65,
    },
    CodeBuddyModelCreditCost {
        model_id: "kimi-k3",
        display_name: "Kimi-K3",
        credit_cost: 1.62,
    },
    CodeBuddyModelCreditCost {
        model_id: "gpt-5.5",
        display_name: "GPT-5.5",
        credit_cost: 3.31,
    },
    CodeBuddyModelCreditCost {
        model_id: "gpt-5.6-sol",
        display_name: "GPT-5.6-Sol",
        credit_cost: 3.47,
    },
    CodeBuddyModelCreditCost {
        model_id: "hy3",
        display_name: "Hy3",
        credit_cost: 0.00,
    },
];

/// Calculates the exact unix timestamp in seconds for the next 12:00 AM CST (China Standard Time, UTC+8),
/// matching Tencent CodeBuddy's daily quota reset policy.
#[must_use]
pub fn calculate_next_midnight_cst_unix_secs() -> u64 {
    let now_utc = chrono::Utc::now();
    let Some(cst) = chrono::FixedOffset::east_opt(8 * 3600) else {
        return (now_utc.timestamp() + 86_400).max(0) as u64;
    };
    let now_cst = now_utc.with_timezone(&cst);
    let tomorrow_cst = now_cst.date_naive().succ_opt().unwrap_or(now_cst.date_naive());
    let next_midnight_naive = tomorrow_cst.and_hms_opt(0, 0, 0).unwrap_or_default();
    if let Some(cst_dt) = next_midnight_naive.and_local_timezone(cst).single() {
        cst_dt.to_utc().timestamp().max(0) as u64
    } else {
        (now_utc.timestamp() + 86_400).max(0) as u64
    }
}

/// Builds breakdown of model capacities and remaining fractions based on CodeBuddy 30 daily credits.
#[must_use]
pub fn build_codebuddy_quota_model_details(
    session_limit: i64,
    session_used: i64,
    reset_at: Option<&str>,
) -> Vec<ModelQuotaDetail> {
    let mut details = Vec::with_capacity(CODEBUDDY_MODEL_CREDIT_COSTS.len());

    for def in CODEBUDDY_MODEL_CREDIT_COSTS {
        let (model_limit, model_used, rem_frac) = if def.credit_cost > 0.0 {
            let m_limit = ((session_limit as f64) / def.credit_cost).floor() as i64;
            let m_used = ((session_used as f64) / def.credit_cost).floor() as i64;
            let frac = if m_limit > 0 {
                ((m_limit - m_used) as f64 / m_limit as f64).clamp(0.0, 1.0)
            } else {
                0.0
            };
            (m_limit, m_used, frac)
        } else {
            // Free / 0.00 credit cost (e.g. hy3)
            (9_999, 0, 1.0)
        };

        details.push(ModelQuotaDetail {
            model_id: def.model_id.to_string(),
            session_used: model_used,
            session_limit: model_limit,
            session_reset_at: reset_at.map(ToString::to_string),
            remaining_fraction: rem_frac,
        });
    }

    details
}

fn extract_numeric_field(val: &serde_json::Value, keys: &[&str]) -> Option<f64> {
    for key in keys {
        if let Some(v) = val.get(*key) {
            if let Some(n) = v.as_f64() {
                return Some(n);
            }
            if let Some(s) = v.as_str()
                && let Ok(n) = s.trim().replace(',', "").parse::<f64>()
            {
                return Some(n);
            }
        }
    }
    None
}

/// Parses the `/v2/accounts` response payload into an [`AccountQuota`] snapshot.
#[must_use]
pub fn parse_codebuddy_accounts_quota(val: &serde_json::Value) -> AccountQuota {
    let accounts_arr = val
        .get("data")
        .and_then(|d| d.get("accounts"))
        .and_then(serde_json::Value::as_array)
        .or_else(|| val.get("accounts").and_then(serde_json::Value::as_array));

    let first_account = accounts_arr
        .and_then(|arr| arr.iter().find(|acc| acc.get("pluginEnabled").and_then(serde_json::Value::as_bool).unwrap_or(true)))
        .or_else(|| accounts_arr.and_then(|arr| arr.first()))
        .or_else(|| val.get("data"));

    let mut plan_name = "CodeBuddy Free (30 daily credits)".to_string();
    let mut session_limit = CODEBUDDY_DEFAULT_DAILY_CREDITS;
    let mut session_used = 0i64;

    if let Some(acc) = first_account {
        if let Some(account_type) = acc.get("type").and_then(serde_json::Value::as_str) {
            let t = account_type.trim().to_lowercase();
            if t == "enterprise" {
                plan_name = "CodeBuddy Enterprise".to_string();
            } else if t == "team" {
                plan_name = "CodeBuddy Team".to_string();
            } else if let Some(plan_str) = acc.get("plan").or_else(|| acc.get("planName")).and_then(serde_json::Value::as_str) {
                let p = plan_str.trim();
                if !p.is_empty() {
                    plan_name = format!("CodeBuddy {p} (30 daily credits)");
                }
            }
        }

        // Check for explicit credit balances
        let parsed_limit = extract_numeric_field(
            acc,
            &["total_credits", "credit_limit", "initial_credits", "total", "daily_credits"],
        );
        if let Some(l) = parsed_limit
            && l > 0.0
        {
            session_limit = l.round() as i64;
        }

        let parsed_rem = extract_numeric_field(
            acc,
            &["credits", "credit_balance", "remaining_credits", "remains", "balance", "remaining"],
        );
        if let Some(rem) = parsed_rem {
            let rem_clamped = rem.max(0.0);
            session_used = (session_limit - rem_clamped.round() as i64).max(0);
        }
    }

    let reset_at_secs = calculate_next_midnight_cst_unix_secs();
    let reset_at_str = reset_at_secs.to_string();
    let model_details =
        build_codebuddy_quota_model_details(session_limit, session_used, Some(&reset_at_str));

    AccountQuota {
        session_used: Some(session_used),
        session_limit: Some(session_limit),
        session_reset_at: Some(reset_at_str),
        weekly_used: None,
        weekly_limit: None,
        weekly_reset_at: None,
        plan_name: Some(plan_name),
        last_fetched_at: now_unix_secs_str(),
        fetch_error: None,
        model_details: Some(model_details.into_boxed_slice()),
    }
}

/// Builds an authenticated UpstreamRequest for querying CodeBuddy accounts and credit information.
#[must_use]
pub fn build_codebuddy_accounts_request(token: &str, proxy_url: Option<&str>) -> UpstreamRequest {
    let mut req = UpstreamRequest::get(CODEBUDDY_ACCOUNTS_URL);
    req.proxy = proxy_url.map(ToString::to_string);

    let trimmed = token.trim();
    let auth_val = if trimmed.starts_with("Bearer ") {
        trimmed.to_string()
    } else {
        format!("Bearer {trimmed}")
    };
    if let Ok(hv) = http::HeaderValue::from_str(&auth_val) {
        req.headers.insert(http::header::AUTHORIZATION, hv);
    }
    req.headers.insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("application/json"),
    );
    req.headers.insert(
        http::HeaderName::from_static("x-no-enterprise-id"),
        http::HeaderValue::from_static("true"),
    );
    req.headers.insert(
        http::HeaderName::from_static("x-no-user-id"),
        http::HeaderValue::from_static("true"),
    );
    req.headers.insert(
        http::HeaderName::from_static("x-no-department-info"),
        http::HeaderValue::from_static("true"),
    );

    apply_codebuddy_spoofing_headers(&mut req);
    req
}

/// Fetches CodeBuddy quota from `https://www.codebuddy.ai/v2/accounts` using the provided token.
pub async fn fetch_codebuddy_quota_unified(
    upstream: &Arc<UpstreamClient>,
    token: &str,
    _provider_specific: Option<&str>,
    proxy_url: Option<&str>,
) -> Result<AccountQuota> {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return Err(CoreError::Validation(
            "missing OAuth access token for codebuddy quota".into(),
        ));
    }

    let req = build_codebuddy_accounts_request(trimmed, proxy_url);
    let cancel = CancellationToken::new();
    let response = upstream
        .call(req, TimeoutProfile::Quota, cancel)
        .await
        .map_err(|e| e.to_core_error(CODEBUDDY_ACCOUNTS_URL))?;

    let status = response.status;
    if status == http::StatusCode::UNAUTHORIZED {
        return Err(CoreError::UpstreamConnection(format!(
            "{CODEBUDDY_ACCOUNTS_URL}: HTTP status 401 (token expired)"
        )));
    }

    if response.status.is_success() {
        let body = response
            .collect()
            .await
            .map_err(|e| e.to_core_error(CODEBUDDY_ACCOUNTS_URL))?;
        if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&body) {
            return Ok(parse_codebuddy_accounts_quota(&json));
        }
    }

    // Default fallback quota if upstream accounts endpoint is temporarily unreachable
    let reset_at_secs = calculate_next_midnight_cst_unix_secs();
    let reset_at_str = reset_at_secs.to_string();
    let model_details = build_codebuddy_quota_model_details(
        CODEBUDDY_DEFAULT_DAILY_CREDITS,
        0,
        Some(&reset_at_str),
    );

    Ok(AccountQuota {
        session_used: Some(0),
        session_limit: Some(CODEBUDDY_DEFAULT_DAILY_CREDITS),
        session_reset_at: Some(reset_at_str),
        weekly_used: None,
        weekly_limit: None,
        weekly_reset_at: None,
        plan_name: Some("CodeBuddy Free (30 daily credits)".into()),
        last_fetched_at: now_unix_secs_str(),
        fetch_error: None,
        model_details: Some(model_details.into_boxed_slice()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_next_midnight_cst() {
        let ts = calculate_next_midnight_cst_unix_secs();
        let now = chrono::Utc::now().timestamp() as u64;
        assert!(ts > now, "next midnight CST must be in the future");
        assert!(
            ts <= now + 86_400 + 3_600,
            "next midnight CST must be within 25 hours"
        );
    }

    #[test]
    fn test_parse_codebuddy_accounts_quota_default() {
        let val = serde_json::json!({
            "code": 0,
            "msg": "ok",
            "data": {
                "accounts": [
                    {
                        "uid": "user_12345",
                        "type": "personal",
                        "pluginEnabled": true
                    }
                ]
            }
        });

        let quota = parse_codebuddy_accounts_quota(&val);
        assert_eq!(quota.session_limit, Some(30));
        assert_eq!(quota.session_used, Some(0));
        assert_eq!(
            quota.plan_name.as_deref(),
            Some("CodeBuddy Free (30 daily credits)")
        );
        assert!(quota.session_reset_at.is_some());
        assert!(quota.model_details.is_some());

        let details = quota.model_details.unwrap();
        // Check minimax-m3 capacity: 30 / 0.25 = 120 calls
        let m3 = details.iter().find(|d| d.model_id == "minimax-m3").unwrap();
        assert_eq!(m3.session_limit, 120);
        assert_eq!(m3.session_used, 0);
        assert_eq!(m3.remaining_fraction, 1.0);

        // Check gemini-3.1-flash-lite: 30 / 0.17 = 176 calls
        let lite = details
            .iter()
            .find(|d| d.model_id == "gemini-3.1-flash-lite")
            .unwrap();
        assert_eq!(lite.session_limit, 176);

        // Check hy3: free 0.00 credits
        let hy3 = details.iter().find(|d| d.model_id == "hy3").unwrap();
        assert_eq!(hy3.session_limit, 9_999);
        assert_eq!(hy3.remaining_fraction, 1.0);
    }

    #[test]
    fn test_parse_codebuddy_accounts_quota_with_explicit_credits() {
        let val = serde_json::json!({
            "code": 0,
            "data": {
                "accounts": [
                    {
                        "uid": "user_999",
                        "type": "personal",
                        "pluginEnabled": true,
                        "total_credits": 30,
                        "remaining_credits": 18.5
                    }
                ]
            }
        });

        let quota = parse_codebuddy_accounts_quota(&val);
        assert_eq!(quota.session_limit, Some(30));
        assert_eq!(quota.session_used, Some(11)); // 30 - 19 = 11 used

        let details = quota.model_details.unwrap();
        let m3 = details.iter().find(|d| d.model_id == "minimax-m3").unwrap();
        assert_eq!(m3.session_limit, 120);
        assert_eq!(m3.session_used, 44); // 11 / 0.25 = 44 used
    }

    #[test]
    fn test_parse_codebuddy_accounts_enterprise() {
        let val = serde_json::json!({
            "data": {
                "accounts": [
                    {
                        "uid": "ent_1",
                        "type": "enterprise",
                        "total_credits": 500,
                        "credits": 250
                    }
                ]
            }
        });

        let quota = parse_codebuddy_accounts_quota(&val);
        assert_eq!(quota.session_limit, Some(500));
        assert_eq!(quota.session_used, Some(250));
        assert_eq!(quota.plan_name.as_deref(), Some("CodeBuddy Enterprise"));
    }

    #[test]
    fn test_build_codebuddy_accounts_request_headers() {
        let req = build_codebuddy_accounts_request("test-token-123", Some("http://proxy.local:8080"));
        assert_eq!(req.proxy.as_deref(), Some("http://proxy.local:8080"));
        assert_eq!(
            req.headers.get(http::header::AUTHORIZATION).unwrap().to_str().unwrap(),
            "Bearer test-token-123"
        );
        assert_eq!(
            req.headers.get("x-ide-type").unwrap().to_str().unwrap(),
            "CLI"
        );
        assert_eq!(
            req.headers.get("x-no-enterprise-id").unwrap().to_str().unwrap(),
            "true"
        );
    }
}
