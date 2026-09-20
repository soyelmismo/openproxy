//! Z.ai (ZCode / GLM) OAuth 2.0 provider.
//!
//! Implements Authorization Code flow against ZCode public endpoints:
//! - Authorize: `https://chat.z.ai/auth/oauth/authorize`
//! - Token Exchange: `https://zcode.z.ai/api/v1/oauth/token`
//! - Business Login: `https://api.z.ai/api/auth/z/login`
//! - User Info: `https://chat.z.ai/api/oauth/userinfo`

use crate::error::{CoreError, Result};
use crate::ids::AccountId;
use crate::oauth::{
    DbRef, DeviceAuthorizationResponse, OAuthFlow, OAuthProvider, TokenResponse, map_upstream_err,
};
use openproxy_adapters::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest,
};
use openproxy_db::secrets::MasterKey;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub const ZAI_CLIENT_ID: &str = "client_P8X5CMWmlaRO9gyO-KSqtg";
pub const ZAI_DEFAULT_AUTHORIZE_URL: &str = "https://chat.z.ai/api/oauth/authorize";
pub const ZAI_DEFAULT_TOKEN_URL: &str = "https://zcode.z.ai/api/v1/oauth/token";
pub const ZAI_DEFAULT_BUSINESS_LOGIN_URL: &str = "https://api.z.ai/api/auth/z/login";
pub const ZAI_DEFAULT_USERINFO_URL: &str = "https://chat.z.ai/api/oauth/userinfo";
pub const ZAI_REDIRECT_URI: &str =
    "https://zcode.z.ai/app/oauth/login?redirect=zcode%3A%2F%2Foauth%2Fcallback&app_version=3.14.0";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ZaiAccountMeta {
    pub zcode_jwt_token: Option<String>,
    pub zai_access_token: Option<String>,
    pub business_access_token: Option<String>,
    pub api_key: Option<String>,
    pub user_id: Option<String>,
    pub name: Option<String>,
    pub email: Option<String>,
    pub avatar: Option<String>,
}

#[derive(Clone, Default)]
pub struct ZaiOAuthProvider;

impl ZaiOAuthProvider {
    pub fn new() -> Self {
        Self
    }
}

