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

#[test]
fn sealed_meta_write_then_read_roundtrip() {
    let master_key = openproxy_db::secrets::MasterKey::generate().unwrap();
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute(
        "CREATE TABLE accounts (id INTEGER PRIMARY KEY, oauth_provider_specific TEXT)",
        [],
    )
    .unwrap();
    conn.execute("INSERT INTO accounts (id) VALUES (1)", [])
        .unwrap();

    let meta = KiroProviderMeta {
        client_id: "cid-123".into(),
        client_secret: "cs-456".into(),
        profile_arn: None,
        region: "us-east-1".into(),
        auth_method: None,
    };
    store_kiro_provider_specific(&conn, AccountId(1), &meta, Some(&master_key)).unwrap();

    // The stored blob must be sealed (not plaintext JSON).
    let stored: String = conn
        .query_row(
            "SELECT oauth_provider_specific FROM accounts WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        !stored.trim_start().starts_with('{'),
        "meta must be sealed, got plaintext"
    );

    let back = parse_kiro_provider_specific(Some(&stored), Some(&master_key)).unwrap();
    assert_eq!(back.client_id, "cid-123");
    assert_eq!(back.client_secret, "cs-456");
}

#[test]
fn sealed_blob_neither_decrypt_nor_parse_reads_default() {
    // Fail-closed: a tampered/planted non-JSON value must not be trusted and
    // must not fail the caller — it reads as default metadata.
    let garbage = "AQAmnRNn+tampered-blob";
    let meta = parse_kiro_provider_specific(Some(garbage), None).unwrap();
    assert!(meta.client_id.is_empty());
    assert_eq!(meta.region, "us-east-1");
}

#[test]
fn plaintext_legacy_row_still_reads_without_key() {
    // Runtime rows written before the seal (or by a no-key writer) parse as
    // legacy plaintext JSON until the boot adoption seals them.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute(
        "CREATE TABLE accounts (id INTEGER PRIMARY KEY, oauth_provider_specific TEXT)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO accounts (id, oauth_provider_specific) VALUES (1, '{\"client_id\":\"legacy-cid\",\"client_secret\":\"cs\"}')",
        [],
    )
    .unwrap();
    let meta = read_profile_meta(&conn, AccountId(1)).unwrap().unwrap();
    assert_eq!(meta.client_id, "legacy-cid");
}
