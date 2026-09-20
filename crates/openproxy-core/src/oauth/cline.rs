//! Cline OAuth provider.

use crate::error::{CoreError, Result};
use crate::oauth::{
    DbRef, DeviceAuthorizationResponse, OAuthFlow, OAuthProvider, TokenResponse, map_upstream_err,
};
use openproxy_adapters::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest,
};
use std::sync::Arc;

pub const CLINE_DEFAULT_BASE_URL: &str = "https://api.cline.bot";
pub const CLINE_AUTH_AUTHORIZE_PATH: &str = "/api/v1/auth/authorize";
pub const CLINE_AUTH_TOKEN_PATH: &str = "/api/v1/auth/token";
pub const CLINE_AUTH_REFRESH_PATH: &str = "/api/v1/auth/refresh";
pub const CLINE_CLIENT_TYPE: &str = "extension";
pub const CLINE_PROVIDER: &str = "cline";

#[derive(Clone)]
pub struct ClineOAuthProvider {
    resolver: super::OAuthEndpointResolver,
}

impl ClineOAuthProvider {
    pub fn new() -> Self {
        Self {
            resolver: super::OAuthEndpointResolver::new(
                "OPENPROXY_CLINE_OAUTH_BASE_URL",
                CLINE_DEFAULT_BASE_URL,
            ),
        }
    }

    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            resolver: super::OAuthEndpointResolver::new(
                "OPENPROXY_CLINE_OAUTH_BASE_URL",
                CLINE_DEFAULT_BASE_URL,
            )
            .with_custom_base(base_url),
        }
    }

    pub fn base_url(&self) -> String {
        self.resolver.base_url()
    }
}

impl Default for ClineOAuthProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl OAuthProvider for ClineOAuthProvider {
    fn name(&self) -> &'static str {
        "cline"
    }

    fn flow(&self) -> OAuthFlow {
        OAuthFlow::AuthorizationCode
    }

    fn build_auth_url(
        &self,
        redirect_uri: &str,
    ) -> impl std::future::Future<Output = Result<(String, String, String, String)>> + Send {
        let authorize_url = self.resolver.url_with_path(CLINE_AUTH_AUTHORIZE_PATH);

        let state = uuid::Uuid::new_v4().to_string();

        let params = vec![
            ("client_type", CLINE_CLIENT_TYPE),
            ("callback_url", redirect_uri),
            ("redirect_uri", redirect_uri),
            ("state", state.as_str()),
        ];

        let url = format!(
            "{authorize_url}?{}",
            crate::oauth::generic::urlencoded_string(&params)
        );

        std::future::ready(Ok((url, String::new(), String::new(), state)))
    }

    async fn exchange_code(
        &self,
        code: &str,
        _code_verifier: &str,
        upstream_client: &Arc<UpstreamClient>,
        redirect_uri: &str,
    ) -> Result<TokenResponse> {
        let (actual_code, _) = crate::oauth::util::parse_oauth_callback_input(code);
        let body = serde_json::json!({
            "grant_type": "authorization_code",
            "code": actual_code,
            "client_type": "extension",
            "redirect_uri": redirect_uri,
            "provider": "cline"
        });

        let body_bytes =
            serde_json::to_vec(&body).map_err(|e| CoreError::Validation(e.to_string()))?;
        let token_url = self.resolver.url_with_path(CLINE_AUTH_TOKEN_PATH);
        let mut req = UpstreamRequest::post_json(token_url, bytes::Bytes::from(body_bytes));
        req.headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        openproxy_adapters::adapters::cline::apply_cline_spoofing_headers(&mut req);

        let cancel = CancellationToken::new();
        let response = upstream_client
            .call(req, TimeoutProfile::OAuth, cancel)
            .await
            .map_err(|e| map_upstream_err(e, "cline exchange"))?;

        let status = response.status;
        let resp_body = response
            .collect()
            .await
            .map_err(|e| map_upstream_err(e, "cline exchange body"))?;

        super::check_oauth_status(status, "cline", &resp_body)?;

        let resp: ClineResponse = serde_json::from_slice(&resp_body)
            .map_err(|e| CoreError::Parse(format!("cline token parse: {e}")))?;

        if !resp.success {
            return Err(CoreError::Validation("Cline returned success=false".into()));
        }

        let expires_in = parse_expires_in(&resp.data);
        Ok(TokenResponse {
            access_token: resp.data.access_token,
            refresh_token: resp.data.refresh_token,
            token_type: "Bearer".to_string(),
            expires_in,
            scope: None,
            id_token: None,
        })
    }

    fn request_device_code(
        &self,
        _upstream_client: &Arc<UpstreamClient>,
    ) -> impl std::future::Future<Output = Result<DeviceAuthorizationResponse>> + Send {
        std::future::ready(Err(CoreError::Validation(
            "cline uses auth code flow, not device code".into(),
        )))
    }

    fn poll_device_token(
        &self,
        _device_code: &str,
        _upstream_client: &Arc<UpstreamClient>,
    ) -> impl std::future::Future<Output = Result<Option<TokenResponse>>> + Send {
        std::future::ready(Err(CoreError::Validation(
            "cline uses auth code flow, not device code".into(),
        )))
    }

    async fn refresh_token(
        &self,
        refresh_token: &str,
        upstream_client: &Arc<UpstreamClient>,
        _account_id: crate::ids::AccountId,
        _db: DbRef<'_>,
    ) -> Result<TokenResponse> {
        let refresh_url = self.resolver.url_with_path(CLINE_AUTH_REFRESH_PATH);

        let body = serde_json::json!({
            "granttype": "refresh_token",
            "refreshToken": refresh_token,
        });

        let body_bytes = serde_json::to_vec(&body)
            .map_err(|e| CoreError::Parse(format!("serialize cline refresh body: {e}")))?;

        let mut req = UpstreamRequest::post_json(refresh_url, bytes::Bytes::from(body_bytes));
        req.headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        req.headers.insert(
            http::header::ACCEPT,
            http::HeaderValue::from_static("application/json"),
        );
        openproxy_adapters::adapters::cline::apply_cline_spoofing_headers(&mut req);

        let cancel = CancellationToken::new();
        let response = upstream_client
            .call(req, TimeoutProfile::OAuth, cancel)
            .await
            .map_err(|e| map_upstream_err(e, "cline refresh"))?;

        let status = response.status;
        let resp_body = response
            .collect()
            .await
            .map_err(|e| map_upstream_err(e, "cline refresh body"))?;

        super::check_oauth_status(status, "cline", &resp_body)?;

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
        super::extract_email_from_token(token)
    }
}

