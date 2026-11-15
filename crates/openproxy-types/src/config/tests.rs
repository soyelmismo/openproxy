use super::*;
use crate::FromDb;

#[test]
fn test_compression_mode_enum() {
    assert_eq!(CompressionMode::Off.as_str(), "off");
    assert_eq!(CompressionMode::Lite.as_str(), "lite");
    assert_eq!(CompressionMode::Rtk.as_str(), "rtk");
    assert_eq!(CompressionMode::LiteRtk.as_str(), "lite_rtk");
    assert_eq!(CompressionMode::parse("lite"), Ok(CompressionMode::Lite));
    assert_eq!(CompressionMode::parse("rtk"), Ok(CompressionMode::Rtk));
    assert!(CompressionMode::parse("invalid").is_err());
}

#[test]
fn test_encryption_key_source_enum() {
    assert_eq!(EncryptionKeySource::Env.as_str(), "env");
    assert_eq!(EncryptionKeySource::File.as_str(), "file");
    assert_eq!(
        EncryptionKeySource::parse("env"),
        Ok(EncryptionKeySource::Env)
    );
    assert_eq!(
        EncryptionKeySource::parse("file"),
        Ok(EncryptionKeySource::File)
    );
    assert!(EncryptionKeySource::parse("invalid").is_err());
}

#[test]
fn test_cooldown_mode_as_str() {
    assert_eq!(CooldownMode::Flat.as_str(), "flat");
    assert_eq!(CooldownMode::Exponential.as_str(), "exponential");
    assert_eq!(CooldownMode::None.as_str(), "none");
}

#[test]
fn test_cooldown_mode_parse() {
    assert_eq!(CooldownMode::parse("flat"), Ok(CooldownMode::Flat));
    assert_eq!(
        CooldownMode::parse("exponential"),
        Ok(CooldownMode::Exponential)
    );
    assert_eq!(CooldownMode::parse("none"), Ok(CooldownMode::None));
    assert_eq!(CooldownMode::parse("disabled"), Ok(CooldownMode::None));
    assert_eq!(CooldownMode::parse("off"), Ok(CooldownMode::None));
    assert!(CooldownMode::parse("unknown").is_err());
    assert_eq!(
        CooldownMode::parse("invalid"),
        Err("invalid cooldown_mode: invalid".to_string())
    );
}

#[test]
fn test_cooldown_mode_from_db() {
    assert_eq!(
        CooldownMode::from_db(Some("exponential")),
        CooldownMode::Exponential
    );
    assert_eq!(CooldownMode::from_db(Some("none")), CooldownMode::None);
    assert_eq!(CooldownMode::from_db(Some("disabled")), CooldownMode::None);
    assert_eq!(CooldownMode::from_db(Some("off")), CooldownMode::None);
    assert_eq!(CooldownMode::from_db(Some("flat")), CooldownMode::Flat);
    assert_eq!(CooldownMode::from_db(Some("unknown")), CooldownMode::Flat);
    assert_eq!(CooldownMode::from_db(None), CooldownMode::Flat);
}

#[test]
fn test_pii_entity_and_config() {
    assert_eq!(PiiEntity::Email.as_str(), "email");
    assert_eq!(PiiEntity::Email.placeholder_prefix(), "EMAIL");
    assert_eq!(PiiEntity::Phone.placeholder_prefix(), "PHONE");
    assert_eq!(PiiEntity::Ip.placeholder_prefix(), "IP");
    assert_eq!(PiiEntity::CreditCard.placeholder_prefix(), "CARD");
    assert_eq!(PiiEntity::Secret.placeholder_prefix(), "KEY");
    assert_eq!(PiiEntity::Person.placeholder_prefix(), "PERSON");

    assert_eq!(PiiEntity::parse("email").unwrap(), PiiEntity::Email);
    assert_eq!(PiiEntity::parse("card").unwrap(), PiiEntity::CreditCard);
    assert_eq!(
        PiiEntity::parse("credit_card").unwrap(),
        PiiEntity::CreditCard
    );
    assert_eq!(PiiEntity::parse("key").unwrap(), PiiEntity::Secret);
    assert_eq!(PiiEntity::parse("api_key").unwrap(), PiiEntity::Secret);

    let default_cfg = PiiConfig::default();
    assert!(!default_cfg.pii_enabled);
    assert!(default_cfg.pii_reversible);
    assert!(default_cfg.pii_redact_logs);
    assert_eq!(default_cfg.pii_entities.len(), 6);

    let json = serde_json::to_string(&default_cfg).unwrap();
    let parsed: PiiConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(default_cfg, parsed);

    let aliased_json = r#"{"enabled": true, "reversible": false, "redact_logs": false, "entities": ["email", "key"]}"#;
    let aliased: PiiConfig = serde_json::from_str(aliased_json).unwrap();
    assert!(aliased.pii_enabled);
    assert!(!aliased.pii_reversible);
    assert!(!aliased.pii_redact_logs);
    assert_eq!(
        aliased.pii_entities,
        vec![PiiEntity::Email, PiiEntity::Secret]
    );

    let dup_json =
        r#"{"pii_entities": ["credit_card", "person", "card", "person", "credit_card", "name"]}"#;
    let dup_cfg: PiiConfig = serde_json::from_str(dup_json).unwrap();
    assert_eq!(
        dup_cfg.pii_entities,
        vec![PiiEntity::CreditCard, PiiEntity::Person]
    );
}

