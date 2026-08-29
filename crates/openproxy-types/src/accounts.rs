use crate::ids::{AccountId, ProviderId};
use serde::{Deserialize, Serialize};

impl_string_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "lowercase")]
    pub enum HealthStatus {
        Healthy => "healthy",
        Degraded => "degraded",
        Unhealthy => "unhealthy",
    }
    error: "health"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: AccountId,
    pub provider_id: ProviderId,
    pub label: Option<Box<str>>,
    pub priority: i32,
    pub health_status: HealthStatus,
    pub extra_config_json: Option<Box<str>>,
    pub rate_limited_until: Option<Box<str>>,
    pub quota_session_used: Option<i64>,
    pub quota_session_limit: Option<i64>,
    pub quota_session_reset_at: Option<Box<str>>,
    pub quota_weekly_used: Option<i64>,
    pub quota_weekly_limit: Option<i64>,
    pub quota_weekly_reset_at: Option<Box<str>>,
    pub quota_plan_name: Option<Box<str>>,
    pub quota_last_fetched_at: Option<Box<str>>,
    pub quota_fetch_error: Option<Box<str>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_model_details: Option<serde_json::Value>,
    pub auth_type: Box<str>,
    pub email: Option<Box<str>>,
    pub oauth_scope: Option<Box<str>>,
    #[serde(skip_serializing)]
    pub oauth_provider_specific: Option<Box<str>>,
    pub expires_at: Option<Box<str>>,
    pub created_at: Box<str>,
    pub current_proxy_id: Option<Box<str>>,
}

/// Parameters for storing or updating OAuth tokens on an account.
#[derive(Debug, Clone, Copy, Default)]
pub struct StoreOAuthTokensParams<'a> {
    pub access_token: &'a str,
    pub refresh_token: Option<&'a str>,
    pub token_type: &'a str,
    pub expires_at: Option<&'a str>,
    pub scope: Option<&'a str>,
    pub provider_specific: Option<&'a str>,
    pub email: Option<&'a str>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_health_status_parse() {
        assert_eq!(HealthStatus::parse("healthy"), Ok(HealthStatus::Healthy));
        assert_eq!(HealthStatus::parse("degraded"), Ok(HealthStatus::Degraded));
        assert_eq!(
            HealthStatus::parse("unhealthy"),
            Ok(HealthStatus::Unhealthy)
        );
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
