//! MiniMax user identity, email, workspace, and credit balance resolution.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use super::matrix::{self, MiniMaxRegion};
use openproxy_adapters::upstream::{CancellationToken, TimeoutProfile, UpstreamClient};

/// Resolves user ID and user email from `/v1/api/user/info`.
pub async fn resolve_user_identity(
    upstream: &Arc<UpstreamClient>,
    token: &str,
    region: MiniMaxRegion,
) -> (Option<String>, Option<String>) {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);

    let req = matrix::build_matrix_get_request(region, "/v1/api/user/info", token, "0", now_ms);
    let resp = upstream
        .call(req, TimeoutProfile::OAuth, CancellationToken::new())
        .await
        .ok();

    let Some(resp) = resp else {
        return (None, None);
    };

    if !resp.status.is_success() {
        return (None, None);
    }

    let bytes = resp.collect().await.ok();
    let Some(bytes) = bytes else {
        return (None, None);
    };
    let json: serde_json::Value = serde_json::from_slice(&bytes).ok().unwrap_or_default();

    let data = json.get("data").unwrap_or(&json);
    let user_info = data.get("userInfo").or_else(|| data.get("user_info")).unwrap_or(data);

    let real_user_id = user_info
        .get("realUserID")
        .or_else(|| user_info.get("real_user_id"))
        .or_else(|| user_info.get("userId"))
        .or_else(|| user_info.get("user_id"))
        .and_then(serde_json::Value::as_str)
        .map(std::string::ToString::to_string);

    let email = user_info
        .get("userEmail")
        .or_else(|| user_info.get("email"))
        .or_else(|| user_info.get("user_email"))
        .or_else(|| user_info.get("mail"))
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(std::string::ToString::to_string)
        .or_else(|| {
            // If no email, fallback to userName or nickname as identity label
            user_info
                .get("userName")
                .or_else(|| user_info.get("user_name"))
                .or_else(|| user_info.get("nickname"))
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(std::string::ToString::to_string)
        });

    (real_user_id, email)
}

/// Resolves personal workspace metadata: (op_group_id, token_plan_tier, credit_balance).
pub async fn resolve_membership_info(
    upstream: &Arc<UpstreamClient>,
    token: &str,
    real_user_id: &str,
    region: MiniMaxRegion,
) -> Option<(String, Option<String>, Option<String>)> {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);

    let req = matrix::build_matrix_post_request(
        region,
        "/matrix/api/v1/user/get_user_extra_info",
        token,
        real_user_id,
        "{}",
        now_ms,
    );

    let resp = upstream
        .call(req, TimeoutProfile::OAuth, CancellationToken::new())
        .await
        .ok()?;

    if !resp.status.is_success() {
        return None;
    }

    let bytes = resp.collect().await.ok()?;
    let json: serde_json::Value = serde_json::from_slice(&bytes).ok()?;

    let data = json.get("data").unwrap_or(&json);
    let workspaces = data
        .get("workspaces")
        .or_else(|| json.get("workspaces"))
        .and_then(serde_json::Value::as_array)?;

    for ws in workspaces {
        let ws_type = ws.get("workspace_type").and_then(serde_json::Value::as_i64);
        if ws_type == Some(0) {
            // Personal workspace
            let op_group = ws
                .get("op_group_id")
                .and_then(serde_json::Value::as_str)
                .map(std::string::ToString::to_string);
            let tier = ws
                .get("token_plan_tier")
                .and_then(serde_json::Value::as_str)
                .map(std::string::ToString::to_string);
            let credit_balance = extract_credit_balance(ws, data);

            if let Some(gid) = op_group {
                return Some((gid, tier, credit_balance));
            }
        }
    }

    None
}

fn extract_credit_amount(val: &serde_json::Value) -> Option<String> {
    if let Some(s) = val.as_str().filter(|s| !s.trim().is_empty()) {
        return Some(s.trim().to_string());
    }
    if let Some(n) = val.as_i64() {
        return Some(n.to_string());
    }
    if let Some(f) = val.as_f64() {
        return Some(format!("{f:.2}"));
    }
    None
}

fn extract_credit_balance(ws: &serde_json::Value, data: &serde_json::Value) -> Option<String> {
    let summaries = [
        ws.get("op_credit_summary"),
        ws.get("opcredit_summary"),
        ws.get("credit_summary"),
        data.get("op_credit_summary"),
        data.get("opcredit_summary"),
        data.get("credit_summary"),
    ];
    for summary in summaries.into_iter().flatten() {
        for key in [
            "total_remaining_amount",
            "remaining_amount",
            "balance",
            "total_amount",
            "available_amount",
        ] {
            if let Some(val) = summary.get(key)
                && let Some(amt) = extract_credit_amount(val)
            {
                return Some(amt);
            }
        }
    }

    for obj in [ws, data] {
        for key in [
            "credit_balance",
            "op_credit_balance",
            "credit",
            "credits",
            "balance",
            "points",
        ] {
            if let Some(val) = obj.get(key)
                && let Some(amt) = extract_credit_amount(val)
            {
                return Some(amt);
            }
        }
    }

    None
}
