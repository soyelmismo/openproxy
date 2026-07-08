//! Declarative OAuth provider support.
//!
//! `OAuthProvider` is still the public extension point. This module supplies a
//! reusable implementation for providers whose OAuth surfaces are mostly
//! standard: PKCE authorization-code flow, optional device-code flow, and token
//! refresh through a token endpoint.

use async_trait::async_trait;
use base64::Engine;
use rand::Rng;
use sha2::{Digest, Sha256};
use std::sync::Arc;

use crate::error::{CoreError, Result};
use crate::ids::AccountId;
use crate::oauth::{DbRef, DeviceAuthorizationResponse, OAuthFlow, OAuthProvider, TokenResponse};
use crate::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamError, UpstreamRequest,
};

/// How token/device requests are encoded on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthRequestEncoding {
    FormUrlEncoded,
    Json,
}

/// Static OAuth metadata for a provider.
#[derive(Debug, Clone)]
pub struct OAuthSpec {
    pub id: &'static str,
    pub flow: OAuthFlow,
    pub authorize_url: Option<&'static str>,
    pub token_url: &'static str,
    pub device_authorization_url: Option<&'static str>,
    pub client_id_env: Option<&'static str>,
    pub client_id_default: &'static str,
    pub client_secret_env: Option<&'static str>,
    pub client_secret_default: Option<&'static str>,
    pub scopes: &'static [&'static str],
    pub auth_extra_params: &'static [(&'static str, &'static str)],
    pub request_encoding: OAuthRequestEncoding,
    pub user_agent: Option<fn() -> String>,
}

impl OAuthSpec {
    fn client_id(&self) -> Result<String> {
        if let Some(env) = self.client_id_env
            && let Ok(value) = std::env::var(env)
            && !value.is_empty()
        {
            return Ok(value);
        }
        if !self.client_id_default.is_empty() {
            return Ok(self.client_id_default.to_string());
        }
        Err(CoreError::Validation(format!(
            "provider '{}' has no OAuth client_id; set {}",
            self.id,
            self.client_id_env.unwrap_or("<provider client_id env>")
        )))
    }

    fn client_secret(&self) -> Option<String> {
        if let Some(env) = self.client_secret_env
            && let Ok(value) = std::env::var(env)
            && !value.is_empty()
        {
            return Some(value);
        }
        self.client_secret_default
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
    }
}

/// Generic provider implementation backed by an [`OAuthSpec`].
#[derive(Clone)]
pub struct GenericOAuthProvider {
    spec: OAuthSpec,
}

impl GenericOAuthProvider {
    pub fn new(spec: OAuthSpec) -> Self {
        Self { spec }
    }

    pub fn spec(&self) -> &OAuthSpec {
        &self.spec
    }

    async fn token_request(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        purpose: &str,
        params: Vec<(&str, String)>,
    ) -> Result<TokenResponse> {
        let body = match self.spec.request_encoding {
            OAuthRequestEncoding::FormUrlEncoded => urlencoded_body_owned(&params),
            OAuthRequestEncoding::Json => json_body(&params)?,
        };

        let mut req = UpstreamRequest::post_json(self.spec.token_url, body);
        match self.spec.request_encoding {
            OAuthRequestEncoding::FormUrlEncoded => {
                req.headers.insert(
                    http::header::CONTENT_TYPE,
                    http::HeaderValue::from_static("application/x-www-form-urlencoded"),
                );
            }
            OAuthRequestEncoding::Json => {
                req.headers.insert(
                    http::header::CONTENT_TYPE,
                    http::HeaderValue::from_static("application/json"),
                );
            }
        }
        if let Some(user_agent) = self.spec.user_agent
            && let Ok(value) = http::HeaderValue::from_str(&user_agent())
        {
            req.headers.insert(http::header::USER_AGENT, value);
        }

        let response = call_oauth_endpoint(upstream_client, &self.spec, req, purpose).await?;
        serde_json::from_slice::<TokenResponse>(&response)
            .map_err(|e| CoreError::Parse(format!("{} token response parse: {e}", self.spec.id)))
    }
}

impl OAuthProvider for GenericOAuthProvider {
    fn name(&self) -> &str {
        self.spec.id
    }

    fn flow(&self) -> OAuthFlow {
        self.spec.flow
    }

