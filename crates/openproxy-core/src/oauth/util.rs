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

/// Extract a named parameter from a URL query string without external dependencies.
pub fn extract_query_param(query: &str, param: &str) -> Option<String> {
    for part in query.split('&') {
        if let Some((k, v)) = part.split_once('=')
            && k.trim() == param
        {
            return Some(v.trim().to_string());
        }
    }
    None
}

/// Robustly extracts the authorization `code` and optional `state` from user input across all OAuth providers.
/// Handles:
/// 1. Raw code: `"code-12345"`
/// 2. Fragment format: `"code-12345#state-67890"`
/// 3. Standard HTTP/HTTPS callback URL: `"http://127.0.0.1:8787/oauth/callback?code=abc&state=xyz"`
/// 4. Custom desktop protocol URL: `"zcode://oauth/callback?code=abc&state=xyz"`, `"cline://..."`, `"cursor://..."`
pub fn parse_oauth_callback_input(input: &str) -> (String, Option<String>) {
    let trimmed = input.trim();
    if let Some((_, query)) = trimmed.split_once('?') {
        let code_from_query = extract_query_param(query, "code")
            .or_else(|| extract_query_param(query, "apiKey"))
            .or_else(|| extract_query_param(query, "token"))
            .or_else(|| extract_query_param(query, "key"));
        let state_from_query = extract_query_param(query, "state");
        (
            code_from_query.unwrap_or_else(|| trimmed.to_string()),
            state_from_query,
        )
    } else if let Some((c, s)) = trimmed.split_once('#') {
        (c.trim().to_string(), Some(s.trim().to_string()))
    } else {
        (trimmed.to_string(), None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_query_param() {
        let q = "redirect=zcode%3A%2F%2Foauth%2Fcallback&app_version=3.14.0&code=code-123&state=state-456";
        assert_eq!(extract_query_param(q, "code").as_deref(), Some("code-123"));
        assert_eq!(
            extract_query_param(q, "state").as_deref(),
            Some("state-456")
        );
        assert_eq!(extract_query_param(q, "notfound"), None);
    }

    #[test]
    fn test_parse_oauth_callback_input_formats() {
        // 1. Raw code
        let (c1, s1) = parse_oauth_callback_input("code-raw-123");
        assert_eq!(c1, "code-raw-123");
        assert_eq!(s1, None);

        // 2. Hash fragment
        let (c2, s2) = parse_oauth_callback_input("code-frag#state-frag");
        assert_eq!(c2, "code-frag");
        assert_eq!(s2.as_deref(), Some("state-frag"));

        // 3. Full HTTP callback URL
        let (c3, s3) = parse_oauth_callback_input(
            "https://zcode.z.ai/app/oauth/login?redirect=zcode%3A%2F%2Foauth%2Fcallback&code=code-xyz&state=state-abc",
        );
        assert_eq!(c3, "code-xyz");
        assert_eq!(s3.as_deref(), Some("state-abc"));

        // 4. Custom desktop protocol URI
        let (c4, s4) =
            parse_oauth_callback_input("zcode://oauth/callback?code=code-proto&state=state-proto");
        assert_eq!(c4, "code-proto");
        assert_eq!(s4.as_deref(), Some("state-proto"));

        // 5. Alternate apiKey param
        let (c5, s5) = parse_oauth_callback_input("https://example.com/callback?apiKey=key-999");
        assert_eq!(c5, "key-999");
        assert_eq!(s5, None);
    }

    #[test]
    fn test_resolver_default_and_custom() {
        let resolver =
            OAuthEndpointResolver::new("NONEXISTENT_OAUTH_ENV_VAR", "https://api.example.com");
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
