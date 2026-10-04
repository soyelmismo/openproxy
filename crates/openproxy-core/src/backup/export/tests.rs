use super::accounts::export_accounts;
use super::catalog::{export_models, export_providers};
use super::routing::{export_combo_targets, export_combos};
use super::settings::{export_api_keys, export_app_config, export_proxy_sources};
use super::*;
use openproxy_db::MasterKey;
use openproxy_types::combos::Strategy;
use openproxy_types::{CoreError, RateLimitScope};
use rusqlite::Connection;

fn setup_test_db() -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    openproxy_db::migrations::run(&mut conn).unwrap();
    conn
}

#[test]
fn test_catalog_ordering_and_error_contract() {
    let conn = setup_test_db();
    conn.execute_batch(
        "INSERT INTO providers (id, name, base_url, auth_type, format, rate_limit_scope) \
         VALUES ('prov_b', 'B', 'https://b.com', 'bearer', 'openai', 'account'), \
                ('prov_a', 'A', 'https://a.com', 'bearer', 'openai', 'account'); \
         INSERT INTO models (id, provider_id, model_id, display_name, target_format, active) \
         VALUES (20, 'prov_a', 'm-z', 'Model Z', 'openai', 1), \
                (10, 'prov_a', 'm-a', 'Model A', 'openai', 0);",
    )
    .unwrap();

    let providers = export_providers(&conn).unwrap();
    assert_eq!(providers.len(), 2);
    assert_eq!(providers[0].id.as_str(), "prov_a");
    assert_eq!(providers[1].id.as_str(), "prov_b");

    let models = export_models(&conn).unwrap();
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].id, 10);
    assert_eq!(models[1].id, 20);

    conn.execute_batch(
        "PRAGMA ignore_check_constraints = ON; \
         UPDATE providers SET format = 'invalid_format' WHERE id = 'prov_a';",
    )
    .unwrap();
    let err = export_providers(&conn).unwrap_err();
    let CoreError::Database { message, source } = err else {
        panic!("expected database conversion error, got {err:?}");
    };
    let source = source.expect("typed SQL source");
    let sqlite = source.downcast_ref::<rusqlite::Error>().unwrap();
    assert_eq!(message, sqlite.to_string());
    assert!(matches!(
        sqlite,
        rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, _)
    ));
}

#[test]
fn test_accounts_secrets_and_fallbacks() {
    let master_key = MasterKey::generate().unwrap();
    let conn = setup_test_db();
    conn.execute(
        "INSERT INTO providers (id, name, base_url, auth_type, format, rate_limit_scope) \
         VALUES ('p1', 'P1', 'https://p1.com', 'bearer', 'openai', 'account')",
        [],
    )
    .unwrap();

    let enc_key = master_key.encrypt("sk-secret-acc1").unwrap();
    let access_token = master_key.encrypt("test-access-token").unwrap();
    let refresh_token = master_key.encrypt("test-refresh-token").unwrap();
    let enc_oauth = master_key.encrypt("oauth-secret").unwrap();
    let b64_oauth = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, enc_oauth);

    conn.execute(
        "INSERT INTO accounts (id, provider_id, api_key_encrypted, label, priority, health_status, oauth_provider_specific, access_token_encrypted, refresh_token_encrypted) \
         VALUES (1, 'p1', ?1, 'Acc 1', 10, 'healthy', ?2, ?3, ?4)",
        rusqlite::params![enc_key, b64_oauth, access_token, refresh_token],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO accounts (id, provider_id, api_key_encrypted, label, priority, health_status, oauth_provider_specific, access_token_encrypted) \
         VALUES (2, 'p1', ?1, 'Acc 2', 5, 'healthy', 'plain-fallback-text', X'FF')",
        rusqlite::params![Vec::<u8>::new()],
    )
    .unwrap();

    let accounts = export_accounts(&conn, &master_key).unwrap();
    assert_eq!(accounts.len(), 2);
    assert_eq!(accounts[0].id, 1);
    assert_eq!(accounts[0].api_key.as_deref(), Some("sk-secret-acc1"));
    assert_eq!(
        accounts[0].access_token.as_deref(),
        Some("test-access-token")
    );
    assert_eq!(
        accounts[0].refresh_token.as_deref(),
        Some("test-refresh-token")
    );
    assert_eq!(
        accounts[0].oauth_provider_specific.as_deref(),
        Some("oauth-secret")
    );
    assert_eq!(accounts[1].id, 2);
    assert_eq!(accounts[1].api_key, None);
    assert_eq!(accounts[1].access_token, None);
    assert_eq!(accounts[1].refresh_token, None);
    assert_eq!(
        accounts[1].oauth_provider_specific.as_deref(),
        Some("plain-fallback-text")
    );
}

