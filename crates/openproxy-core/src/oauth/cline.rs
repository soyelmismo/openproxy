//! Cline OAuth provider.

use crate::error::{CoreError, Result};
use crate::oauth::{DbRef, DeviceAuthorizationResponse, OAuthFlow, OAuthProvider, TokenResponse};
use base64::Engine;
use openproxy_adapters::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamError, UpstreamRequest,
};
use std::sync::Arc;

#[derive(Clone)]
pub struct ClineOAuthProvider {}

impl ClineOAuthProvider {
    pub fn new() -> Self {
        Self {}
    }
}

impl Default for ClineOAuthProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl OAuthProvider for ClineOAuthProvider {
    fn name(&self) -> &str {
        "cline"
    }

    fn flow(&self) -> OAuthFlow {
        OAuthFlow::AuthorizationCode
    }

    async fn build_auth_url(&self, redirect_uri: &str) -> Result<(String, String, String, String)> {
        let authorize_url = "https://api.cline.bot/api/v1/auth/authorize";

        let state = uuid::Uuid::new_v4().to_string();

        let params = vec![
            ("client_type", "extension".to_string()),
            ("callback_url", redirect_uri.to_string()),
            ("redirect_uri", redirect_uri.to_string()),
            ("state", state.clone()),
        ];

        let url = format!(
            "{authorize_url}?{}",
            crate::oauth::generic::urlencoded_string(&params)
        );

        Ok((url, String::new(), String::new(), state))
    }

    async fn exchange_code(
        &self,
        code: &str,
        _code_verifier: &str,
        upstream_client: &Arc<UpstreamClient>,
        redirect_uri: &str,
    ) -> Result<TokenResponse> {
        let body = serde_json::json!({
            "grant_type": "authorization_code",
            "code": code,
            "client_type": "extension",
            "redirect_uri": redirect_uri,
            "provider": "cline"
        });

        let body_bytes = serde_json::to_vec(&body).unwrap();
        let mut req = UpstreamRequest::post_json(
            "https://api.cline.bot/api/v1/auth/token",
            bytes::Bytes::from(body_bytes),
        );
        req.headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        openproxy_adapters::adapters::cline::apply_cline_spoofing_headers(&mut req);

        let cancel = CancellationToken::new();
        let response = upstream_client
            .call(req, TimeoutProfile::OAuth, cancel)
            .await
            .map_err(|e| match e {
                UpstreamError::Cancel => {
                    CoreError::Cancelled(openproxy_types::CancelReason::ClientDisconnected)
                }
                other => CoreError::UpstreamConnection(format!("cline exchange: {other}")),
            })?;

        let status = response.status;
        let resp_body = response.collect().await.map_err(|e| match e {
            UpstreamError::Cancel => {
                CoreError::Cancelled(openproxy_types::CancelReason::ClientDisconnected)
            }
            other => CoreError::UpstreamConnection(format!("cline exchange body: {other}")),
        })?;

        if !status.is_success() {
            return Err(CoreError::UpstreamError {
                status: status.as_u16(),
                provider: "cline".into(),
                model: "<oauth>".into(),
                body: String::from_utf8_lossy(&resp_body).into(),
                is_proxy_rotated: false,
            });
        }

        #[derive(serde::Deserialize)]
        struct ClineResponse {
            success: bool,
            data: ClineAuthResponseData,
        }

        let resp: ClineResponse = serde_json::from_slice(&resp_body)
            .map_err(|e| CoreError::Parse(format!("cline token parse: {e}")))?;

        if !resp.success {
            return Err(CoreError::Validation("Cline returned success=false".into()));
        }

        let expires_in = parse_expires_in(&resp.data);
        Ok(TokenResponse {
            access_token: resp.data.access_token,
            refresh_token: resp.data.refresh_token,
            token_type: "Bearer".into(),
            expires_in,
            scope: None,
            id_token: None,
        })
    }

    async fn request_device_code(
        &self,
        _upstream_client: &Arc<UpstreamClient>,
    ) -> Result<DeviceAuthorizationResponse> {
        Err(CoreError::Validation(
            "cline uses auth code flow, not device code".into(),
        ))
    }

    async fn poll_device_token(
        &self,
        _device_code: &str,
        _upstream_client: &Arc<UpstreamClient>,
    ) -> Result<Option<TokenResponse>> {
        Err(CoreError::Validation(
            "cline uses auth code flow, not device code".into(),
        ))
    }

