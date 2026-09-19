//! Shared OAuth utilities and abstractions.

use crate::error::{CoreError, Result};
use openproxy_types::oauth::TokenResponse;

/// Resolves OAuth base URLs and endpoint paths dynamically with support for
/// environment variable overrides and test mocks.
#[derive(Clone, Debug)]
pub struct OAuthEndpointResolver {
    env_var: &'static str,
    default_base: &'static str,
    custom_base: Option<String>,
}

impl OAuthEndpointResolver {
    pub const fn new(env_var: &'static str, default_base: &'static str) -> Self {
        Self {
            env_var,
            default_base,
            custom_base: None,
        }
    }

    pub fn with_custom_base(mut self, base: impl Into<String>) -> Self {
        self.custom_base = Some(base.into());
        self
    }

    pub fn has_custom_base(&self) -> bool {
        self.custom_base.is_some()
    }

    pub fn base_url(&self) -> String {
        if let Some(ref base) = self.custom_base {
            return base.clone();
        }
        std::env::var(self.env_var).unwrap_or_else(|_| self.default_base.to_string())
    }

    pub fn url_with_path(&self, path: &str) -> String {
        let base = self.base_url();
        let base = base.trim_end_matches('/');
        let path = path.trim_start_matches('/');
        format!("{base}/{path}")
    }
}

/// Verifies HTTP status code for an OAuth response, returning a typed `CoreError` on failure.
pub fn check_oauth_status(status: http::StatusCode, provider: &str, body: &[u8]) -> Result<()> {
    if !status.is_success() {
        return Err(CoreError::upstream_error(
            status.as_u16(),
            provider,
            "<oauth>",
            String::from_utf8_lossy(body).to_string(),
            false,
        ));
    }
    Ok(())
}

/// Robustly extracts the user's email or display name from an OAuth `TokenResponse`.
/// Inspects `id_token` first, then falls back to decoding the `access_token` JWT payload
/// (stripping `Bearer ` or `workos:` prefixes if present).
pub fn extract_email_from_token(token: &TokenResponse) -> Option<String> {
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
        && let Some(claims) = super::decode_jwt_payload(id_jwt)
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
    let claims = super::decode_jwt_payload(jwt)?;
    extract(&claims)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolver_default_and_custom() {
        let resolver = OAuthEndpointResolver::new("NONEXISTENT_OAUTH_ENV_VAR", "https://api.example.com");
        assert_eq!(resolver.base_url(), "https://api.example.com");
        assert_eq!(
            resolver.url_with_path("/v1/token"),
            "https://api.example.com/v1/token"
        );
        assert!(!resolver.has_custom_base());

        let custom = resolver.with_custom_base("http://127.0.0.1:9000/");
        assert!(custom.has_custom_base());
        assert_eq!(custom.base_url(), "http://127.0.0.1:9000/");
        assert_eq!(
            custom.url_with_path("v1/token"),
            "http://127.0.0.1:9000/v1/token"
        );
    }

    #[test]
    fn test_check_oauth_status_ok_and_err() {
        assert!(check_oauth_status(http::StatusCode::OK, "test", b"ok").is_ok());
        let err = check_oauth_status(http::StatusCode::BAD_REQUEST, "test", b"fail").unwrap_err();
        assert!(err.to_string().contains("400"));
    }
}
