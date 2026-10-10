use crate::ids::{AccountId, ProviderId};
use crate::quota::{ModelQuotaDetail, QuotaPool};
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
    pub quota_model_details: Option<Box<[ModelQuotaDetail]>>,
    /// Independently accounted Z.ai / ZCode entitlements. Additive: absent on
    /// every pre-existing row and in every pre-existing serialized `Account`,
    /// so old payloads still deserialize (`serde(default)`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_pools: Option<Box<[QuotaPool]>>,
    pub auth_type: Box<str>,
    pub email: Option<Box<str>>,
    pub oauth_scope: Option<Box<str>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
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

    #[test]
    fn test_account_oauth_provider_specific_serialization() {
        let acc = Account {
            id: AccountId::new(1),
            provider_id: ProviderId::new("minimax"),
            label: Some("test-label".into()),
            priority: 10,
            health_status: HealthStatus::Healthy,
            extra_config_json: None,
            rate_limited_until: None,
            quota_session_used: None,
            quota_session_limit: None,
            quota_session_reset_at: None,
            quota_weekly_used: None,
            quota_weekly_limit: None,
            quota_weekly_reset_at: None,
            quota_plan_name: Some("Free".into()),
            quota_last_fetched_at: None,
            quota_fetch_error: None,
            quota_model_details: None,
            quota_pools: None,
            auth_type: "oauth".into(),
            email: Some("test@example.com".into()),
            oauth_scope: None,
            oauth_provider_specific: Some(
                r#"{"credit_balance":"604.884","streak_days":1,"last_checkin_date":"2026-09-18"}"#
                    .into(),
            ),
            expires_at: None,
            created_at: "2026-09-18 12:00:00".into(),
            current_proxy_id: None,
        };

        let json_str = serde_json::to_string(&acc).expect("serialize account");
        let v: serde_json::Value = serde_json::from_str(&json_str).expect("parse json");

        let specific_str = v
            .get("oauth_provider_specific")
            .and_then(|v| v.as_str())
            .expect("must serialize oauth_provider_specific");
        let specific_obj: serde_json::Value =
            serde_json::from_str(specific_str).expect("must parse internal json");

        assert_eq!(
            specific_obj.get("credit_balance").and_then(|v| v.as_str()),
            Some("604.884")
        );
        assert_eq!(
            specific_obj.get("streak_days").and_then(|v| v.as_i64()),
            Some(1)
        );
        assert_eq!(
            specific_obj
                .get("last_checkin_date")
                .and_then(|v| v.as_str()),
            Some("2026-09-18")
        );
    }

    #[test]
    fn test_store_oauth_tokens_params_default_and_fields() {
        let params = StoreOAuthTokensParams::default();
        assert_eq!(params.access_token, "");
        assert!(params.refresh_token.is_none());
        assert_eq!(params.token_type, "");
        assert!(params.expires_at.is_none());
        assert!(params.scope.is_none());
        assert!(params.provider_specific.is_none());
        assert!(params.email.is_none());

        let custom_params = StoreOAuthTokensParams {
            access_token: "access_123",
            refresh_token: Some("refresh_456"),
            token_type: "Bearer",
            expires_at: Some("2026-10-01T00:00:00Z"),
            scope: Some("read write"),
            provider_specific: Some("custom_data"),
            email: Some("user@example.com"),
        };

        let copied_params = custom_params;
        assert_eq!(copied_params.access_token, "access_123");
        assert_eq!(copied_params.refresh_token, Some("refresh_456"));
        assert_eq!(copied_params.token_type, "Bearer");
        assert_eq!(copied_params.expires_at, Some("2026-10-01T00:00:00Z"));
        assert_eq!(copied_params.scope, Some("read write"));
        assert_eq!(copied_params.provider_specific, Some("custom_data"));
        assert_eq!(copied_params.email, Some("user@example.com"));

        let debug_str = format!("{custom_params:?}");
        assert!(debug_str.contains("StoreOAuthTokensParams"));
        assert!(debug_str.contains("access_123"));
    }
}
