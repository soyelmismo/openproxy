//! CodeBuddy (`@tencent-ai/codebuddy-code`) OAuth 2.0 provider.
//!
//! Implements Tencent CodeBuddy CLI External Link / Device Code authorization flow:
//! - State Request: `POST https://www.codebuddy.ai/v2/plugin/auth/state?platform=CLI` with body `{}`.
//!   Returns `state` (UUID) and `authUrl` (verification URL for the user to open in the browser).
//! - Polling: `GET https://www.codebuddy.ai/v2/plugin/auth/token?state={state}`.
//!   While response is `{"code": 11217, "msg": "login ing..."}`, returns pending (`None`).
//!   On success (`code: 0`), extracts `accessToken`, `refreshToken`, `expiresIn`, `tokenType: "Bearer"`.
//! - Refresh: `POST https://www.codebuddy.ai/v2/plugin/auth/token/refresh` with body `{}` and
//!   headers `X-Refresh-Token: <refreshToken>` and `X-Auth-Refresh-Source: plugin`.

use std::sync::Arc;

use openproxy_adapters::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest,
};
use openproxy_db::secrets::MasterKey;
use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};
use crate::ids::AccountId;
use crate::oauth::{
    DbRef, DeviceAuthorizationResponse, OAuthFlow, OAuthProvider, TokenResponse, map_upstream_err,
};

pub const DEFAULT_CODEBUDDY_BASE_URL: &str = "https://www.codebuddy.ai/v2";
pub const CODEBUDDY_STATE_PATH: &str = "/plugin/auth/state?platform=CLI";
pub const CODEBUDDY_TOKEN_PATH: &str = "/plugin/auth/token";
pub const CODEBUDDY_REFRESH_PATH: &str = "/plugin/auth/token/refresh";

pub const LOGIN_TOKEN_PENDING_CODE: i64 = 11217;
pub const LOGIN_ACCOUNT_PENDING_CODE: i64 = 12151;

/// Resolve canonical base URL for CodeBuddy API calls.
/// Respects `OPENPROXY_CODEBUDDY_BASE_URL` or `OPENPROXY_CODEBUDDY_AUTH_BASE_URL` env vars if set.
pub fn codebuddy_base_url() -> String {
    std::env::var("OPENPROXY_CODEBUDDY_BASE_URL")
        .or_else(|_| std::env::var("OPENPROXY_CODEBUDDY_AUTH_BASE_URL"))
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_CODEBUDDY_BASE_URL.to_string())
}

/// Extracts numeric return code from either standard envelope or nested response.
pub(crate) fn read_envelope_code(val: &serde_json::Value) -> Option<i64> {
    if let Some(code) = val.get("code").and_then(serde_json::Value::as_i64) {
        return Some(code);
    }
    let resp = val.get("response")?;
    resp.get("data")
        .and_then(|d| d.get("code"))
        .or_else(|| resp.get("code"))
        .and_then(serde_json::Value::as_i64)
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CodeBuddyAccountMeta {
    pub refresh_expires_in: Option<u64>,
}

#[derive(Clone, Default)]
pub struct CodeBuddyOAuthProvider {
    custom_base_url: Option<String>,
}

impl CodeBuddyOAuthProvider {
    pub fn new() -> Self {
        Self {
            custom_base_url: None,
        }
    }

    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            custom_base_url: Some(base_url.into()),
        }
    }

    pub fn base_url(&self) -> String {
        if let Some(ref base) = self.custom_base_url {
            return base.clone();
        }
        codebuddy_base_url()
    }
}