    async fn build_auth_url(&self, redirect_uri: &str) -> Result<(String, String, String)> {
        let authorize_url = self.spec.authorize_url.ok_or_else(|| {
            CoreError::Validation(format!(
                "provider '{}' does not support authorization URL",
                self.spec.id
            ))
        })?;
        if self.spec.flow != OAuthFlow::AuthorizationCodePkce
            && self.spec.flow != OAuthFlow::AuthorizationCode
        {
            return Err(CoreError::Validation(format!(
                "provider '{}' does not support authorization code flow",
                self.spec.id
            )));
        }

        let code_verifier = if self.spec.flow == OAuthFlow::AuthorizationCodePkce {
            generate_code_verifier()
        } else {
            String::new()
        };
        let code_challenge = if code_verifier.is_empty() {
            String::new()
        } else {
            code_challenge_s256(&code_verifier)
        };
        let client_id = self.spec.client_id()?;

        let mut params = vec![
            ("response_type", "code".to_string()),
            ("client_id", client_id),
            ("redirect_uri", redirect_uri.to_string()),
        ];
        if !self.spec.scopes.is_empty() {
            params.push(("scope", self.spec.scopes.join(" ")));
        }
        if !code_challenge.is_empty() {
            params.push(("code_challenge", code_challenge.clone()));
            params.push(("code_challenge_method", "S256".to_string()));
        }
        for (key, value) in self.spec.auth_extra_params {
            params.push((*key, (*value).to_string()));
        }

        Ok((
            format!("{authorize_url}?{}", urlencoded_string(&params)),
            code_verifier,
            code_challenge,
        ))
    }

    async fn exchange_code(
        &self,
        code: &str,
        code_verifier: &str,
        upstream_client: &Arc<UpstreamClient>,
        redirect_uri: &str,
    ) -> Result<TokenResponse> {
        let client_id = self.spec.client_id()?;
        let mut params = vec![
            ("grant_type", "authorization_code".to_string()),
            ("code", code.to_string()),
            ("client_id", client_id),
            ("redirect_uri", redirect_uri.to_string()),
        ];
        if !code_verifier.is_empty() {
            params.push(("code_verifier", code_verifier.to_string()));
        }
        if let Some(secret) = self.spec.client_secret() {
            params.push(("client_secret", secret));
        }
        self.token_request(upstream_client, "token exchange", params)
            .await
    }

    async fn request_device_code(
        &self,
        upstream_client: &Arc<UpstreamClient>,
    ) -> Result<DeviceAuthorizationResponse> {
        let url = self.spec.device_authorization_url.ok_or_else(|| {
            CoreError::Validation(format!(
                "provider '{}' does not support device code flow",
                self.spec.id
            ))
        })?;
        if self.spec.flow != OAuthFlow::DeviceCode {
            return Err(CoreError::Validation(format!(
                "provider '{}' does not support device code flow",
                self.spec.id
            )));
        }

        let mut params = vec![("client_id", self.spec.client_id()?)];
        if !self.spec.scopes.is_empty() {
            params.push(("scope", self.spec.scopes.join(" ")));
        }
        if let Some(secret) = self.spec.client_secret() {
            params.push(("client_secret", secret));
        }

        let body = match self.spec.request_encoding {
            OAuthRequestEncoding::FormUrlEncoded => urlencoded_body_owned(&params),
            OAuthRequestEncoding::Json => json_body(&params)?,
        };
        let mut req = UpstreamRequest::post_json(url, body);
        if let OAuthRequestEncoding::FormUrlEncoded = self.spec.request_encoding {
            req.headers.insert(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static("application/x-www-form-urlencoded"),
            );
        }
        let body =
            call_oauth_endpoint(upstream_client, &self.spec, req, "device authorization").await?;
        serde_json::from_slice::<DeviceAuthorizationResponse>(&body)
            .map_err(|e| CoreError::Parse(format!("{} device response parse: {e}", self.spec.id)))
    }

