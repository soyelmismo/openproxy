use super::export::export_backup;
use super::restore::restore_backup;
use openproxy_db::MasterKey;
use openproxy_types::backup::{
    BACKUP_FORMAT_VERSION, BackupBundle, BackupPayload, BackupProvider, RestoreOptions,
};
use openproxy_types::{CoreError, ProviderFormat, ProviderId, RateLimitScope, providers::AuthType};
use rusqlite::Connection;

#[test]
fn test_export_missing_table_preserves_sqlite_failure_downcast() {
    let master_key = MasterKey::generate().unwrap();
    let conn = Connection::open_in_memory().unwrap();

    let err = export_backup(&conn, &master_key, None).unwrap_err();
    let CoreError::Database { message, source } = err else {
        panic!("expected CoreError::Database, got {err:?}");
    };

    let source = source.expect("expected source to be Some");
    let rusqlite_err = source
        .downcast_ref::<rusqlite::Error>()
        .expect("source should downcast to rusqlite::Error");

    assert_eq!(message, rusqlite_err.to_string());
    assert!(
        matches!(
            rusqlite_err,
            rusqlite::Error::SqliteFailure(..) | rusqlite::Error::SqlInputError { .. }
        ),
        "expected SqliteFailure or SqlInputError, got {rusqlite_err:?}"
    );
}

#[test]
fn test_export_row_conversion_failure_preserves_from_sql_error() {
    let master_key = MasterKey::generate().unwrap();
    let mut conn = Connection::open_in_memory().unwrap();
    openproxy_db::migrations::run(&mut conn).unwrap();

    conn.execute(
        "INSERT INTO providers (id, name, base_url, auth_type, format, rate_limit_scope) \
         VALUES ('test_prov', 'Test Provider', 'https://api.test.com', 'bearer', 'openai', 'account')",
        [],
    ).unwrap();

    conn.execute_batch(
        "PRAGMA ignore_check_constraints = ON; \
         UPDATE providers SET format = 'invalid_format' WHERE id = 'test_prov';",
    )
    .unwrap();

    let err = export_backup(&conn, &master_key, None).unwrap_err();
    let CoreError::Database { message, source } = err else {
        panic!("expected CoreError::Database, got {err:?}");
    };

    let source = source.expect("expected source to be Some");
    let rusqlite_err = source
        .downcast_ref::<rusqlite::Error>()
        .expect("source should downcast to rusqlite::Error");

    assert_eq!(message, rusqlite_err.to_string());
    match rusqlite_err {
        rusqlite::Error::FromSqlConversionFailure(idx, typ, _) => {
            assert_eq!(*idx, 4);
            assert_eq!(*typ, rusqlite::types::Type::Text);
        }
        other => panic!("expected FromSqlConversionFailure, got {other:?}"),
    }
}

#[test]
fn test_restore_duplicate_provider_preserves_downcast_and_rolls_back() {
    let master_key = MasterKey::generate().unwrap();
    let mut conn = Connection::open_in_memory().unwrap();
    openproxy_db::migrations::run(&mut conn).unwrap();

    conn.execute(
        "INSERT INTO providers (id, name, base_url, auth_type, format, rate_limit_scope) \
         VALUES ('sentinel', 'Sentinel', 'https://sentinel.test', 'bearer', 'openai', 'account')",
        [],
    )
    .unwrap();

    let p = BackupProvider {
        id: ProviderId::new("dup_prov"),
        name: "Dup".into(),
        base_url: "https://api.test.com".into(),
        auth_type: AuthType::Bearer,
        format: ProviderFormat::Openai,
        extra_headers_json: None,
        auto_activate_keyword: None,
        active: true,
        use_proxies: false,
        current_proxy_id: None,
        proxy_rotation_errors: "403,429".into(),
        rate_limit_scope: RateLimitScope::Account,
        notif_keyword_only: false,
        proxy_rotation_mode: "global".into(),
    };
    let bundle = BackupBundle {
        version: BACKUP_FORMAT_VERSION,
        exported_at: chrono::Utc::now().to_rfc3339(),
        openproxy_version: "1.0.0".into(),
        encrypted: false,
        kdf: None,
        kdf_salt: None,
        kdf_iterations: None,
        nonce: None,
        ciphertext: None,
        payload: Some(BackupPayload {
            providers: vec![p.clone(), p],
            ..Default::default()
        }),
    };

    let err = restore_backup(
        &mut conn,
        &master_key,
        &bundle,
        &RestoreOptions::default(),
        None,
    )
    .unwrap_err();
    let CoreError::Database { message, source } = err else {
        panic!("expected CoreError::Database, got {err:?}");
    };

    let source = source.expect("expected source to be Some");
    let rusqlite_err = source
        .downcast_ref::<rusqlite::Error>()
        .expect("source downcast to rusqlite::Error");

    assert_eq!(
        message,
        format!("restore provider dup_prov: {rusqlite_err}")
    );
    assert!(matches!(rusqlite_err, rusqlite::Error::SqliteFailure(..)));

    let sentinel_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM providers WHERE id = 'sentinel'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(sentinel_count, 1, "sentinel should survive after rollback");
}
