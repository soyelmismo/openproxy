use super::{fresh_pool, setup_account};
use crate::accounts::*;
use openproxy_db::secrets::MasterKey;
use openproxy_types::{AccountId, CoreError};

#[test]
fn oauth_access_token_roundtrip() {
    let (pool, _path, mk, id) = setup_account();
    let conn = pool.writer();
    let access = "ya29.a0AfH6SMB_test-access-token_12345";
    store_oauth_tokens(
        &conn,
        id,
        &mk,
        StoreOAuthTokensParams {
            access_token: access,
            token_type: "Bearer",
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(decrypt_access_token(&conn, id, &mk).unwrap(), access);
}

#[test]
fn oauth_refresh_token_roundtrip() {
    let (pool, _path, mk, id) = setup_account();
    let conn = pool.writer();
    let (access, refresh) = ("ya29.access", "1//0test-refresh-token_xyz");
    store_oauth_tokens(
        &conn,
        id,
        &mk,
        StoreOAuthTokensParams {
            access_token: access,
            refresh_token: Some(refresh),
            token_type: "Bearer",
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        decrypt_refresh_token(&conn, id, &mk).unwrap().as_deref(),
        Some(refresh)
    );
}

#[test]
fn oauth_no_refresh_token_returns_none() {
    let (pool, _path, mk, id) = setup_account();
    let conn = pool.writer();
    store_oauth_tokens(
        &conn,
        id,
        &mk,
        StoreOAuthTokensParams {
            access_token: "access-only",
            token_type: "Bearer",
            ..Default::default()
        },
    )
    .unwrap();
    assert!(decrypt_refresh_token(&conn, id, &mk).unwrap().is_none());
}

#[test]
fn oauth_empty_access_and_refresh_token() {
    let (pool, _path, mk, id) = setup_account();
    let conn = pool.writer();
    store_oauth_tokens(
        &conn,
        id,
        &mk,
        StoreOAuthTokensParams {
            access_token: "",
            refresh_token: Some(""),
            token_type: "Bearer",
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(decrypt_access_token(&conn, id, &mk).unwrap(), "");
    assert_eq!(
        decrypt_refresh_token(&conn, id, &mk).unwrap().as_deref(),
        Some("")
    );
}

#[test]
fn oauth_very_long_and_unicode_tokens() {
    let (pool, _path, mk, id) = setup_account();
    let conn = pool.writer();
    let (long_a, long_r) = ("A".repeat(10_000), "R".repeat(10_000));
    store_oauth_tokens(
        &conn,
        id,
        &mk,
        StoreOAuthTokensParams {
            access_token: &long_a,
            refresh_token: Some(&long_r),
            token_type: "Bearer",
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(decrypt_access_token(&conn, id, &mk).unwrap(), long_a);
    assert_eq!(
        decrypt_refresh_token(&conn, id, &mk).unwrap().as_deref(),
        Some(long_r.as_str())
    );

    let (uni_a, uni_r) = ("アクセストークン_🔑_12345", "リフレッシュ_🔄_67890");
    store_oauth_tokens(
        &conn,
        id,
        &mk,
        StoreOAuthTokensParams {
            access_token: uni_a,
            refresh_token: Some(uni_r),
            token_type: "Bearer",
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(decrypt_access_token(&conn, id, &mk).unwrap(), uni_a);
    assert_eq!(
        decrypt_refresh_token(&conn, id, &mk).unwrap().as_deref(),
        Some(uni_r)
    );
}

#[test]
fn oauth_wrong_key_fails_decrypt() {
    let (pool, _path, mk, id) = setup_account();
    let conn = pool.writer();
    store_oauth_tokens(
        &conn,
        id,
        &mk,
        StoreOAuthTokensParams {
            access_token: "tok",
            refresh_token: Some("ref"),
            token_type: "Bearer",
            ..Default::default()
        },
    )
    .unwrap();
    let wrong_mk = MasterKey::generate().unwrap();
    assert!(matches!(
        decrypt_access_token(&conn, id, &wrong_mk),
        Err(CoreError::Internal(_))
    ));
    assert!(matches!(
        decrypt_refresh_token(&conn, id, &wrong_mk),
        Err(CoreError::Internal(_))
    ));
}

#[test]
fn oauth_missing_account_operations() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    let mk = MasterKey::generate().unwrap();
    let missing = AccountId(99999);
    assert!(matches!(
        decrypt_access_token(&conn, missing, &mk),
        Err(CoreError::AccountNotFound(99999))
    ));
    assert!(matches!(
        decrypt_refresh_token(&conn, missing, &mk),
        Err(CoreError::AccountNotFound(99999))
    ));
    assert!(matches!(
        store_oauth_tokens(
            &conn,
            missing,
            &mk,
            StoreOAuthTokensParams {
                access_token: "tok",
                token_type: "Bearer",
                ..Default::default()
            }
        ),
        Err(CoreError::AccountNotFound(99999))
    ));
}

#[test]
fn oauth_replacing_tokens_overwrites_old() {
    let (pool, _path, mk, id) = setup_account();
    let conn = pool.writer();
    store_oauth_tokens(
        &conn,
        id,
        &mk,
        StoreOAuthTokensParams {
            access_token: "t1",
            refresh_token: Some("r1"),
            token_type: "Bearer",
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(decrypt_access_token(&conn, id, &mk).unwrap(), "t1");

    store_oauth_tokens(
        &conn,
        id,
        &mk,
        StoreOAuthTokensParams {
            access_token: "t2",
            refresh_token: Some("r2"),
            token_type: "Bearer",
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(decrypt_access_token(&conn, id, &mk).unwrap(), "t2");
    assert_eq!(
        decrypt_refresh_token(&conn, id, &mk).unwrap().as_deref(),
        Some("r2")
    );
}

#[test]
fn store_oauth_tokens_expiry_handling() {
    let (pool, _path, mk, id) = setup_account();
    let conn = pool.writer();
    store_oauth_tokens(
        &conn,
        id,
        &mk,
        StoreOAuthTokensParams {
            access_token: "t",
            expires_at: None,
            token_type: "Bearer",
            ..Default::default()
        },
    )
    .unwrap();
    let acc = get(&conn, id, &mk).unwrap().unwrap();
    assert!(acc.expires_at.is_some());

    let explicit = "2030-01-01T00:00:00Z";
    store_oauth_tokens(
        &conn,
        id,
        &mk,
        StoreOAuthTokensParams {
            access_token: "t2",
            expires_at: Some(explicit),
            token_type: "Bearer",
            ..Default::default()
        },
    )
    .unwrap();
    let acc2 = get(&conn, id, &mk).unwrap().unwrap();
    assert_eq!(acc2.expires_at.as_deref(), Some(explicit));
}

#[test]
fn decrypt_api_key_on_oauth_account_returns_validation_error() {
    let (pool, _path, mk, id) = setup_account();
    let conn = pool.writer();
    store_oauth_tokens(
        &conn,
        id,
        &mk,
        StoreOAuthTokensParams {
            access_token: "t",
            token_type: "Bearer",
            ..Default::default()
        },
    )
    .unwrap();
    let err = decrypt_api_key(&conn, id, &mk).unwrap_err();
    assert!(matches!(err, CoreError::Validation(_)));
}

#[test]
fn oauth_store_preserves_existing_refresh_token_when_none() {
    let (pool, _path, mk, id) = setup_account();
    let conn = pool.writer();
    store_oauth_tokens(
        &conn,
        id,
        &mk,
        StoreOAuthTokensParams {
            access_token: "t1",
            refresh_token: Some("r1"),
            token_type: "Bearer",
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        decrypt_refresh_token(&conn, id, &mk).unwrap().as_deref(),
        Some("r1")
    );

    store_oauth_tokens(
        &conn,
        id,
        &mk,
        StoreOAuthTokensParams {
            access_token: "t2",
            refresh_token: None,
            token_type: "Bearer",
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(decrypt_access_token(&conn, id, &mk).unwrap(), "t2");
    assert_eq!(
        decrypt_refresh_token(&conn, id, &mk).unwrap().as_deref(),
        Some("r1")
    );
}

#[test]
fn oauth_store_preserves_existing_provider_specific_when_none() {
    let (pool, _path, mk, id) = setup_account();
    let conn = pool.writer();
    let initial_specific = r#"{"credit_balance":"799.942","streak_days":2}"#;
    store_oauth_tokens(
        &conn,
        id,
        &mk,
        StoreOAuthTokensParams {
            access_token: "t1",
            token_type: "Bearer",
            provider_specific: Some(initial_specific),
            ..Default::default()
        },
    )
    .unwrap();
    let acc = get(&conn, id, &mk).unwrap().unwrap();
    assert_eq!(
        acc.oauth_provider_specific.as_deref(),
        Some(initial_specific)
    );

    // Token refresh passes provider_specific: None to avoid wiping out metadata
    store_oauth_tokens(
        &conn,
        id,
        &mk,
        StoreOAuthTokensParams {
            access_token: "t2",
            token_type: "Bearer",
            provider_specific: None,
            ..Default::default()
        },
    )
    .unwrap();
    let acc2 = get(&conn, id, &mk).unwrap().unwrap();
    assert_eq!(
        acc2.oauth_provider_specific.as_deref(),
        Some(initial_specific)
    );
}

#[test]
fn oauth_store_resets_unhealthy_status_and_errors() {
    let (pool, _path, mk, id) = setup_account();
    let conn = pool.writer();
    set_health(&conn, id, openproxy_types::HealthStatus::Unhealthy).unwrap();
    let acc = get(&conn, id, &mk).unwrap().unwrap();
    assert_eq!(acc.health_status, openproxy_types::HealthStatus::Unhealthy);

    store_oauth_tokens(
        &conn,
        id,
        &mk,
        StoreOAuthTokensParams {
            access_token: "t_fresh",
            token_type: "Bearer",
            ..Default::default()
        },
    )
    .unwrap();

    let updated = get(&conn, id, &mk).unwrap().unwrap();
    assert_eq!(updated.health_status, openproxy_types::HealthStatus::Healthy);
    assert!(updated.rate_limited_until.is_none());
    assert!(updated.quota_fetch_error.is_none());
}
