use crate::upstream::{CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest};
use openproxy_types::{AccountQuota, Result, ResultExt};
use serde_json::Value;
use std::sync::Arc;

use super::apply_commandcode_cli_headers;

fn extract_commandcode_reset(val: Option<&Value>) -> Option<String> {
    val.and_then(|v| {
        v.as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != "0")
            .map(ToString::to_string)
            .or_else(|| v.as_i64().filter(|&n| n > 0).map(|n| n.to_string()))
            .or_else(|| {
                v.as_f64()
                    .filter(|&n| n > 0.0)
                    .map(|n| (n as i64).to_string())
            })
    })
}

pub(crate) fn parse_commandcode_window(
    win: Option<&Value>,
) -> (Option<i64>, Option<i64>, Option<String>) {
    let Some(win) = win else {
        return (None, None, None);
    };
    let reset = extract_commandcode_reset(win.get("resetAt"));
    let Some(used) = win.get("used").and_then(Value::as_f64) else {
        return (None, None, reset);
    };
    let Some(cap) = win.get("cap").and_then(Value::as_f64) else {
        return (None, None, reset);
    };

    if cap <= 0.0 {
        return (None, None, reset);
    }

    let is_exceeded = win
        .get("exceeded")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut pct = ((used / cap) * 100.0).round().clamp(0.0, 100.0) as i64;
    if is_exceeded {
        pct = 100;
    }

    (Some(pct), Some(100), reset)
}

pub(crate) fn humanize_commandcode_plan(
    plan_id: Option<&str>,
    monthly_info: Option<&str>,
) -> String {
    let base = match plan_id {
        Some("individual-go") => "Command Code · Go",
        Some("individual-pro") => "Command Code · Pro",
        Some("individual-goat") => "Command Code · GOAT",
        Some("individual-max-10x") => "Command Code · Max 10×",
        Some("individual-max-20x") => "Command Code · Max 20×",
        Some("team-pro") => "Command Code · Team Pro",
        Some(other) if !other.is_empty() => other,
        _ => "Command Code",
    };

    match monthly_info {
        Some(info) if !info.is_empty() => format!("{base} · {info}"),
        _ => base.to_string(),
    }
}

pub(crate) async fn fetch_commandcode_quota(
    upstream_client: &Arc<UpstreamClient>,
    token: &str,
) -> Result<AccountQuota> {
    // 1. Query /alpha/billing/credits
    let credits_url = "https://api.commandcode.ai/alpha/billing/credits";
    let mut credits_req = UpstreamRequest::get(credits_url);
    apply_commandcode_cli_headers(&mut credits_req, token);
    let cancel = CancellationToken::new();
    let credits_resp = upstream_client
        .call(credits_req, TimeoutProfile::Quota, cancel)
        .await
        .ctx_upstream("credits request failed")?;

    let mut session_used = None;
    let mut session_limit = None;
    let mut session_reset_at = None;
    let mut weekly_used = None;
    let mut weekly_limit = None;
    let mut weekly_reset_at = None;
    let mut remaining_monthly = None;

    if credits_resp.status.is_success()
        && let Ok(body) = credits_resp.collect().await
        && let Ok(v) = serde_json::from_slice::<Value>(&body)
    {
        let limits = v.get("windowLimits").unwrap_or(&v);
        let (s_used, s_limit, s_reset) = parse_commandcode_window(limits.get("fiveHour"));
        session_used = s_used;
        session_limit = s_limit;
        session_reset_at = s_reset;

        let (w_used, w_limit, w_reset) = parse_commandcode_window(limits.get("weekly"));
        weekly_used = w_used;
        weekly_limit = w_limit;
        weekly_reset_at = w_reset;

        remaining_monthly = v
            .get("credits")
            .and_then(|c| c.get("monthlyCredits"))
            .and_then(Value::as_f64);
    }

    // 2. Query /alpha/billing/subscriptions
    let sub_url = "https://api.commandcode.ai/alpha/billing/subscriptions";
    let mut sub_req = UpstreamRequest::get(sub_url);
    apply_commandcode_cli_headers(&mut sub_req, token);
    let cancel = CancellationToken::new();
    let (plan_id, period_end) = if let Ok(sub_resp) = upstream_client
        .call(sub_req, TimeoutProfile::Quota, cancel)
        .await
        && sub_resp.status.is_success()
        && let Ok(body) = sub_resp.collect().await
        && let Ok(v) = serde_json::from_slice::<Value>(&body)
    {
        let data = v.get("data").unwrap_or(&v);
        let plan = data
            .get("planId")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        let end = data
            .get("currentPeriodEnd")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        (plan, end)
    } else {
        (None, None)
    };

    // 3. Query /alpha/usage/summary for monthly spent credits
    let summary_url = "https://api.commandcode.ai/alpha/usage/summary";
    let mut summary_req = UpstreamRequest::get(summary_url);
    apply_commandcode_cli_headers(&mut summary_req, token);
    let cancel = CancellationToken::new();
    let used_monthly = if let Ok(summary_resp) = upstream_client
        .call(summary_req, TimeoutProfile::Quota, cancel)
        .await
        && summary_resp.status.is_success()
        && let Ok(body) = summary_resp.collect().await
        && let Ok(v) = serde_json::from_slice::<Value>(&body)
    {
        v.get("totalMonthlyCredits")
            .and_then(Value::as_f64)
            .or_else(|| v.get("totalCost").and_then(Value::as_f64))
    } else {
        None
    };

    let mut model_details_vec = Vec::new();
    if let (Some(rem), Some(used)) = (remaining_monthly, used_monthly)
        && (rem + used) > 0.0
    {
        let total = rem + used;
        let pct = ((used / total) * 100.0).round().clamp(0.0, 100.0) as i64;
        let rem_fraction = ((total - used).max(0.0)) / total;
        model_details_vec.push(openproxy_types::ModelQuotaDetail {
            model_id: "Monthly Limit".to_string(),
            session_used: pct,
            session_limit: 100,
            session_reset_at: period_end,
            remaining_fraction: rem_fraction,
        });
    }

    let model_details = if model_details_vec.is_empty() {
        None
    } else {
        Some(model_details_vec.into_boxed_slice())
    };

    let plan_name = Some(humanize_commandcode_plan(plan_id.as_deref(), None));

    Ok(AccountQuota {
        session_used,
        session_limit,
        session_reset_at,
        weekly_used,
        weekly_limit,
        weekly_reset_at,
        plan_name,
        last_fetched_at: openproxy_types::now_unix_secs_str(),
        fetch_error: None,
        model_details,
    })
}
