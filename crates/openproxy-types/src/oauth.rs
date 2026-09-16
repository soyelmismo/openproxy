//! OAuth2 data transfer types.

use serde::{Deserialize, Serialize};

/// Standard OAuth2 token response from the token endpoint (RFC 6749 §5.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenResponse {
    #[serde(rename = "access_token", alias = "accessToken")]
    pub access_token: String,
    #[serde(default, rename = "token_type", alias = "tokenType")]
    pub token_type: String,
    #[serde(default, rename = "expires_in", alias = "expiresIn")]
    pub expires_in: Option<u64>,
    #[serde(default, rename = "refresh_token", alias = "refreshToken")]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default, rename = "id_token", alias = "idToken")]
    pub id_token: Option<String>,
}

/// Device Authorization Response (RFC 8628 §3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceAuthorizationResponse {
    #[serde(rename = "deviceCode", alias = "device_code")]
    pub device_code: String,
    #[serde(rename = "userCode", alias = "user_code")]
    pub user_code: String,
    #[serde(rename = "verificationUri", alias = "verification_uri")]
    pub verification_uri: String,
    #[serde(
        default,
        rename = "verificationUriComplete",
        alias = "verification_uri_complete"
    )]
    pub verification_uri_complete: Option<String>,
    #[serde(default, rename = "expiresIn", alias = "expires_in")]
    pub expires_in: Option<u64>,
    #[serde(default)]
    pub interval: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_response_serde_snake_and_camel() {
        let json_snake = r#"{
            "access_token": "at_123",
            "token_type": "Bearer",
            "expires_in": 3600,
            "refresh_token": "rt_456",
            "scope": "read write",
            "id_token": "id_789"
        }"#;
        let resp: TokenResponse = serde_json::from_str(json_snake).unwrap();
        assert_eq!(resp.access_token, "at_123");
        assert_eq!(resp.token_type, "Bearer");
        assert_eq!(resp.expires_in, Some(3600));
        assert_eq!(resp.refresh_token.as_deref(), Some("rt_456"));
        assert_eq!(resp.scope.as_deref(), Some("read write"));
        assert_eq!(resp.id_token.as_deref(), Some("id_789"));

        let json_camel = r#"{
            "accessToken": "at_123",
            "tokenType": "Bearer",
            "expiresIn": 3600,
            "refreshToken": "rt_456",
            "idToken": "id_789"
        }"#;
        let resp2: TokenResponse = serde_json::from_str(json_camel).unwrap();
        assert_eq!(resp2.access_token, "at_123");
        assert_eq!(resp2.token_type, "Bearer");
    }

    #[test]
    fn test_device_auth_response_serde() {
        let json = r#"{
            "device_code": "dc_1",
            "user_code": "uc_1",
            "verification_uri": "https://auth.example.com/device",
            "verification_uri_complete": "https://auth.example.com/device?user_code=uc_1",
            "expires_in": 1800,
            "interval": 5
        }"#;
        let resp: DeviceAuthorizationResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.device_code, "dc_1");
        assert_eq!(resp.user_code, "uc_1");
        assert_eq!(resp.verification_uri, "https://auth.example.com/device");
        assert_eq!(
            resp.verification_uri_complete.as_deref(),
            Some("https://auth.example.com/device?user_code=uc_1")
        );
        assert_eq!(resp.expires_in, Some(1800));
        assert_eq!(resp.interval, Some(5));
    }
}
