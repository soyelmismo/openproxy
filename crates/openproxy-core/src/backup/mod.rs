//! Backup, export, validation, and restore services for OpenProxy state and configuration.

pub mod crypto;
pub mod export;
pub mod introspection;
pub mod restore;
pub mod validate;

#[cfg(test)]
mod error_contract_tests;

pub use crypto::{decrypt_bundle_payload, encrypt_bundle_payload};
pub use export::export_backup;
pub use restore::restore_backup;
pub use validate::validate_backup;

#[cfg(test)]
mod tests {
    use super::*;
    use openproxy_db::MasterKey;
    use openproxy_types::backup::RestoreOptions;
    use rusqlite::Connection;

    fn setup_test_db() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        openproxy_db::migrations::run(&mut conn).unwrap();
        conn
    }

    #[test]
    fn test_export_validate_restore_unencrypted_roundtrip() {
        let master_key = MasterKey::generate().unwrap();
        let src_conn = setup_test_db();

        // Seed some data in source DB
        src_conn
            .execute(
                "INSERT INTO providers (id, name, base_url, auth_type, format, rate_limit_scope) \
                 VALUES ('test_prov', 'Test Provider', 'https://api.test.com', 'bearer', 'openai', 'account')",
                [],
            )
            .unwrap();

        let enc_key = master_key.encrypt("sk-secret-12345").unwrap();
        src_conn
            .execute(
                "INSERT INTO accounts (id, provider_id, api_key_encrypted, label, priority, health_status) \
                 VALUES (1, 'test_prov', ?1, 'Primary Key', 100, 'healthy')",
                rusqlite::params![enc_key],
            )
            .unwrap();

        src_conn
            .execute(
                "INSERT INTO models (id, provider_id, model_id, display_name, target_format, active) \
                 VALUES (1, 'test_prov', 'test-model-1', 'Test Model 1', 'openai', 1)",
                [],
            )
            .unwrap();

        src_conn
            .execute(
                "INSERT INTO combos (id, name, strategy) VALUES (1, 'smart-combo', 'priority')",
                [],
            )
            .unwrap();

        src_conn
            .execute(
                "INSERT INTO combo_targets (id, combo_id, provider_id, account_id, model_row_id, priority_order, active) \
                 VALUES (1, 1, 'test_prov', 1, 1, 1, 1)",
                [],
            )
            .unwrap();

        src_conn
            .execute(
                "INSERT INTO api_keys (id, key_hash, key_prefix, label, scopes_json, is_active) \
                 VALUES (1, 'hash123', 'op_live_test', 'Dev Key', '[\"chat\"]', 1)",
                [],
            )
            .unwrap();

        src_conn
            .execute(
                "INSERT INTO app_config (key, value, updated_at) VALUES ('recording_ttl_secs', '600', 123456789)",
                [],
            )
            .unwrap();

        // 1. Export without passphrase (plain JSON)
        let bundle = export_backup(&src_conn, &master_key, None).unwrap();
        assert!(!bundle.encrypted);
        let payload = bundle.payload.as_ref().unwrap();
        assert_eq!(payload.providers.len(), 1); // 'test_prov'
        assert_eq!(payload.accounts.len(), 1);
        assert_eq!(
            payload.accounts[0].api_key.as_deref(),
            Some("sk-secret-12345")
        );
        assert_eq!(payload.models.len(), 1);
        assert_eq!(payload.combos.len(), 1);
        assert_eq!(payload.combo_targets.len(), 1);
        assert_eq!(payload.api_keys.len(), 1);
        assert_eq!(payload.app_config.len(), 1);

        // 2. Validate
        let summary = validate_backup(&bundle, None).unwrap();
        assert_eq!(summary.accounts_count, 1);
        assert_eq!(summary.combos_count, 1);
        assert!(summary.warnings.is_empty());

        // 3. Restore into a new database with a different master key
        let target_master_key = MasterKey::generate().unwrap();
        let mut dst_conn = setup_test_db();

        let report = restore_backup(
            &mut dst_conn,
            &target_master_key,
            &bundle,
            &RestoreOptions::default(),
            None,
        )
        .unwrap();

        assert!(report.success);
        assert_eq!(report.accounts_restored, 1);
        assert_eq!(report.combos_restored, 1);

        // Verify restored account in destination database can be decrypted by target master key
        let enc_key_restored: Vec<u8> = dst_conn
            .query_row(
                "SELECT api_key_encrypted FROM accounts WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let decrypted_key = target_master_key.decrypt(&enc_key_restored).unwrap();
        assert_eq!(decrypted_key, "sk-secret-12345");

        // Verify combo targets exist
        let target_count: i64 = dst_conn
            .query_row("SELECT COUNT(*) FROM combo_targets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(target_count, 1);
    }

    #[test]
    fn test_export_encrypted_with_passphrase_roundtrip() {
        let master_key = MasterKey::generate().unwrap();
        let src_conn = setup_test_db();

        src_conn
            .execute(
                "INSERT INTO providers (id, name, base_url, auth_type, format, rate_limit_scope) \
                 VALUES ('prov_enc', 'Encrypted Prov', 'https://api.test.com', 'bearer', 'openai', 'account')",
                [],
            )
            .unwrap();

        let bundle = export_backup(&src_conn, &master_key, Some("passphrase-xyz")).unwrap();
        assert!(bundle.encrypted);
        assert!(bundle.payload.is_none());
        assert!(bundle.ciphertext.is_some());

        // Validate without passphrase fails
        assert!(validate_backup(&bundle, None).is_err());

        // Validate with wrong passphrase fails
        assert!(validate_backup(&bundle, Some("wrong-pass")).is_err());

        // Validate with correct passphrase succeeds
        let summary = validate_backup(&bundle, Some("passphrase-xyz")).unwrap();
        assert_eq!(summary.providers_count, 1);

        // Restore with correct passphrase into new DB
        let target_master_key = MasterKey::generate().unwrap();
        let mut dst_conn = setup_test_db();
        let opts = RestoreOptions {
            passphrase: Some("passphrase-xyz".to_string()),
            mode: None,
        };

        let report =
            restore_backup(&mut dst_conn, &target_master_key, &bundle, &opts, None).unwrap();
        assert!(report.success);
        assert_eq!(report.providers_restored, 1);
    }

    #[test]
    fn test_automatic_backup_extra_columns_and_extra_tables() {
        let master_key = MasterKey::generate().unwrap();
        let src_conn = setup_test_db();

        // Simulate a new migration adding an unknown column to providers
        src_conn
            .execute_batch(
                "ALTER TABLE providers ADD COLUMN future_setting TEXT DEFAULT 'default_future';",
            )
            .unwrap();

        // Simulate a new custom table not in core tables and not blacklisted
        src_conn
            .execute_batch(
                "CREATE TABLE custom_plugin_config (
                    plugin_id TEXT PRIMARY KEY,
                    config_json TEXT NOT NULL
                );
                INSERT INTO custom_plugin_config VALUES ('my_plugin', '{\"enabled\": true}');",
            )
            .unwrap();

        src_conn
            .execute(
                "INSERT INTO providers (id, name, base_url, auth_type, format, rate_limit_scope, future_setting) \
                 VALUES ('test_prov_fut', 'Future Prov', 'https://api.test.com', 'bearer', 'openai', 'account', 'custom_value_42')",
                [],
            )
            .unwrap();

        let bundle = export_backup(&src_conn, &master_key, None).unwrap();
        let payload = bundle.payload.as_ref().unwrap();

        // 1. Verify future_setting was captured automatically in extra_fields
        let prov = payload
            .providers
            .iter()
            .find(|p| p.id.as_str() == "test_prov_fut")
            .expect("provider present");
        assert_eq!(
            prov.extra_fields.get("future_setting"),
            Some(&serde_json::Value::String("custom_value_42".into()))
        );

        // 2. Verify custom table was captured in extra_tables
        assert!(payload.extra_tables.contains_key("custom_plugin_config"));

        // 3. Restore into a destination DB that also has the schema
        let mut dst_conn = setup_test_db();
        dst_conn
            .execute_batch(
                "ALTER TABLE providers ADD COLUMN future_setting TEXT DEFAULT 'default_future';
                 CREATE TABLE custom_plugin_config (
                    plugin_id TEXT PRIMARY KEY,
                    config_json TEXT NOT NULL
                 );",
            )
            .unwrap();

        let dst_master = MasterKey::generate().unwrap();
        let report = restore_backup(
            &mut dst_conn,
            &dst_master,
            &bundle,
            &RestoreOptions::default(),
            None,
        )
        .unwrap();
        assert!(report.success);

        // Verify extra column was restored
        let restored_val: String = dst_conn
            .query_row(
                "SELECT future_setting FROM providers WHERE id = 'test_prov_fut'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(restored_val, "custom_value_42");

        // Verify extra table was restored
        let custom_cfg: String = dst_conn
            .query_row(
                "SELECT config_json FROM custom_plugin_config WHERE plugin_id = 'my_plugin'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(custom_cfg, "{\"enabled\": true}");
    }
}
