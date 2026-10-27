//! Nous Portal OAuth (device-code flow).
//!
//! Mirrors the device-code contract the Nous Portal exposes and that the
//! Hermes CLI already speaks:
//!
//!   POST {portal}/api/oauth/device/code   -> device_code, user_code, verification_uri
//!   POST {portal}/api/oauth/token          -> access_token (RFC 8628 grant)
//!   POST {portal}/api/oauth/token          -> refresh (grant_type=refresh_token)
//!
//! The Portal identifies the caller from the JWT it issues. The adapter reads
//! the `sub` claim out of that token to build the mandatory `user=` request tag,
//! so an OAuth account is attributed correctly and each account in a
//! multi-account pool reports its own identity. An API-key account carries no
//! such identity, which is exactly why the Portal rejects bare `sk-nous-...`
//! keys with 400 "missing tags".
//!
//! Endpoint resolution follows the Codex provider: a resolver with an
//! operator-overridable base URL, so a self-hosted or staging Portal can be
//! pointed at without recompiling.

use crate::oauth::generic::{
    GenericOAuthProvider, OAuthRequestEncoding, OAuthSpec, urlencoded_string,
};
use crate::oauth::{DbRef, DeviceAuthorizationResponse, OAuthFlow, OAuthProvider, TokenResponse};
use crate::{error::Result, ids::AccountId};
use openproxy_adapters::adapters::nous_research::jwt_email;
use openproxy_adapters::upstream::UpstreamClient;
use std::sync::Arc;

pub const NOUS_OAUTH_PROVIDER: &str = "nous-research";
pub const NOUS_DEFAULT_PORTAL_URL: &str = "https://portal.nousresearch.com";
pub const NOUS_DEVICE_CODE_PATH: &str = "/api/oauth/device/code";
pub const NOUS_TOKEN_PATH: &str = "/api/oauth/token";
/// The Portal's public device client. Override with `NOUS_OAUTH_CLIENT_ID`.
pub const NOUS_DEFAULT_CLIENT_ID: &str = "hermes-cli";
/// Minimum scope to invoke inference.
pub const NOUS_INFERENCE_INVOKE_SCOPE: &str = "inference:invoke";

/// Declarative spec for the generic provider. `FormUrlEncoded` matches the
/// Portal (and the Hermes CLI), which expect urlencoded bodies.
fn nous_oauth_spec() -> OAuthSpec {
    OAuthSpec {
        id: NOUS_OAUTH_PROVIDER,
        flow: OAuthFlow::DeviceCode,
        authorize_url: None,
        token_url: NOUS_TOKEN_PATH,
        device_authorization_url: Some(NOUS_DEVICE_CODE_PATH),
        client_id_env: Some("NOUS_OAUTH_CLIENT_ID"),
        client_id_default: NOUS_DEFAULT_CLIENT_ID,
        client_secret_env: None,
        client_secret_default: None,
        scopes: &[NOUS_INFERENCE_INVOKE_SCOPE],
        auth_extra_params: &[],
        request_encoding: OAuthRequestEncoding::FormUrlEncoded,
        user_agent: None,
    }
}

#[derive(Clone)]
pub struct NousOAuthProvider {
    generic: GenericOAuthProvider,
    resolver: super::OAuthEndpointResolver,
}

impl NousOAuthProvider {
    pub fn new() -> Self {
        Self {
            generic: GenericOAuthProvider::new(nous_oauth_spec()),
            resolver: super::OAuthEndpointResolver::new(
                "OPENPROXY_NOUS_PORTAL_BASE_URL",
                NOUS_DEFAULT_PORTAL_URL,
            ),
        }
    }

    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            generic: GenericOAuthProvider::new(nous_oauth_spec()),
            resolver: super::OAuthEndpointResolver::new(
                "OPENPROXY_NOUS_PORTAL_BASE_URL",
                NOUS_DEFAULT_PORTAL_URL,
            )
            .with_custom_base(base_url),
        }
    }

    pub fn portal_base_url(&self) -> String {
        self.resolver.base_url()
    }

    pub fn device_code_url(&self) -> String {
        self.resolver.url_with_path(NOUS_DEVICE_CODE_PATH)
    }

    pub fn token_url(&self) -> String {
        self.resolver.url_with_path(NOUS_TOKEN_PATH)
    }
}