#[test]
fn test_routing_combos_and_target_order() {
    let conn = setup_test_db();
    conn.execute_batch(
        "PRAGMA foreign_keys = OFF; \
         INSERT INTO providers (id, name, base_url, auth_type, format, rate_limit_scope) \
         VALUES ('p_route', 'Route Prov', 'https://p.com', 'bearer', 'openai', 'account'); \
         INSERT INTO combos (id, name, strategy, cooldown_base_secs, lkgp_exploration_rate) \
         VALUES (2, 'combo-b', 'round_robin', 60, 0.1), \
                (1, 'combo-a', 'priority', 30, 0.05); \
         INSERT INTO combo_targets (id, combo_id, provider_id, priority_order, active) \
         VALUES (101, 1, 'p_route', 20, 1), \
                (102, 1, 'no_prov', 10, 1);",
    )
    .unwrap();

    let combos = export_combos(&conn).unwrap();
    assert_eq!(combos.len(), 2);
    assert_eq!(combos[0].id, 1);
    assert_eq!(combos[0].strategy, Strategy::Priority);
    assert_eq!(combos[0].cooldown_base_secs, Some(30));
    assert_eq!(combos[0].lkgp_exploration_rate, Some(0.05));
    assert_eq!(combos[1].id, 2);

    let targets = export_combo_targets(&conn).unwrap();
    assert_eq!(targets.len(), 2);
    assert_eq!(targets[0].id, 102);
    assert_eq!(targets[0].priority_order, 10);
    assert_eq!(targets[0].rate_limit_scope, RateLimitScope::Account);
    assert_eq!(targets[1].id, 101);
    assert_eq!(targets[1].priority_order, 20);
}

#[test]
fn test_settings_sources_keys_config_order() {
    let conn = setup_test_db();
    conn.execute_batch(
        "INSERT INTO proxy_sources (id, name, url, priority, active, is_builtin) \
         VALUES (1, 'p-low', 'http://1', 10, 1, 0), \
                (2, 'p-high-2', 'http://2', 50, 1, 0), \
                (3, 'p-high-1', 'http://3', 50, 1, 0); \
         INSERT INTO api_keys (id, key_hash, key_prefix, label, scopes_json, is_active) \
         VALUES (20, 'hash20', 'op_live_b', 'Key B', '[\"chat\"]', 1), \
                (10, 'hash10', 'op_live_a', 'Key A', '[\"all\"]', 0); \
         INSERT INTO app_config (key, value, updated_at) \
         VALUES ('z_setting', 'val_z', 200), \
                ('a_setting', 'val_a', 100);",
    )
    .unwrap();

    let sources = export_proxy_sources(&conn).unwrap();
    assert_eq!(sources.len(), 3);
    assert_eq!(sources[0].id, "2");
    assert_eq!(sources[1].id, "3");
    assert_eq!(sources[2].id, "1");

    let keys = export_api_keys(&conn).unwrap();
    assert_eq!(keys.len(), 2);
    assert_eq!(keys[0].id, 10);
    assert_eq!(keys[0].key_prefix.as_deref(), Some("op_live_a"));
    assert_eq!(keys[0].key_hash, "hash10");
    assert_eq!(keys[0].scopes_json, "[\"all\"]");
    assert!(!keys[0].is_active);
    assert_eq!(keys[1].id, 20);

    let config = export_app_config(&conn).unwrap();
    assert_eq!(config.len(), 2);
    assert_eq!(config[0].key, "a_setting");
    assert_eq!(config[1].key, "z_setting");
}

#[test]
fn test_export_backup_coordination() {
    let master_key = MasterKey::generate().unwrap();
    let conn = setup_test_db();
    let plain_bundle = export_backup(&conn, &master_key, None).unwrap();
    assert!(!plain_bundle.encrypted);
    assert!(plain_bundle.payload.is_some());

    let enc_bundle = export_backup(&conn, &master_key, Some("passphrase123")).unwrap();
    assert!(enc_bundle.encrypted);
    assert!(enc_bundle.ciphertext.is_some());
}
