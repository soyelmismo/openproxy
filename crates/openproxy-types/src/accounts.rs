use crate::ids::{AccountId, ProviderId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthStatus {
    Healthy,
    Degraded,
    Unhealthy,
}

impl HealthStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::Unhealthy => "unhealthy",
        }
    }

    pub fn parse(s: &str) -> std::result::Result<Self, String> {
        match s {
            "healthy" => Ok(Self::Healthy),
            "degraded" => Ok(Self::Degraded),
            "unhealthy" => Ok(Self::Unhealthy),
            other => Err(format!("invalid health: {}", other)),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: AccountId,
    pub provider_id: ProviderId,
    pub label: Option<String>,
    pub priority: i32,
    pub extra_config_json: Option<String>,
    pub health_status: HealthStatus,
    pub rate_limited_until: Option<String>,
    pub quota_session_used: Option<i64>,
    pub quota_session_limit: Option<i64>,
    pub quota_session_reset_at: Option<String>,
    pub quota_weekly_used: Option<i64>,
    pub quota_weekly_limit: Option<i64>,
    pub quota_weekly_reset_at: Option<String>,
    pub quota_plan_name: Option<String>,
    pub quota_last_fetched_at: Option<String>,
    pub quota_fetch_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_model_details: Option<serde_json::Value>,
    pub auth_type: String,
    pub email: Option<String>,
    pub oauth_scope: Option<String>,
    #[serde(skip_serializing)]
    pub oauth_provider_specific: Option<String>,
    pub expires_at: Option<String>,
    pub created_at: String,
    pub current_proxy_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_health_status_parse() {
        assert_eq!(HealthStatus::parse("healthy"), Ok(HealthStatus::Healthy));
        assert_eq!(HealthStatus::parse("degraded"), Ok(HealthStatus::Degraded));
        assert_eq!(HealthStatus::parse("unhealthy"), Ok(HealthStatus::Unhealthy));
        assert_eq!(
            HealthStatus::parse("unknown"),
            Err("invalid health: unknown".to_string())
        );
    }

    #[test]
    fn test_health_status_as_str() {
        assert_eq!(HealthStatus::Healthy.as_str(), "healthy");
        assert_eq!(HealthStatus::Degraded.as_str(), "degraded");
        assert_eq!(HealthStatus::Unhealthy.as_str(), "unhealthy");
    }
}
