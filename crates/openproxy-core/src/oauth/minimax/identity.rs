//! MiniMax user identity, email, workspace, and credit balance resolution.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use super::matrix::{self, MiniMaxRegion};
use openproxy_adapters::upstream::{CancellationToken, TimeoutProfile, UpstreamClient};

/// User identity resolved from `/v1/api/user/info`.
#[derive(Debug, Clone, Default)]
pub struct MiniMaxIdentity {
    pub real_user_id: Option<String>,
    pub email: Option<String>,
    pub display_name: Option<String>,
}

impl MiniMaxIdentity {
    /// Returns the best non-empty label for account display: email, then display name, then fallback.
    pub fn display_label(&self) -> String {
        if let Some(ref em) = self.email
            && !em.trim().is_empty()
        {
            return em.clone();
        }
        if let Some(ref name) = self.display_name
            && !name.trim().is_empty()
        {
            return name.clone();
        }
        if let Some(ref uid) = self.real_user_id
            && !uid.trim().is_empty()
        {
            return format!("MiniMax User {uid}");
        }
        "MiniMax User".to_string()
    }
}

/// Resolves user ID, email, and display name from `/v1/api/user/info`.
pub async fn resolve_user_identity(
    upstream: &Arc<UpstreamClient>,
    token: &str,
    region: MiniMaxRegion,
) -> MiniMaxIdentity {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);

    let req = matrix::build_matrix_get_request(region, "/v1/api/user/info", token, "0", now_ms);
    let resp = upstream
        .call(req, TimeoutProfile::OAuth, CancellationToken::new())
        .await
        .ok();

    let Some(resp) = resp else {
        return MiniMaxIdentity::default();
    };

    if !resp.status.is_success() {
        return MiniMaxIdentity::default();
    }

    let bytes = resp.collect().await.ok();
    let Some(bytes) = bytes else {
        return MiniMaxIdentity::default();
    };
    let json: serde_json::Value = serde_json::from_slice(&bytes).ok().unwrap_or_default();

    let data = json.get("data").unwrap_or(&json);
    let user_info = data.get("userInfo").or_else(|| data.get("user_info")).unwrap_or(data);

    let real_user_id = user_info
        .get("realUserID")
        .or_else(|| user_info.get("real_user_id"))
        .or_else(|| user_info.get("userID"))
        .or_else(|| user_info.get("userId"))
        .or_else(|| user_info.get("user_id"))
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(std::string::ToString::to_string);

    let email = user_info
        .get("userEmail")
        .or_else(|| user_info.get("email"))
        .or_else(|| user_info.get("user_email"))
        .or_else(|| user_info.get("mail"))
        .or_else(|| user_info.get("userMail"))
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(std::string::ToString::to_string);

    let display_name = user_info
        .get("name")
        .or_else(|| user_info.get("userName"))
        .or_else(|| user_info.get("user_name"))
        .or_else(|| user_info.get("subUserName"))
        .or_else(|| user_info.get("sub_user_name"))
        .or_else(|| user_info.get("nickname"))
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(std::string::ToString::to_string);

    MiniMaxIdentity {
        real_user_id,
        email,
        display_name,
    }
}

/// Resolves personal workspace metadata: (op_group_id, token_plan_tier, credit_balance).
///
/// Matches upstream `matrix-account-client.ts` 1:1 by inspecting `/matrix/api/v1/user/get_user_extra_info`
/// for personal workspaces and querying `/matrix/api/v1/commerce/get_membership_info` to extract
/// `op_credit_summary.total_remaining_amount`.
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

    let (mut op_group, mut tier, mut credit_balance, ws_id) = if resp.status.is_success() {
        let bytes = resp.collect().await.ok()?;
        let json: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        let data = json.get("data").unwrap_or(&json);
        let workspaces = data
            .get("workspaces")
            .or_else(|| json.get("workspaces"))
            .and_then(serde_json::Value::as_array);

        let mut found_gid = None;
        let mut found_tier = None;
        let mut found_credits = None;
        let mut found_ws_id = serde_json::json!(0);

        if let Some(workspaces) = workspaces {
            for ws in workspaces {
                let ws_type = ws.get("workspace_type").and_then(serde_json::Value::as_i64);
                if ws_type == Some(0) {
                    if let Some(id_val) = ws.get("workspace_id") {
                        found_ws_id = id_val.clone();
                    }
                    found_gid = ws
                        .get("op_group_id")
                        .and_then(serde_json::Value::as_str)
                        .map(std::string::ToString::to_string);
                    found_tier = ws
                        .get("token_plan_tier")
                        .and_then(serde_json::Value::as_str)
                        .map(std::string::ToString::to_string);
                    found_credits = extract_credit_balance(ws, data);
                    break;
                }
            }
        }
        (found_gid, found_tier, found_credits, found_ws_id)
    } else {
        (None, None, None, serde_json::json!(0))
    };

    // Upstream fallback / enrichment: query /matrix/api/v1/commerce/get_membership_info
    let commerce_body = serde_json::json!({ "workspace_id": ws_id }).to_string();
    let commerce_req = matrix::build_matrix_post_request(
        region,
        "/matrix/api/v1/commerce/get_membership_info",
        token,
        real_user_id,
        &commerce_body,
        now_ms,
    );

    if let Ok(c_resp) = upstream
        .call(commerce_req, TimeoutProfile::OAuth, CancellationToken::new())
        .await
        && c_resp.status.is_success()
        && let Ok(c_bytes) = c_resp.collect().await
        && let Ok(c_json) = serde_json::from_slice::<serde_json::Value>(&c_bytes)
    {
        if op_group.is_none() {
            op_group = c_json
                .get("op_group_id")
                .and_then(serde_json::Value::as_str)
                .map(std::string::ToString::to_string);
        }
        if tier.is_none() {
            tier = c_json
                .get("token_plan_tier")
                .and_then(serde_json::Value::as_str)
                .map(std::string::ToString::to_string);
        }
        let c_credits = extract_credit_balance(&c_json, &c_json);
        if c_credits.is_some() {
            credit_balance = c_credits;
        }
    }

    op_group.map(|gid| (gid, tier, credit_balance))
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