impl Default for NousOAuthProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl OAuthProvider for NousOAuthProvider {
    fn name(&self) -> &str {
        NOUS_OAUTH_PROVIDER
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["nous"]
    }

    fn flow(&self) -> OAuthFlow {
        OAuthFlow::DeviceCode
    }

    /// Nous uses device code, not the PKCE authorization-code flow.
    fn exchange_code(
        &self,
        _code: &str,
        _code_verifier: &str,
        _upstream_client: &Arc<UpstreamClient>,
        _redirect_uri: &str,
    ) -> impl std::future::Future<Output = Result<TokenResponse>> + Send {
        std::future::ready(Err(crate::error::CoreError::Validation(
            "nous uses device code flow, not authorization code".into(),
        )))
    }

    /// The Portal expects the absolute device-code URL. The generic provider
    /// posts to `resolved_device_auth_url()`, which is the bare path from the
    /// spec, so we resolve the full URL here before delegating.
    fn request_device_code(
        &self,
        upstream_client: &Arc<UpstreamClient>,
    ) -> impl std::future::Future<Output = Result<DeviceAuthorizationResponse>> + Send {
        let url = self.device_code_url();
        let client_id = self.generic.spec().client_id_default.to_string();
        async move {
            nous_post_form(upstream_client, &url, &[("client_id", client_id.as_str())])
                .await
                .and_then(|body| {
                    serde_json::from_slice::<DeviceAuthorizationResponse>(&body)
                        .map_err(|e| crate::error::CoreError::Parse(e.to_string()))
                })
        }
    }

    /// Polls the token endpoint. `Ok(None)` means still pending.
    fn poll_device_token(
        &self,
        device_code: &str,
        upstream_client: &Arc<UpstreamClient>,
    ) -> impl std::future::Future<Output = Result<Option<TokenResponse>>> + Send {
        let url = self.token_url();
        let client_id = self.generic.spec().client_id_default.to_string();
        let device_code = device_code.to_string();
        async move {
            let body = nous_post_form(
                upstream_client,
                &url,
                &[
                    ("grant_type", NOUS_DEVICE_CODE_GRANT_TYPE),
                    ("device_code", device_code.as_str()),
                    ("client_id", client_id.as_str()),
                ],
            )
            .await?;
            serde_json::from_slice::<TokenResponse>(&body)
                .map(Some)
                .map_err(|e| crate::error::CoreError::Parse(e.to_string()))
        }
    }

    fn refresh_token(
        &self,
        refresh_token: &str,
        upstream_client: &Arc<UpstreamClient>,
        _account_id: AccountId,
        _db: DbRef<'_>,
    ) -> impl std::future::Future<Output = Result<TokenResponse>> + Send {
        let url = self.token_url();
        let client_id = self.generic.spec().client_id_default.to_string();
        let refresh_token = refresh_token.to_string();
        async move {
            let req = nous_refresh_request(&url, &client_id, &refresh_token);
            let body = crate::oauth::call_oauth_checked(
                upstream_client,
                req,
                NOUS_OAUTH_PROVIDER,
                "nous oauth",
            )
            .await?;
            serde_json::from_slice::<TokenResponse>(&body)
                .map_err(|e| crate::error::CoreError::Parse(e.to_string()))
        }
    }

    /// Label OAuth accounts with the Portal email when the token carries one.
    fn email_from_token(&self, token: &TokenResponse) -> Option<String> {
        jwt_email(&token.access_token).or_else(|| token.id_token.as_deref().and_then(jwt_email))
    }
}