impl OAuthProvider for CodeBuddyOAuthProvider {
    fn name(&self) -> &'static str {
        "codebuddy"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["codebuddy-code", "@tencent-ai/codebuddy-code"]
    }

    fn flow(&self) -> OAuthFlow {
        OAuthFlow::DeviceCode
    }

    fn build_auth_url(
        &self,
        _redirect_uri: &str,
    ) -> impl std::future::Future<Output = Result<(String, String, String, String)>> + Send {
        std::future::ready(Err(CoreError::Validation(
            "codebuddy uses device code flow, not PKCE".into(),
        )))
    }

    fn exchange_code(
        &self,
        _code: &str,
        _code_verifier: &str,
        _upstream_client: &Arc<UpstreamClient>,
        _redirect_uri: &str,
    ) -> impl std::future::Future<Output = Result<TokenResponse>> + Send {
        std::future::ready(Err(CoreError::Validation(
            "codebuddy uses device code flow, not authorization code".into(),
        )))
    }

    async fn request_device_code(
        &self,
        upstream_client: &Arc<UpstreamClient>,
    ) -> Result<DeviceAuthorizationResponse> {
        let base = self.base_url();
        let trimmed_base = base.trim_end_matches('/');
        let url = format!("{trimmed_base}{CODEBUDDY_STATE_PATH}");

        let mut req = UpstreamRequest::post_json(&url, bytes::Bytes::from_static(b"{}"));
        req.headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        req.headers.insert(
            http::header::ACCEPT,
            http::HeaderValue::from_static("application/json"),
        );
        req.headers.insert(
            http::header::HeaderName::from_static("x-no-authorization"),
            http::HeaderValue::from_static("true"),
        );
        req.headers.insert(
            http::header::HeaderName::from_static("x-no-user-id"),
            http::HeaderValue::from_static("true"),
        );
        req.headers.insert(
            http::header::HeaderName::from_static("x-no-enterprise-id"),
            http::HeaderValue::from_static("true"),
        );
        req.headers.insert(
            http::header::HeaderName::from_static("x-no-department-info"),
            http::HeaderValue::from_static("true"),
        );
        openproxy_adapters::apply_codebuddy_spoofing_headers(&mut req);

        let cancel = CancellationToken::new();
        let response = upstream_client
            .call(req, TimeoutProfile::OAuth, cancel)
            .await
            .map_err(|e| map_upstream_err(e, "codebuddy device state request"))?;

        let status = response.status;
        let body = response
            .collect()
            .await
            .map_err(|e| map_upstream_err(e, "codebuddy device state body read"))?;

        super::check_oauth_status(status, "codebuddy", &body)?;

        let json: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|e| CoreError::Parse(format!("codebuddy device state parse: {e}")))?;

        if let Some(code) = read_envelope_code(&json)
            && code != 0
        {
            let msg = json
                .get("msg")
                .or_else(|| json.get("message"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown error");
            return Err(CoreError::upstream_error(
                status.as_u16(),
                "codebuddy",
                "<oauth>",
                format!("codebuddy device state failed [code {code}]: {msg}"),
                false,
            ));
        }

        let data = json.get("data").unwrap_or(&json);
        let state = data
            .get("state")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| CoreError::Parse("codebuddy device state missing 'state'".into()))?;
        let auth_url = data
            .get("authUrl")
            .or_else(|| data.get("auth_url"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| CoreError::Parse("codebuddy device state missing 'authUrl'".into()))?;

        Ok(DeviceAuthorizationResponse {
            device_code: state.to_string(),
            user_code: state.to_string(),
            verification_uri: auth_url.to_string(),
            verification_uri_complete: Some(auth_url.to_string()),
            expires_in: Some(300),
            interval: Some(2),
        })
    }

    async fn poll_device_token(
        &self,
        device_code: &str,
        upstream_client: &Arc<UpstreamClient>,
    ) -> Result<Option<TokenResponse>> {
        let base = self.base_url();
        let trimmed_base = base.trim_end_matches('/');
        let encoded_state = urlencoding::encode(device_code);
        let url = format!("{trimmed_base}{CODEBUDDY_TOKEN_PATH}?state={encoded_state}");

        let mut req = UpstreamRequest::get(&url);
        req.headers.insert(
            http::header::ACCEPT,
            http::HeaderValue::from_static("application/json"),
        );
        req.headers.insert(
            http::header::HeaderName::from_static("x-no-authorization"),
            http::HeaderValue::from_static("true"),
        );
        req.headers.insert(
            http::header::HeaderName::from_static("x-no-user-id"),
            http::HeaderValue::from_static("true"),
        );
        req.headers.insert(
            http::header::HeaderName::from_static("x-no-enterprise-id"),
            http::HeaderValue::from_static("true"),
        );
        req.headers.insert(
            http::header::HeaderName::from_static("x-no-department-info"),
            http::HeaderValue::from_static("true"),
        );
        openproxy_adapters::apply_codebuddy_spoofing_headers(&mut req);

        let cancel = CancellationToken::new();
        let response = upstream_client
            .call(req, TimeoutProfile::OAuth, cancel)
            .await
            .map_err(|e| map_upstream_err(e, "codebuddy device token poll"))?;

        let status = response.status;
        let body = response
            .collect()
            .await
            .map_err(|e| map_upstream_err(e, "codebuddy device token poll read"))?;

        // 404 or 428 standard pending response
        if status.as_u16() == 404 || status.as_u16() == 428 {
            return Ok(None);
        }

        let json_res: std::result::Result<serde_json::Value, _> = serde_json::from_slice(&body);
        if let Ok(json) = json_res {
            if let Some(code) = read_envelope_code(&json) {
                if code == LOGIN_TOKEN_PENDING_CODE || code == LOGIN_ACCOUNT_PENDING_CODE {
                    return Ok(None);
                }
                if code != 0 {
                    let msg = json
                        .get("msg")
                        .or_else(|| json.get("message"))
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown error");
                    return Err(CoreError::upstream_error(
                        status.as_u16(),
                        "codebuddy",
                        "<oauth>",
                        format!("codebuddy login poll failed [code {code}]: {msg}"),
                        false,
                    ));
                }
            } else if let Some(msg) = json.get("msg").and_then(serde_json::Value::as_str)
                && msg.contains("login ing")
            {
                return Ok(None);
            }

            super::check_oauth_status(status, "codebuddy", &body)?;

            let data = json.get("data").unwrap_or(&json);
            let Some(access_token) = data
                .get("accessToken")
                .or_else(|| data.get("access_token"))
                .and_then(serde_json::Value::as_str)
            else {
                if let Some(msg) = json.get("msg").and_then(serde_json::Value::as_str)
                    && msg.contains("login ing")
                {
                    return Ok(None);
                }
                return Err(CoreError::Parse(
                    "codebuddy token response missing 'accessToken'".into(),
                ));
            };

            let refresh_token = data
                .get("refreshToken")
                .or_else(|| data.get("refresh_token"))
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string);

            let expires_in = data
                .get("expiresIn")
                .or_else(|| data.get("expires_in"))
                .and_then(serde_json::Value::as_u64);

            let token_type = data
                .get("tokenType")
                .or_else(|| data.get("token_type"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("Bearer")
                .to_string();

            let id_token = data
                .get("idToken")
                .or_else(|| data.get("id_token"))
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string);

            return Ok(Some(TokenResponse {
                access_token: access_token.to_string(),
                token_type,
                expires_in,
                refresh_token,
                scope: None,
                id_token,
            }));
        }

        super::check_oauth_status(status, "codebuddy", &body)?;
        Err(CoreError::Parse("codebuddy poll invalid JSON".into()))
    }

    async fn refresh_token(
        &self,
        refresh_token: &str,
        upstream_client: &Arc<UpstreamClient>,
        _account_id: AccountId,
        _db: DbRef<'_>,
    ) -> Result<TokenResponse> {
        let base = self.base_url();
        let trimmed_base = base.trim_end_matches('/');
        let url = format!("{trimmed_base}{CODEBUDDY_REFRESH_PATH}");

        let mut req = UpstreamRequest::post_json(&url, bytes::Bytes::from_static(b"{}"));
        req.headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        req.headers.insert(
            http::header::ACCEPT,
            http::HeaderValue::from_static("application/json"),
        );
        if let Ok(val) = http::HeaderValue::from_str(refresh_token) {
            req.headers.insert(
                http::header::HeaderName::from_static("x-refresh-token"),
                val,
            );
        }
        req.headers.insert(
            http::header::HeaderName::from_static("x-auth-refresh-source"),
            http::HeaderValue::from_static("plugin"),
        );
        openproxy_adapters::apply_codebuddy_spoofing_headers(&mut req);

        let cancel = CancellationToken::new();
        let response = upstream_client
            .call(req, TimeoutProfile::OAuth, cancel)
            .await
            .map_err(|e| map_upstream_err(e, "codebuddy token refresh"))?;

        let status = response.status;
        let body = response
            .collect()
            .await
            .map_err(|e| map_upstream_err(e, "codebuddy token refresh read"))?;

        super::check_oauth_status(status, "codebuddy", &body)?;

        let json: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|e| CoreError::Parse(format!("codebuddy token refresh parse: {e}")))?;

        if let Some(code) = read_envelope_code(&json)
            && code != 0
        {
            let msg = json
                .get("msg")
                .or_else(|| json.get("message"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown error");
            return Err(CoreError::upstream_error(
                status.as_u16(),
                "codebuddy",
                "<oauth-refresh>",
                format!("codebuddy refresh failed [code {code}]: {msg}"),
                false,
            ));
        }

        let data = json.get("data").unwrap_or(&json);
        let access_token = data
            .get("accessToken")
            .or_else(|| data.get("access_token"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                CoreError::Parse("codebuddy refresh missing 'accessToken'".into())
            })?;

        let new_refresh_token = data
            .get("refreshToken")
            .or_else(|| data.get("refresh_token"))
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string)
            .or_else(|| Some(refresh_token.to_string()));

        let expires_in = data
            .get("expiresIn")
            .or_else(|| data.get("expires_in"))
            .and_then(serde_json::Value::as_u64);

        let token_type = data
            .get("tokenType")
            .or_else(|| data.get("token_type"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Bearer")
            .to_string();

        let id_token = data
            .get("idToken")
            .or_else(|| data.get("id_token"))
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string);

        Ok(TokenResponse {
            access_token: access_token.to_string(),
            token_type,
            expires_in,
            refresh_token: new_refresh_token,
            scope: None,
            id_token,
        })
    }

    fn email_from_token(&self, token: &TokenResponse) -> Option<String> {
        super::extract_email_from_token(token)
    }

    fn provider_specific_from_token(&self, token: &TokenResponse) -> Option<String> {
        let mut meta = serde_json::Map::new();
        meta.insert(
            "token_type".to_string(),
            serde_json::Value::String(token.token_type.clone()),
        );
        if let Some(exp) = token.expires_in {
            meta.insert(
                "expires_in".to_string(),
                serde_json::Value::Number(exp.into()),
            );
        }
        meta.insert(
            "credit_balance".to_string(),
            serde_json::Value::Number(30.into()),
        );
        meta.insert(
            "daily_credits".to_string(),
            serde_json::Value::Number(30.into()),
        );
        meta.insert(
            "provider".to_string(),
            serde_json::Value::String("codebuddy".to_string()),
        );
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        meta.insert(
            "last_checkin_date".to_string(),
            serde_json::Value::String(today),
        );
        meta.insert(
            "streak_days".to_string(),
            serde_json::Value::Number(1.into()),
        );
        serde_json::to_string(&meta).ok()
    }

    async fn post_exchange(
        &self,
        account_id: AccountId,
        db_pool: &Arc<openproxy_db::DbPool>,
        master_key: &MasterKey,
        upstream: &Arc<UpstreamClient>,
    ) -> Result<()> {
        let pool = Arc::clone(db_pool);
        let key = master_key.clone();
        let decrypted_token = tokio::task::spawn_blocking(move || {
            let conn = pool.reader();
            crate::accounts::decrypt_access_token(&conn, account_id, &key)
        })
        .await
        .map_err(|e| CoreError::Internal(e.to_string()))??;

        let quota = crate::admin::fetch_account_quota_with_proxy(
            "codebuddy",
            upstream,
            "",
            Some(&decrypted_token),
            None,
            None,
        )
        .await;

        let pool = Arc::clone(db_pool);
        tokio::task::spawn_blocking(move || {
            let conn = pool.writer();
            openproxy_db::accounts::set_quota(&conn, account_id, &quota)
        })
        .await
        .map_err(|e| CoreError::Internal(e.to_string()))??;

        Ok(())
    }
}