    async fn poll_device_token(
        &self,
        device_code: &str,
        upstream_client: &Arc<UpstreamClient>,
    ) -> Result<Option<TokenResponse>> {
        if self.spec.flow != OAuthFlow::DeviceCode {
            return Err(CoreError::Validation(format!(
                "provider '{}' does not support device token polling",
                self.spec.id
            )));
        }
        let mut params = vec![
            (
                "grant_type",
                "urn:ietf:params:oauth:grant-type:device_code".to_string(),
            ),
            ("device_code", device_code.to_string()),
            ("client_id", self.spec.client_id()?),
        ];
        if let Some(secret) = self.spec.client_secret() {
            params.push(("client_secret", secret));
        }

        match self
            .token_request(upstream_client, "device token poll", params)
            .await
        {
            Ok(token) => Ok(Some(token)),
            Err(CoreError::UpstreamError { status, .. }) if status == 400 || status == 428 => {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    async fn refresh_token(
        &self,
        refresh_token: &str,
        upstream_client: &Arc<UpstreamClient>,
        _account_id: AccountId,
        _db: DbRef<'_>,
    ) -> Result<TokenResponse> {
        let mut params = vec![
            ("grant_type", "refresh_token".to_string()),
            ("client_id", self.spec.client_id()?),
            ("refresh_token", refresh_token.to_string()),
        ];
        if let Some(secret) = self.spec.client_secret() {
            params.push(("client_secret", secret));
        }
        self.token_request(upstream_client, "token refresh", params)
            .await
    }
}

async fn call_oauth_endpoint(
    upstream_client: &Arc<UpstreamClient>,
    spec: &OAuthSpec,
    req: UpstreamRequest,
    purpose: &str,
) -> Result<bytes::Bytes> {
    let cancel = CancellationToken::new();
    let response = upstream_client
        .call(req, TimeoutProfile::OAuth, cancel)
        .await
        .map_err(|e| match e {
            UpstreamError::Cancel => CoreError::ClientDisconnected,
            other => CoreError::UpstreamConnection(format!("{} {purpose}: {other}", spec.id)),
        })?;
    let status = response.status;
    let body = response.collect().await.map_err(|e| match e {
        UpstreamError::Cancel => CoreError::ClientDisconnected,
        other => CoreError::UpstreamConnection(format!("{} {purpose} body read: {other}", spec.id)),
    })?;

    if !status.is_success() {
        return Err(CoreError::UpstreamError {
            status: status.as_u16(),
            provider: spec.id.into(),
            model: "<oauth>".into(),
            body: String::from_utf8_lossy(&body).to_string(),
        });
    }
    Ok(body)
}

/// Generate a cryptographically random PKCE code verifier (43-128 chars).
pub fn generate_code_verifier() -> String {
    let mut buf = [0u8; 32];
    rand::rng().fill_bytes(&mut buf);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

/// Compute the S256 PKCE code challenge from a verifier.
pub fn code_challenge_s256(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

pub fn urlencoded_body(params: &[(&str, &str)]) -> bytes::Bytes {
    let owned: Vec<(&str, String)> = params
        .iter()
        .map(|(key, value)| (*key, (*value).to_string()))
        .collect();
    urlencoded_body_owned(&owned)
}

fn urlencoded_body_owned(params: &[(&str, String)]) -> bytes::Bytes {
    bytes::Bytes::from(urlencoded_string(params))
}

fn urlencoded_string(params: &[(&str, String)]) -> String {
    let mut s = String::new();
    for (i, (k, v)) in params.iter().enumerate() {
        if i > 0 {
            s.push('&');
        }
        s.push_str(&urlencoding::encode(k));
        s.push('=');
        s.push_str(&urlencoding::encode(v));
    }
    s
}

fn json_body(params: &[(&str, String)]) -> Result<bytes::Bytes> {
    let mut obj = serde_json::Map::new();
    for (key, value) in params {
        obj.insert((*key).to_string(), serde_json::Value::String(value.clone()));
    }
    serde_json::to_vec(&serde_json::Value::Object(obj))
        .map(bytes::Bytes::from)
        .map_err(|e| CoreError::Parse(format!("oauth json body serialize: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_verifier_is_url_safe() {
        let v = generate_code_verifier();
        assert!(v.len() >= 43);
        assert!(v.len() <= 128);
        assert!(
            v.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    #[test]
    fn code_challenge_deterministic() {
        let verifier = "test-verifier-string";
        assert_eq!(code_challenge_s256(verifier), code_challenge_s256(verifier));
    }

    #[test]
    fn urlencoded_body_escapes_values() {
        let body = urlencoded_body(&[("scope", "openid email"), ("redirect_uri", "http://x/cb")]);
        assert_eq!(
            std::str::from_utf8(&body).unwrap(),
            "scope=openid%20email&redirect_uri=http%3A%2F%2Fx%2Fcb"
        );
    }

    #[tokio::test]
    async fn generic_pkce_authorize_url_includes_extra_params() {
        let p = GenericOAuthProvider::new(OAuthSpec {
            id: "example",
            flow: OAuthFlow::AuthorizationCodePkce,
            authorize_url: Some("https://auth.example/authorize"),
            token_url: "https://auth.example/token",
            device_authorization_url: None,
            client_id_env: None,
            client_id_default: "client-1",
            client_secret_env: None,
            client_secret_default: None,
            scopes: &["openid", "email"],
            auth_extra_params: &[("prompt", "login")],
            request_encoding: OAuthRequestEncoding::FormUrlEncoded,
            user_agent: None,
        });

        let (url, verifier, challenge) = p.build_auth_url("http://localhost/cb").await.unwrap();
        assert!(!verifier.is_empty());
        assert_eq!(challenge, code_challenge_s256(&verifier));
        assert!(url.starts_with("https://auth.example/authorize?"));
        assert!(url.contains("client_id=client-1"));
        assert!(url.contains("scope=openid%20email"));
        assert!(url.contains("prompt=login"));
        assert!(url.contains("code_challenge_method=S256"));
    }
}