impl OAuthProvider for ZaiOAuthProvider {
    fn name(&self) -> &'static str {
        "zai"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["zcode", "z.ai"]
    }

    fn flow(&self) -> OAuthFlow {
        OAuthFlow::AuthorizationCode
    }

    fn build_auth_url(
        &self,
        _redirect_uri: &str,
    ) -> impl std::future::Future<Output = Result<(String, String, String, String)>> + Send {
        let state = uuid::Uuid::new_v4().to_string();
        let effective_redirect = ZAI_REDIRECT_URI;

        let params = [
            ("response_type", "code"),
            ("client_id", ZAI_CLIENT_ID),
            ("redirect_uri", effective_redirect),
            ("state", &state),
        ];

        let query = crate::oauth::generic::urlencoded_string(&params);
        let auth_url = format!("{ZAI_DEFAULT_AUTHORIZE_URL}?{query}");

        std::future::ready(Ok((
            auth_url,
            state.clone(),
            effective_redirect.to_string(),
            state,
        )))
    }

    async fn exchange_code(
        &self,
        code: &str,
        code_verifier: &str,
        upstream_client: &Arc<UpstreamClient>,
        _redirect_uri: &str,
    ) -> Result<TokenResponse> {
        let (actual_code, extracted_state) = crate::oauth::util::parse_oauth_callback_input(code);

        // Determine effective state:
        // 1. Extracted from callback URL / fragment
        // 2. Passed via code_verifier (stored during build_auth_url)
        // 3. Fallback to random UUID (Z.ai strictly requires non-empty state with code 3001)
        let effective_state = extracted_state
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                let cv = code_verifier.trim();
                if !cv.is_empty() {
                    Some(cv.to_string())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        let effective_redirect = ZAI_REDIRECT_URI;

        let body = serde_json::json!({
            "provider": "zai",
            "code": actual_code.trim(),
            "redirect_uri": effective_redirect,
            "state": effective_state.trim()
        });

        let body_bytes =
            serde_json::to_vec(&body).map_err(|e| CoreError::Validation(e.to_string()))?;

        let mut req =
            UpstreamRequest::post_json(ZAI_DEFAULT_TOKEN_URL, bytes::Bytes::from(body_bytes));
        req.headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        req.headers.insert(
            http::header::ACCEPT,
            http::HeaderValue::from_static("application/json"),
        );
        req.headers.insert(
            http::header::USER_AGENT,
            http::HeaderValue::from_static("ZCode/3.14.0"),
        );

        let cancel = CancellationToken::new();
        let response = upstream_client
            .call(req, TimeoutProfile::OAuth, cancel)
            .await
            .map_err(|e| map_upstream_err(e, "zai exchange"))?;

        let status = response.status;
        let resp_bytes = response
            .collect()
            .await
            .map_err(|e| map_upstream_err(e, "zai exchange body"))?;

        crate::oauth::check_oauth_status(status, "zai", &resp_bytes)?;

        let json: serde_json::Value = serde_json::from_slice(&resp_bytes)
            .map_err(|e| CoreError::Parse(format!("zai token parse: {e}")))?;

        let code_num = json
            .get("code")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(-1);
        if code_num != 0 {
            let msg = json
                .get("msg")
                .or_else(|| json.get("message"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("Z.ai backend token exchange failed");
            return Err(CoreError::Validation(format!(
                "Z.ai token exchange error (code {code_num}): {msg}"
            )));
        }

        let data = json
            .get("data")
            .ok_or_else(|| CoreError::Parse("zai token response missing data object".into()))?;

        let zcode_jwt = data
            .get("token")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| CoreError::Parse("zai token response missing data.token".into()))?
            .trim()
            .to_string();

        let zai_access_token = data
            .get("zai")
            .and_then(|z| z.get("access_token"))
            .and_then(serde_json::Value::as_str)
            .map(|s| s.trim().to_string());

        let expires_in = data
            .get("expires_in")
            .and_then(serde_json::Value::as_u64)
            .or(Some(86400 * 30));

        let user_val = data.get("user");
        let user_id = user_val
            .and_then(|u| u.get("id").or_else(|| u.get("user_id")))
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string);
        let name = user_val
            .and_then(|u| u.get("name").or_else(|| u.get("username")))
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string);
        let email = user_val
            .and_then(|u| u.get("email"))
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string);
        let avatar = user_val
            .and_then(|u| u.get("avatar"))
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string);

        let business_access_token = match zai_access_token.as_deref() {
            Some(tok) => Self::exchange_business_token(upstream_client, tok)
                .await
                .ok(),
            None => None,
        };

        let api_key = match business_access_token.as_deref() {
            Some(tok) => {
                openproxy_adapters::adapters::zai::resolve_zai_api_key(upstream_client, tok).await
            }
            None => None,
        };

        let effective_access_token = api_key
            .clone()
            .or_else(|| business_access_token.clone())
            .unwrap_or_else(|| zcode_jwt.clone());

        let meta = ZaiAccountMeta {
            zcode_jwt_token: Some(zcode_jwt.clone()),
            zai_access_token,
            business_access_token,
            api_key,
            user_id,
            name,
            email,
            avatar,
        };

        let id_token = serde_json::to_string(&meta).ok();

        Ok(TokenResponse {
            access_token: effective_access_token,
            token_type: "Bearer".into(),
            expires_in,
            refresh_token: None,
            scope: None,
            id_token,
        })
    }

    fn request_device_code(
        &self,
        _upstream_client: &Arc<UpstreamClient>,
    ) -> impl std::future::Future<Output = Result<DeviceAuthorizationResponse>> + Send {
        std::future::ready(Err(CoreError::Validation(
            "Z.ai uses Authorization Code flow, not Device Code".into(),
        )))
    }

    fn poll_device_token(
        &self,
        _device_code: &str,
        _upstream_client: &Arc<UpstreamClient>,
    ) -> impl std::future::Future<Output = Result<Option<TokenResponse>>> + Send {
        std::future::ready(Err(CoreError::Validation(
            "Z.ai uses Authorization Code flow, not Device Code".into(),
        )))
    }

    fn refresh_token(
        &self,
        _refresh_token: &str,
        _upstream_client: &Arc<UpstreamClient>,
        _account_id: AccountId,
        _db: DbRef<'_>,
    ) -> impl std::future::Future<Output = Result<TokenResponse>> + Send {
        std::future::ready(Err(CoreError::Validation(
            "Z.ai does not support token refresh; re-authentication required".into(),
        )))
    }

    fn provider_specific_from_token(&self, token: &TokenResponse) -> Option<String> {
        token.id_token.clone()
    }

    fn email_from_token(&self, token: &TokenResponse) -> Option<String> {
        token
            .id_token
            .as_deref()
            .and_then(|raw| serde_json::from_str::<ZaiAccountMeta>(raw).ok())
            .and_then(|m| m.email)
            .filter(|e| !e.trim().is_empty())
    }

    async fn post_exchange(
        &self,
        account_id: AccountId,
        db_pool: &Arc<openproxy_db::DbPool>,
        _master_key: &MasterKey,
        _upstream: &Arc<UpstreamClient>,
    ) -> Result<()> {
        const WRITER_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
        let pool = Arc::clone(db_pool);

        tokio::task::spawn_blocking(move || {
            let conn = pool.try_writer_for(WRITER_LOCK_TIMEOUT).ok_or_else(|| {
                CoreError::Internal(format!(
                    "zai post_exchange: writer lock not acquired within {WRITER_LOCK_TIMEOUT:?}"
                ))
            })?;

            let meta_raw: Option<String> = conn
                .query_row(
                    "SELECT oauth_provider_specific FROM accounts WHERE id = ?1",
                    rusqlite::params![account_id.0],
                    |row| row.get(0),
                )
                .ok()
                .flatten();

            if let Some(raw) = meta_raw
                && let Ok(meta) = serde_json::from_str::<ZaiAccountMeta>(&raw)
            {
                let user_identifier = meta
                    .name
                    .or(meta.email)
                    .or(meta.user_id)
                    .filter(|s| !s.trim().is_empty());

                if let Some(ident) = user_identifier {
                    let label = format!("Z.ai ({ident})");
                    let _ = conn.execute(
                        "UPDATE accounts SET label = COALESCE(NULLIF(label, ''), ?1) WHERE id = ?2",
                        rusqlite::params![label, account_id.0],
                    );
                }
            }

            Ok::<(), CoreError>(())
        })
        .await
        .map_err(|e| CoreError::Internal(format!("zai post_exchange spawn join error: {e}")))??;

        Ok(())
    }
}

