//! Unit tests for the zAI OAuth provider.

use super::*;
use crate::oauth::OAuthFlow;

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
        autoclaw_access_token: None,
        autoclaw_refresh_token: None,
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
#[test]
fn test_zai_email_from_access_token_jti_fallback() {
    use base64::Engine;

    let provider = ZaiOAuthProvider::new();
    let jwt = |jti: &str| {
        let b64 = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        format!(
            "{}.{}.{}",
            b64(br#"{"alg":"HS256"}"#),
            b64(format!(r#"{{"user_id":189807,"jti":"{jti}"}}"#).as_bytes()),
            "sig",
        )
    };

    // AutoClaw carries the account's address in `jti`.
    let token = TokenResponse {
        access_token: jwt("t2297973@gmail.com"),
        token_type: "bearer".into(),
        expires_in: Some(86400),
        refresh_token: None,
        scope: None,
        id_token: None,
    };
    assert_eq!(
        provider.email_from_token(&token).as_deref(),
        Some("t2297973@gmail.com")
    );

    // A non-email jti (or no jti at all) must not become the email.
    let token = TokenResponse {
        access_token: jwt("b1b01c10-91dc-4a25-9d32-d17b5f5bbd9b"),
        ..token
    };
    assert_eq!(provider.email_from_token(&token), None);
}
