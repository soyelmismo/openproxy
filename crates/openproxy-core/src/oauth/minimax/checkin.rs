//! MiniMax daily check-in (signin) status checking and reward claiming.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use super::matrix::{MiniMaxRegion, build_matrix_get_request, build_matrix_post_request};
use crate::error::{CoreError, Result};
use openproxy_adapters::upstream::{CancellationToken, TimeoutProfile, UpstreamClient};
use serde::{Deserialize, Serialize};

const STATUS_PATH: &str = "/minimax-cloud/api/v1/signin/status";
const CLAIM_PATH: &str = "/minimax-cloud/api/v1/signin/claim";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SigninDayStatus {
    Upcoming = 1,
    Claimable = 2,
    Claimed = 3,
    Disabled = 4,
}

impl SigninDayStatus {
    pub fn from_u8(v: u8) -> Self {
        match v {
            2 => Self::Claimable,
            3 => Self::Claimed,
            4 => Self::Disabled,
            _ => Self::Upcoming,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SigninDayItem {
    pub day_no: u8,
    pub points: i64,
    #[serde(default)]
    pub bonus_points: Option<i64>,
    pub status: u8,
    #[serde(default)]
    pub is_today: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SigninPanel {
    #[serde(default)]
    pub scene: Option<u8>,
    pub days: Vec<SigninDayItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimSigninData {
    pub claim_id: String,
    pub claim_result: u8,
    pub day_no: u8,
    pub points: i64,
    #[serde(default)]
    pub expire_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyCheckinSummary {
    pub already_claimed: bool,
    pub points_claimed: i64,
    pub streak_days: u8,
    pub message: String,
}

fn current_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Fetches the 7-day sign-in panel and today's status.
pub async fn fetch_signin_panel(
    upstream: &Arc<UpstreamClient>,
    token: &str,
    user_id: &str,
    region: MiniMaxRegion,
) -> Result<SigninPanel> {
    let now_ms = current_now_ms();
    let req = build_matrix_get_request(region, STATUS_PATH, token, user_id, now_ms);
    let cancel = CancellationToken::new();

    let resp = upstream
        .call(req, TimeoutProfile::Quota, cancel)
        .await
        .map_err(|e| CoreError::UpstreamConnection(format!("minimax checkin status: {e}")))?;

    if !resp.status.is_success() {
        return Err(CoreError::UpstreamConnection(format!(
            "minimax checkin status HTTP {}",
            resp.status.as_u16()
        )));
    }

    let body_bytes = resp
        .collect()
        .await
        .map_err(|e| CoreError::UpstreamConnection(format!("minimax checkin read: {e}")))?;

    let json: serde_json::Value = serde_json::from_slice(&body_bytes)
        .map_err(|e| CoreError::Parse(format!("minimax checkin parse: {e}")))?;

    if let Some(base_resp) = json.get("base_resp") {
        let code = base_resp
            .get("status_code")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        if code != 0 {
            let msg = base_resp
                .get("status_msg")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("error");
            return Err(CoreError::UpstreamConnection(format!(
                "minimax checkin status failed: [{code}] {msg}"
            )));
        }
    }

    let data = json
        .get("data")
        .ok_or_else(|| CoreError::Parse("minimax checkin missing 'data'".into()))?;

    serde_json::from_value::<SigninPanel>(data.clone())
        .map_err(|e| CoreError::Parse(format!("minimax checkin panel parse: {e}")))
}

/// Claims the daily sign-in reward.
pub async fn claim_signin(
    upstream: &Arc<UpstreamClient>,
    token: &str,
    user_id: &str,
    region: MiniMaxRegion,
) -> Result<ClaimSigninData> {
    let now_ms = current_now_ms();
    let req = build_matrix_post_request(region, CLAIM_PATH, token, user_id, "{}", now_ms);
    let cancel = CancellationToken::new();

    let resp = upstream
        .call(req, TimeoutProfile::Quota, cancel)
        .await
        .map_err(|e| CoreError::UpstreamConnection(format!("minimax checkin claim: {e}")))?;

    if !resp.status.is_success() {
        return Err(CoreError::UpstreamConnection(format!(
            "minimax checkin claim HTTP {}",
            resp.status.as_u16()
        )));
    }

    let body_bytes = resp
        .collect()
        .await
        .map_err(|e| CoreError::UpstreamConnection(format!("minimax checkin claim read: {e}")))?;

    let json: serde_json::Value = serde_json::from_slice(&body_bytes)
        .map_err(|e| CoreError::Parse(format!("minimax checkin claim parse: {e}")))?;

    if let Some(base_resp) = json.get("base_resp") {
        let code = base_resp
            .get("status_code")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        if code != 0 {
            let msg = base_resp
                .get("status_msg")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("error");
            return Err(CoreError::UpstreamConnection(format!(
                "minimax checkin claim failed: [{code}] {msg}"
            )));
        }
    }

    let data = json
        .get("data")
        .ok_or_else(|| CoreError::Parse("minimax claim missing 'data'".into()))?;

    serde_json::from_value::<ClaimSigninData>(data.clone())
        .map_err(|e| CoreError::Parse(format!("minimax claim data parse: {e}")))
}

/// Computes the current consecutive signin streak in days.
pub fn calculate_streak(days: &[SigninDayItem]) -> u8 {
    let mut sorted = days.to_vec();
    sorted.sort_by_key(|d| d.day_no);

    let Some(today_idx) = sorted.iter().position(|d| d.is_today) else {
        return 0;
    };

    let today_item = &sorted[today_idx];
    let start_idx = if today_item.status == SigninDayStatus::Claimed as u8 {
        today_idx
    } else if today_item.status == SigninDayStatus::Claimable as u8 && today_idx > 0 {
        today_idx - 1
    } else {
        return 0;
    };

    let mut streak = 0;
    for i in (0..=start_idx).rev() {
        if sorted[i].status == SigninDayStatus::Claimed as u8 {
            streak += 1;
        } else {
            break;
        }
    }
    streak
}

/// Executes daily checkin for an account: inspects status and claims if claimable.
pub async fn execute_daily_checkin(
    upstream: &Arc<UpstreamClient>,
    token: &str,
    user_id: &str,
    region: MiniMaxRegion,
) -> Result<DailyCheckinSummary> {
    let panel = fetch_signin_panel(upstream, token, user_id, region).await?;
    let today = panel.days.iter().find(|d| d.is_today);

    let is_claimable = today.is_some_and(|d| d.status == SigninDayStatus::Claimable as u8);
    let is_already_claimed = today.is_some_and(|d| d.status == SigninDayStatus::Claimed as u8);

    if is_already_claimed {
        let streak = calculate_streak(&panel.days);
        return Ok(DailyCheckinSummary {
            already_claimed: true,
            points_claimed: 0,
            streak_days: streak,
            message: format!("Today's check-in already completed. Current streak: {streak} days"),
        });
    }

    if is_claimable {
        let claim = claim_signin(upstream, token, user_id, region).await?;
        let streak = calculate_streak(&panel.days) + 1;
        return Ok(DailyCheckinSummary {
            already_claimed: false,
            points_claimed: claim.points,
            streak_days: streak,
            message: format!(
                "Successfully claimed daily check-in (day {}): +{} points! Streak: {} days",
                claim.day_no, claim.points, streak
            ),
        });
    }

    let streak = calculate_streak(&panel.days);
    Ok(DailyCheckinSummary {
        already_claimed: true,
        points_claimed: 0,
        streak_days: streak,
        message: format!("No claimable check-in available. Current streak: {streak} days"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_streak_calculation() {
        let days = vec![
            SigninDayItem {
                day_no: 1,
                points: 10,
                bonus_points: None,
                status: SigninDayStatus::Claimed as u8,
                is_today: false,
            },
            SigninDayItem {
                day_no: 2,
                points: 20,
                bonus_points: None,
                status: SigninDayStatus::Claimed as u8,
                is_today: false,
            },
            SigninDayItem {
                day_no: 3,
                points: 30,
                bonus_points: None,
                status: SigninDayStatus::Claimed as u8,
                is_today: true,
            },
            SigninDayItem {
                day_no: 4,
                points: 40,
                bonus_points: None,
                status: SigninDayStatus::Upcoming as u8,
                is_today: false,
            },
        ];
        assert_eq!(calculate_streak(&days), 3);
    }
}