impl ZaiOAuthProvider {
    async fn exchange_business_token(
        upstream_client: &Arc<UpstreamClient>,
        zai_access_token: &str,
    ) -> Result<String> {
        let body = serde_json::json!({
            "token": zai_access_token
        });
        let body_bytes =
            serde_json::to_vec(&body).map_err(|e| CoreError::Validation(e.to_string()))?;

        let mut req = UpstreamRequest::post_json(
            ZAI_DEFAULT_BUSINESS_LOGIN_URL,
            bytes::Bytes::from(body_bytes),
        );
        req.headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        req.headers.insert(
            http::header::ACCEPT,
            http::HeaderValue::from_static("application/json"),
        );

        let cancel = CancellationToken::new();
        let response = upstream_client
            .call(req, TimeoutProfile::OAuth, cancel)
            .await
            .map_err(|e| map_upstream_err(e, "zai business login"))?;

        if !response.status.is_success() {
            return Err(CoreError::UpstreamConnection(format!(
                "business login status: {}",
                response.status.as_u16()
            )));
        }

        let resp_bytes = response
            .collect()
            .await
            .map_err(|e| map_upstream_err(e, "zai business login body"))?;

        let json: serde_json::Value = serde_json::from_slice(&resp_bytes)
            .map_err(|e| CoreError::Parse(format!("zai business login parse: {e}")))?;

        let tok = json
            .get("data")
            .and_then(|d| d.get("access_token").or_else(|| d.get("accessToken")))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| CoreError::Parse("missing business access_token".into()))?
            .trim()
            .to_string();

        Ok(tok)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_zai_build_auth_url() {
        let provider = ZaiOAuthProvider::new();
        assert_eq!(provider.name(), "zai");
        assert_eq!(provider.aliases(), &["zcode", "z.ai"]);
        assert_eq!(provider.flow(), OAuthFlow::AuthorizationCode);

        let (url, verifier, redirect, state) = provider
            .build_auth_url("https://zcode.z.ai/app/oauth/login?redirect=zcode%3A%2F%2Foauth%2Fcallback&app_version=3.14.0")
            .await
            .expect("build auth url");

        assert_eq!(verifier, state);
        assert!(!state.is_empty());
        assert_eq!(redirect, ZAI_REDIRECT_URI);
        assert!(url.starts_with(ZAI_DEFAULT_AUTHORIZE_URL));
        assert!(url.contains("client_id=client_P8X5CMWmlaRO9gyO-KSqtg"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains(&format!("state={state}")));
    }

    #[test]
    fn test_zai_metadata_and_email_extraction() {
        let provider = ZaiOAuthProvider::new();
        let meta = ZaiAccountMeta {
            zcode_jwt_token: Some("zcode.jwt.mock".into()),
            zai_access_token: Some("zai.at.mock".into()),
            business_access_token: Some("biz.mock".into()),
            api_key: Some("mock.api.key".into()),
            user_id: Some("u-12345".into()),
            name: Some("ZCodeUser".into()),
            email: Some("dev@example.com".into()),
            avatar: None,
        };

        let id_token = serde_json::to_string(&meta).unwrap();
        let token = TokenResponse {
            access_token: "zcode.jwt.mock".into(),
            token_type: "Bearer".into(),
            expires_in: Some(3600),
            refresh_token: None,
            scope: None,
            id_token: Some(id_token.clone()),
        };

        assert_eq!(
            provider.provider_specific_from_token(&token).as_deref(),
            Some(id_token.as_str())
        );
        assert_eq!(
            provider.email_from_token(&token).as_deref(),
            Some("dev@example.com")
        );
    }
}
