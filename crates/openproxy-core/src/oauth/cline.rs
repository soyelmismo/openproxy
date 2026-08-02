//! Cline OAuth provider.

use std::sync::Arc;
use crate::error::{CoreError, Result};
use crate::oauth::{DeviceAuthorizationResponse, OAuthFlow, OAuthProvider, TokenResponse, DbRef};
use openproxy_adapters::upstream::{CancellationToken, TimeoutProfile, UpstreamClient, UpstreamError, UpstreamRequest};

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
        
        let url = format!("{authorize_url}?{}", crate::oauth::generic::urlencoded_string(&params));
        
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
        let mut req = UpstreamRequest::post_json("https://api.cline.bot/api/v1/auth/token", bytes::Bytes::from(body_bytes));
        req.headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        req.headers.insert(
            http::header::USER_AGENT,
            http::HeaderValue::from_static("Cline/3.5.0"),
        );
        req.headers.insert(
            http::header::HeaderName::from_static("x-is-multiroot"),
            http::HeaderValue::from_static("false"),
        );
        req.headers.insert(
            http::header::HeaderName::from_static("x-client-type"),
            http::HeaderValue::from_static("cline-sdk"),
        );
        req.headers.insert(
            http::header::HeaderName::from_static("x-client-version"),
            http::HeaderValue::from_static("3.5.0"),
        );
        req.headers.insert(
            http::header::HeaderName::from_static("x-core-version"),
            http::HeaderValue::from_static("3.5.0"),
        );
        
        let cancel = CancellationToken::new();
        let response = upstream_client
            .call(req, TimeoutProfile::OAuth, cancel)
            .await
            .map_err(|e| match e {
                UpstreamError::Cancel => CoreError::Cancelled(openproxy_types::CancelReason::ClientDisconnected),
                other => CoreError::UpstreamConnection(format!("cline exchange: {other}")),
            })?;
            
        let status = response.status;
        let resp_body = response.collect().await.map_err(|e| match e {
            UpstreamError::Cancel => CoreError::Cancelled(openproxy_types::CancelReason::ClientDisconnected),
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
        
        #[derive(serde::Deserialize)]
        struct ClineAuthResponseData {
            #[serde(rename = "accessToken")]
            access_token: String,
            #[serde(rename = "refreshToken")]
            refresh_token: Option<String>,
        }
        
        let resp: ClineResponse = serde_json::from_slice(&resp_body)
            .map_err(|e| CoreError::Parse(format!("cline token parse: {e}")))?;
            
        if !resp.success {
            return Err(CoreError::Validation("Cline returned success=false".into()));
        }
        
        Ok(TokenResponse {
            access_token: resp.data.access_token,
            refresh_token: resp.data.refresh_token,
            token_type: "Bearer".into(),
            expires_in: None,
            scope: None,
            id_token: None,
        })
    }
    
    async fn request_device_code(
        &self,
        _upstream_client: &Arc<UpstreamClient>,
    ) -> Result<DeviceAuthorizationResponse> {
        Err(CoreError::Validation("cline uses auth code flow, not device code".into()))
    }

    async fn poll_device_token(
        &self,
        _device_code: &str,
        _upstream_client: &Arc<UpstreamClient>,
    ) -> Result<Option<TokenResponse>> {
        Err(CoreError::Validation("cline uses auth code flow, not device code".into()))
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
            "grantType": "refresh_token"
        });
        
        let body_bytes = serde_json::to_vec(&body).unwrap();
        let mut req = UpstreamRequest::post_json("https://api.cline.bot/api/v1/auth/refresh", bytes::Bytes::from(body_bytes));
        req.headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        req.headers.insert(
            http::header::USER_AGENT,
            http::HeaderValue::from_static("Cline/3.5.0"),
        );
        req.headers.insert(
            http::header::HeaderName::from_static("x-is-multiroot"),
            http::HeaderValue::from_static("false"),
        );
        req.headers.insert(
            http::header::HeaderName::from_static("x-client-type"),
            http::HeaderValue::from_static("cline-sdk"),
        );
        req.headers.insert(
            http::header::HeaderName::from_static("x-client-version"),
            http::HeaderValue::from_static("3.5.0"),
        );
        req.headers.insert(
            http::header::HeaderName::from_static("x-core-version"),
            http::HeaderValue::from_static("3.5.0"),
        );
        
        let cancel = CancellationToken::new();
        let response = upstream_client
            .call(req, TimeoutProfile::OAuth, cancel)
            .await
            .map_err(|e| match e {
                UpstreamError::Cancel => CoreError::Cancelled(openproxy_types::CancelReason::ClientDisconnected),
                other => CoreError::UpstreamConnection(format!("cline refresh: {other}")),
            })?;
            
        let status = response.status;
        let resp_body = response.collect().await.map_err(|e| match e {
            UpstreamError::Cancel => CoreError::Cancelled(openproxy_types::CancelReason::ClientDisconnected),
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
        
        #[derive(serde::Deserialize)]
        struct ClineAuthResponseData {
            #[serde(rename = "accessToken")]
            access_token: String,
            #[serde(rename = "refreshToken")]
            refresh_token: Option<String>,
        }
        
        let resp: ClineResponse = serde_json::from_slice(&resp_body)
            .map_err(|e| CoreError::Parse(format!("cline token refresh parse: {e}")))?;
            
        if !resp.success {
            return Err(CoreError::Validation("Cline refresh returned success=false".into()));
        }
        
        Ok(TokenResponse {
            access_token: resp.data.access_token,
            refresh_token: resp.data.refresh_token,
            token_type: "Bearer".into(),
            expires_in: None,
            scope: None,
            id_token: None,
        })
    }
}
