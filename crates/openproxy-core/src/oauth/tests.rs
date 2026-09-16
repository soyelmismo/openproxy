use super::*;
use crate::accounts::{Account, HealthStatus};
use crate::ids::{AccountId, ProviderId};

#[test]
fn oauth_flow_str_roundtrip() {
    assert_eq!(OAuthFlow::DeviceCode.as_str(), "device_code");
    assert_eq!(
        OAuthFlow::AuthorizationCodePkce.as_str(),
        "authorization_code_pkce"
    );
}

#[test]
fn oauth_flow_serde_roundtrip() {
    let flow = OAuthFlow::DeviceCode;
    let json = serde_json::to_string(&flow).unwrap();
    assert_eq!(json, "\"device_code\"");
    let back: OAuthFlow = serde_json::from_str(&json).unwrap();
    assert_eq!(back, OAuthFlow::DeviceCode);
}

#[test]
fn token_response_deserialize() {
    let json = r#"{"access_token":"ya29.test","token_type":"Bearer","expires_in":3600,"refresh_token":"1//0test"}"#;
    let tr: TokenResponse = serde_json::from_str(json).unwrap();
    assert_eq!(tr.access_token, "ya29.test");
    assert_eq!(tr.token_type, "Bearer");
    assert_eq!(tr.expires_in, Some(3600));
    assert_eq!(tr.refresh_token.as_deref(), Some("1//0test"));
}

#[test]
fn device_auth_response_deserialize() {
    let json = r#"{
            "deviceCode": "GmRhmhcxhwAzkoEqiMgzy",
            "userCode": "DJQR-KCZS",
            "verificationUri": "https://example.com/device",
            "expiresIn": 1800,
            "interval": 5
        }"#;
    let dar: DeviceAuthorizationResponse = serde_json::from_str(json).unwrap();
    assert_eq!(dar.device_code, "GmRhmhcxhwAzkoEqiMgzy");
    assert_eq!(dar.user_code, "DJQR-KCZS");
    assert_eq!(dar.verification_uri, "https://example.com/device");
    assert_eq!(dar.expires_in, Some(1800));
    assert_eq!(dar.interval, Some(5));
}

#[test]
fn backoff_seconds_zero_failures() {
    assert_eq!(backoff_seconds(0), BASE_BACKOFF_SECS);
}

#[test]
fn backoff_seconds_exponential_growth() {
    assert_eq!(backoff_seconds(1), 60); // 60 * 2^0
    assert_eq!(backoff_seconds(2), 120); // 60 * 2^1
    assert_eq!(backoff_seconds(3), 240); // 60 * 2^2
    assert_eq!(backoff_seconds(4), 480); // 60 * 2^3
}

#[test]
fn backoff_seconds_caps_at_max() {
    assert_eq!(backoff_seconds(100), MAX_BACKOFF_SECS);
    assert_eq!(backoff_seconds(31), MAX_BACKOFF_SECS);
}

#[test]
fn refresh_lead_seconds_rotating_providers() {
    // Auth0-backed rotating token providers: 5 minutes
    assert_eq!(refresh_lead_seconds("kiro"), 300);
    assert_eq!(refresh_lead_seconds("antigravity"), 300);
}

#[test]
fn refresh_lead_seconds_non_rotating_providers() {
    // Non-rotating providers: 15 minutes (default)
    assert_eq!(refresh_lead_seconds("google"), 900);
    assert_eq!(refresh_lead_seconds("github"), 900);
    assert_eq!(refresh_lead_seconds("iflow"), 900);
    assert_eq!(refresh_lead_seconds("unknown-provider"), 900);
}

#[test]
fn oauth_expires_soon_expired() {
    let account = dummy_account(Some("2020-01-01T00:00:00Z"));
    assert!(oauth_expires_soon(&account, "antigravity"));
}

#[test]
fn oauth_expires_soon_due() {
    let expires_at = (chrono::Utc::now() + chrono::Duration::seconds(120))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let account = dummy_account(Some(&expires_at));
    assert!(oauth_expires_soon(&account, "antigravity"));
}

#[test]
fn oauth_expires_soon_not_due() {
    let expires_at = (chrono::Utc::now() + chrono::Duration::seconds(3_600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let account = dummy_account(Some(&expires_at));
    assert!(!oauth_expires_soon(&account, "antigravity"));
}

#[test]
fn builtin_registry_registers_aliases() {
    let reg = OAuthProviderRegistry::builtin();
    assert!(reg.get("antigravity").is_some());
    assert!(reg.get("antigravity-cli").is_some());
    assert_eq!(
        reg.get("antigravity-cli").unwrap().name(),
        reg.get("antigravity").unwrap().name()
    );
}

fn dummy_account(expires_at: Option<&str>) -> Account {
    Account {
        id: AccountId(1),
        provider_id: ProviderId::new("antigravity"),
        label: None,
        priority: 0,
        extra_config_json: None,
        health_status: HealthStatus::Healthy,
        rate_limited_until: None,
        quota_session_used: None,
        quota_session_limit: None,
        quota_session_reset_at: None,
        quota_weekly_used: None,
        quota_weekly_limit: None,
        quota_weekly_reset_at: None,
        quota_plan_name: None,
        quota_last_fetched_at: None,
        quota_fetch_error: None,
        quota_model_details: None,
        auth_type: "oauth".into(),
        email: Some("t@example.com".into()),
        oauth_scope: None,
        oauth_provider_specific: None,
        expires_at: expires_at.map(Into::into),
        created_at: chrono::Utc::now()
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string()
            .into_boxed_str(),
        current_proxy_id: None,
    }
}