#[test]
fn test_sqlite_synchronous_enum() {
    assert_eq!(SqliteSynchronous::default(), SqliteSynchronous::Full);
    assert_eq!(SqliteSynchronous::Full.as_str(), "full");
    assert_eq!(SqliteSynchronous::Normal.as_str(), "normal");
    assert_eq!(SqliteSynchronous::Full.as_pragma_str(), "FULL");
    assert_eq!(SqliteSynchronous::Normal.as_pragma_str(), "NORMAL");

    assert_eq!(
        SqliteSynchronous::parse("full"),
        Ok(SqliteSynchronous::Full)
    );
    assert_eq!(
        SqliteSynchronous::parse("FULL"),
        Ok(SqliteSynchronous::Full)
    );
    assert_eq!(
        SqliteSynchronous::parse("Full"),
        Ok(SqliteSynchronous::Full)
    );
    assert_eq!(
        SqliteSynchronous::parse("normal"),
        Ok(SqliteSynchronous::Normal)
    );
    assert_eq!(
        SqliteSynchronous::parse("NORMAL"),
        Ok(SqliteSynchronous::Normal)
    );
    assert_eq!(
        SqliteSynchronous::parse("Normal"),
        Ok(SqliteSynchronous::Normal)
    );
    assert!(SqliteSynchronous::parse("off").is_err());
    assert!(SqliteSynchronous::parse("invalid").is_err());
}

#[test]
fn test_storage_config_backward_compatibility_and_synchronous() {
    let def = StorageConfig::default();
    assert_eq!(def.synchronous, SqliteSynchronous::Full);

    // Deserializing without synchronous field defaults safely to Full
    let json_no_sync = r#"{"database_path": "/var/data.db", "encryption_key_source": "env"}"#;
    let parsed_no_sync: StorageConfig = serde_json::from_str(json_no_sync).unwrap();
    assert_eq!(parsed_no_sync.synchronous, SqliteSynchronous::Full);
    assert_eq!(parsed_no_sync.database_path, "/var/data.db");

    // Deserializing with synchronous = "normal"
    let json_normal = r#"{"database_path": "/var/data.db", "synchronous": "normal", "encryption_key_source": "env"}"#;
    let parsed_normal: StorageConfig = serde_json::from_str(json_normal).unwrap();
    assert_eq!(parsed_normal.synchronous, SqliteSynchronous::Normal);

    // Deserializing with synchronous = "full"
    let json_full = r#"{"database_path": "/var/data.db", "synchronous": "full", "encryption_key_source": "env"}"#;
    let parsed_full: StorageConfig = serde_json::from_str(json_full).unwrap();
    assert_eq!(parsed_full.synchronous, SqliteSynchronous::Full);

    // Deserializing with alias sqlite_synchronous = "normal"
    let json_alias = r#"{"database_path": "/var/data.db", "sqlite_synchronous": "normal", "encryption_key_source": "env"}"#;
    let parsed_alias: StorageConfig = serde_json::from_str(json_alias).unwrap();
    assert_eq!(parsed_alias.synchronous, SqliteSynchronous::Normal);

    // Roundtrip
    let serialized = serde_json::to_string(&parsed_normal).unwrap();
    let roundtripped: StorageConfig = serde_json::from_str(&serialized).unwrap();
    assert_eq!(roundtripped.synchronous, SqliteSynchronous::Normal);
}