    async fn refresh_token(
        &self,
        refresh_token: &str,
        upstream_client: &Arc<UpstreamClient>,
        _account_id: crate::ids::AccountId,
        _db: DbRef<'_>,
    ) -> Result<TokenResponse> {
        let body = serde_json::json!({
            "refreshToken": refresh_token,
            "refresh_token": refresh_token,
            "grantType": "refresh_token",
            "grant_type": "refresh_token"
        });

        let body_bytes = serde_json::to_vec(&body).unwrap();
        let mut req = UpstreamRequest::post_json(
            "https://api.cline.bot/api/v1/auth/refresh",
            bytes::Bytes::from(body_bytes),
        );
        req.headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        openproxy_adapters::adapters::cline::apply_cline_spoofing_headers(&mut req);

        let cancel = CancellationToken::new();
        let response = upstream_client
            .call(req, TimeoutProfile::OAuth, cancel)
            .await
            .map_err(|e| match e {
                UpstreamError::Cancel => {
                    CoreError::Cancelled(openproxy_types::CancelReason::ClientDisconnected)
                }
                other => CoreError::UpstreamConnection(format!("cline refresh: {other}")),
            })?;

        let status = response.status;
        let resp_body = response.collect().await.map_err(|e| match e {
            UpstreamError::Cancel => {
                CoreError::Cancelled(openproxy_types::CancelReason::ClientDisconnected)
            }
            other => CoreError::UpstreamConnection(format!("cline refresh body: {other}")),
        })?;

        if !status.is_success() {
            return Err(CoreError::UpstreamError {
                status: status.as_u16(),
                provider: "cline".into(),
                model: "<oauth>".into(),
                body: String::from_utf8_lossy(&resp_body).into(),
                is_proxy_rotated: false,
            });
        }

        #[derive(serde::Deserialize)]
        struct ClineResponse {
            success: bool,
            data: ClineAuthResponseData,
        }

        let resp: ClineResponse = serde_json::from_slice(&resp_body)
            .map_err(|e| CoreError::Parse(format!("cline token refresh parse: {e}")))?;

        if !resp.success {
            return Err(CoreError::Validation(
                "Cline refresh returned success=false".into(),
            ));
        }

        let expires_in = parse_expires_in(&resp.data);
        Ok(TokenResponse {
            access_token: resp.data.access_token,
            refresh_token: resp.data.refresh_token,
            token_type: "Bearer".into(),
            expires_in,
            scope: None,
            id_token: None,
        })
    }

    fn email_from_token(&self, token: &TokenResponse) -> Option<String> {
        let extract = |claims: &serde_json::Value| -> Option<String> {
            claims
                .get("email")
                .and_then(|v| v.as_str())
                .filter(|v| !v.is_empty())
                .or_else(|| {
                    claims
                        .get("name")
                        .and_then(|v| v.as_str())
                        .filter(|v| !v.is_empty())
                })
                .map(ToString::to_string)
        };

        if let Some(ref id_jwt) = token.id_token
            && let Some(claims) = decode_jwt_payload(id_jwt)
            && let Some(val) = extract(&claims)
        {
            return Some(val);
        }

        let jwt = token
            .access_token
            .strip_prefix("Bearer ")
            .unwrap_or(&token.access_token)
            .strip_prefix("workos:")
            .unwrap_or(&token.access_token);
        let claims = decode_jwt_payload(jwt)?;
        extract(&claims)
    }
}

fn decode_jwt_payload(jwt: &str) -> Option<serde_json::Value> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[derive(serde::Deserialize)]
struct ClineAuthResponseData {
    #[serde(rename = "accessToken", alias = "access_token")]
    access_token: String,
    #[serde(rename = "refreshToken", alias = "refresh_token")]
    refresh_token: Option<String>,
    #[serde(rename = "expiresAt", alias = "expires_at")]
    expires_at: Option<String>,
    #[serde(rename = "expiresIn", alias = "expires_in")]
    expires_in: Option<u64>,
}

fn parse_expires_in(data: &ClineAuthResponseData) -> Option<u64> {
    if let Some(secs) = data.expires_in {
        return Some(secs);
    }
    if let Some(ref ts) = data.expires_at
        && let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts)
    {
        let diff = dt
            .with_timezone(&chrono::Utc)
            .signed_duration_since(chrono::Utc::now());
        return Some(diff.num_seconds().max(0) as u64);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cline_auth_response_data_deserialization_and_ttl() {
        let json_data = serde_json::json!({
            "accessToken": "test_acc",
            "refreshToken": "test_ref",
            "expiresAt": (chrono::Utc::now() + chrono::Duration::seconds(3600)).to_rfc3339()
        });
        let data: ClineAuthResponseData =
            serde_json::from_value(json_data).expect("deserialize valid json");
        assert_eq!(data.access_token, "test_acc");
        assert_eq!(data.refresh_token.as_deref(), Some("test_ref"));
        let expires_in = parse_expires_in(&data).expect("expires_in parsed");
        assert!((3590..=3600).contains(&expires_in));

        let snake_json = serde_json::json!({
            "access_token": "acc2",
            "refresh_token": "ref2",
            "expires_in": 1800u64
        });
        let data_snake: ClineAuthResponseData =
            serde_json::from_value(snake_json).expect("deserialize snake case");
        assert_eq!(data_snake.access_token, "acc2");
        assert_eq!(data_snake.refresh_token.as_deref(), Some("ref2"));
        assert_eq!(parse_expires_in(&data_snake), Some(1800));
    }

    #[test]
    fn test_cline_email_from_token() {
        let provider = ClineOAuthProvider::new();
        // Construct a dummy JWT payload with email
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"alg":"HS256"}"#);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"email":"user@example.com","name":"Test User"}"#);
        let token_str = format!("Bearer workos:{header}.{payload}.sig");
        let token = TokenResponse {
            access_token: token_str,
            token_type: "Bearer".into(),
            expires_in: None,
            refresh_token: None,
            scope: None,
            id_token: None,
        };
        assert_eq!(
            provider.email_from_token(&token).as_deref(),
            Some("user@example.com")
        );
    }
}