#[derive(serde::Deserialize)]
struct ClineResponse {
    success: bool,
    data: ClineAuthResponseData,
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
        && let Ok(dt) = openproxy_types::timestamp::parse_timestamp(ts)
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
    use base64::Engine;

    #[test]
    fn test_cline_auth_response_data_deserialization_and_ttl() {
        let json_data = serde_json::json!({
            "accessToken": "test_acc",
            "refreshToken": "test_ref",
            "expiresAt": (chrono::Utc::now() + chrono::Duration::seconds(3600)).to_rfc3339()
        });
        let data: ClineAuthResponseData =
            <ClineAuthResponseData as serde::Deserialize>::deserialize(&json_data)
                .expect("deserialize valid json");
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
            <ClineAuthResponseData as serde::Deserialize>::deserialize(&snake_json)
                .expect("deserialize snake case");
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

    #[tokio::test]
    async fn test_cline_provider_metadata() {
        let provider = ClineOAuthProvider::new();
        assert_eq!(provider.name(), "cline");
        assert_eq!(provider.flow(), OAuthFlow::AuthorizationCode);
        assert!(provider.aliases().is_empty());
        assert_eq!(provider.base_url(), CLINE_DEFAULT_BASE_URL);
    }

    #[tokio::test]
    async fn test_cline_build_auth_url() {
        let provider = ClineOAuthProvider::new();
        let redirect_uri = "http://127.0.0.1:4000/oauth/callback";
        let (url, verifier, challenge, state) = provider
            .build_auth_url(redirect_uri)
            .await
            .expect("build auth url");

        assert!(verifier.is_empty(), "Cline does not use PKCE verifier");
        assert!(challenge.is_empty(), "Cline does not use PKCE challenge");
        assert!(
            uuid::Uuid::parse_str(&state).is_ok(),
            "state must be a valid UUID v4"
        );
        assert!(
            url.starts_with("https://api.cline.bot/api/v1/auth/authorize?"),
            "URL must target Cline authorize endpoint: {url}"
        );
        assert!(url.contains("client_type=extension"));
        assert!(url.contains("callback_url=http%3A%2F%2F127.0.0.1%3A4000%2Foauth%2Fcallback"));
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A4000%2Foauth%2Fcallback"));
        assert!(url.contains(&format!("state={state}")));
    }

    #[tokio::test]
    async fn test_cline_build_auth_url_custom_base() {
        let provider = ClineOAuthProvider::with_base_url("http://localhost:9999");
        assert_eq!(provider.base_url(), "http://localhost:9999");
        let (url, _, _, _) = provider
            .build_auth_url("http://loc/cb")
            .await
            .expect("build custom base auth url");
        assert!(url.starts_with("http://localhost:9999/api/v1/auth/authorize?"));
    }

    #[tokio::test]
    async fn test_cline_device_flow_unsupported() {
        let provider = ClineOAuthProvider::new();
        let client = Arc::new(UpstreamClient::new());
        let dev_res = provider.request_device_code(&client).await;
        assert!(dev_res.is_err());

        let poll_res = provider.poll_device_token("dev-code", &client).await;
        assert!(poll_res.is_err());
    }

    #[test]
    fn test_cline_email_fallback_to_name() {
        let provider = ClineOAuthProvider::new();
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"alg":"HS256"}"#);
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"name":"Developer Bob"}"#);
        let token = TokenResponse {
            access_token: format!("{header}.{payload}.sig"),
            token_type: "Bearer".into(),
            expires_in: None,
            refresh_token: None,
            scope: None,
            id_token: None,
        };
        assert_eq!(
            provider.email_from_token(&token).as_deref(),
            Some("Developer Bob")
        );
    }

    #[test]
    fn test_cline_email_from_id_token() {
        let provider = ClineOAuthProvider::new();
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"alg":"HS256"}"#);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"email":"id_user@example.com"}"#);
        let token = TokenResponse {
            access_token: "non_jwt_access_token".into(),
            token_type: "Bearer".into(),
            expires_in: None,
            refresh_token: None,
            scope: None,
            id_token: Some(format!("{header}.{payload}.sig")),
        };
        assert_eq!(
            provider.email_from_token(&token).as_deref(),
            Some("id_user@example.com")
        );
    }

    #[test]
    fn test_cline_email_invalid_tokens() {
        let provider = ClineOAuthProvider::new();
        let token = TokenResponse {
            access_token: "invalid-token".into(),
            token_type: "Bearer".into(),
            expires_in: None,
            refresh_token: None,
            scope: None,
            id_token: None,
        };
        assert_eq!(provider.email_from_token(&token), None);
    }
}