/// RFC 8628 device-code grant type, as the Portal expects it.
const NOUS_DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// POSTs a urlencoded form to the Portal and returns the raw response body,
/// mapping a non-2xx status into an upstream error.
async fn nous_post_form(
    upstream_client: &Arc<UpstreamClient>,
    url: &str,
    params: &[(&str, &str)],
) -> Result<bytes::Bytes> {
    let req = nous_form_request(url, params);
    crate::oauth::call_oauth_checked(upstream_client, req, NOUS_OAUTH_PROVIDER, "nous oauth").await
}

/// Builds a urlencoded POST to the Portal.
fn nous_form_request(
    url: &str,
    params: &[(&str, &str)],
) -> openproxy_adapters::upstream::UpstreamRequest {
    let encoded = urlencoded_string(params);
    let mut req = openproxy_adapters::upstream::UpstreamRequest::post_json(
        url.to_string(),
        bytes::Bytes::from(encoded),
    );
    req.headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/x-www-form-urlencoded"),
    );
    req
}

/// Header the Portal reads the refresh token from.
const NOUS_REFRESH_TOKEN_HEADER: &str = "x-nous-refresh-token";

/// Builds the refresh-token exchange request.
///
/// The refresh token travels in the `x-nous-refresh-token` header, which is the
/// channel the Portal's own clients use; the RFC 6749 `refresh_token` form field
/// stays populated as well. The distinction matters because these refresh tokens
/// are single-use: a request the Portal cannot read means the grant was never
/// redeemed, and a retry against an already-rotated chain kills the session.
fn nous_refresh_request(
    url: &str,
    client_id: &str,
    refresh_token: &str,
) -> openproxy_adapters::upstream::UpstreamRequest {
    let mut req = nous_form_request(
        url,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("refresh_token", refresh_token),
        ],
    );
    if let Ok(value) = http::HeaderValue::from_str(refresh_token) {
        req.headers.insert(
            http::HeaderName::from_static(NOUS_REFRESH_TOKEN_HEADER),
            value,
        );
    }
    req
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_request_carries_the_token_in_the_portal_header() {
        let req = nous_refresh_request(
            "https://portal.nousresearch.com/api/oauth/token",
            "hermes-cli",
            "rt_single_use",
        );
        assert_eq!(
            req.headers
                .get("x-nous-refresh-token")
                .and_then(|v| v.to_str().ok()),
            Some("rt_single_use"),
            "the Portal reads the grant from this header"
        );
        let body = req
            .body
            .as_deref()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_default();
        assert!(body.contains("grant_type=refresh_token"), "body: {body}");
        assert!(body.contains("client_id=hermes-cli"), "body: {body}");
        assert!(body.contains("refresh_token=rt_single_use"), "body: {body}");
    }

    #[test]
    fn provider_identity_and_flow() {
        let p = NousOAuthProvider::new();
        assert_eq!(p.name(), "nous-research");
        assert_eq!(p.aliases(), &["nous"]);
        assert_eq!(p.flow(), OAuthFlow::DeviceCode);
    }

    #[test]
    fn endpoints_resolve_against_the_portal_base_url() {
        let p = NousOAuthProvider::new();
        assert_eq!(p.portal_base_url(), NOUS_DEFAULT_PORTAL_URL);
        assert_eq!(
            p.device_code_url(),
            "https://portal.nousresearch.com/api/oauth/device/code"
        );
        assert_eq!(
            p.token_url(),
            "https://portal.nousresearch.com/api/oauth/token"
        );
    }

    #[test]
    fn spec_declares_the_portal_contract() {
        let spec = nous_oauth_spec();
        assert_eq!(spec.id, NOUS_OAUTH_PROVIDER);
        assert_eq!(spec.client_id_default, NOUS_DEFAULT_CLIENT_ID);
        assert_eq!(spec.scopes, &[NOUS_INFERENCE_INVOKE_SCOPE]);
        assert_eq!(spec.device_authorization_url, Some(NOUS_DEVICE_CODE_PATH));
        assert!(matches!(
            spec.request_encoding,
            OAuthRequestEncoding::FormUrlEncoded
        ));
    }
}
