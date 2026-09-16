use super::*;

#[test]
fn name_and_flow() {
    let p = KiroOAuthProvider::new();
    assert_eq!(p.name(), "kiro");
    assert_eq!(p.flow(), OAuthFlow::DeviceCode);
}

#[test]
fn kiro_provider_meta_serde_roundtrip() {
    let meta = KiroProviderMeta {
        client_id: "test-client-id".into(),
        client_secret: "test-client-secret".into(),
        profile_arn: Some("arn:aws:codewhisperer:us-east-1:123:profile/abc".into()),
        region: "us-east-1".into(),
        auth_method: None,
    };
    let json = serde_json::to_string(&meta).unwrap();
    let back: KiroProviderMeta = serde_json::from_str(&json).unwrap();
    assert_eq!(back.client_id, "test-client-id");
    assert_eq!(back.client_secret, "test-client-secret");
    assert_eq!(
        back.profile_arn.as_deref(),
        Some("arn:aws:codewhisperer:us-east-1:123:profile/abc")
    );
    assert_eq!(back.region, "us-east-1");
}

#[test]
fn kiro_provider_meta_default_region() {
    let raw = r#"{"client_id":"c","client_secret":"s"}"#;
    let meta: KiroProviderMeta = serde_json::from_str(raw).unwrap();
    assert_eq!(meta.region, "us-east-1");
    assert!(meta.profile_arn.is_none());
}

#[test]
fn test_read_profile_meta_null() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute(
        "CREATE TABLE accounts (id INTEGER PRIMARY KEY, oauth_provider_specific TEXT)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO accounts (id, oauth_provider_specific) VALUES (1, NULL)",
        [],
    )
    .unwrap();
    let meta = read_profile_meta(&conn, AccountId(1)).unwrap().unwrap();
    assert_eq!(meta.region, "us-east-1");
    assert!(meta.client_id.is_empty());
}
