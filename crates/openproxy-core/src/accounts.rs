//! Account CRUD. API keys are stored encrypted (BLOB) using a MasterKey.

use crate::error::{CoreError, Result};
use crate::ids::AccountId;
use rusqlite::{Connection, params};

pub use openproxy_db::accounts::*;
pub use openproxy_types::accounts::*;

/// Stamp a fresh quota snapshot onto the account row. The `AccountQuota`
/// struct is the one defined in [`crate::quota`]; the fields map 1:1 onto
/// the `quota_*` columns added by migration 000012.
///
/// Every field is written in a single UPDATE so the row stays
/// consistent: a half-written quota snapshot (e.g. session filled but
/// weekly missing) is never observable.
///
/// A failure to find the row surfaces as [`CoreError::AccountNotFound`].
pub fn set_quota(conn: &Connection, id: AccountId, q: &crate::quota::AccountQuota) -> Result<()> {
    // Serialize model_details (per-model quota breakdown) as JSON for
    // storage. NULL when the provider doesn't expose per-model quota.
    let model_details_json: Option<String> = q
        .model_details
        .as_ref()
        .and_then(|d| serde_json::to_string(d).ok())
        .filter(|s| s != "null" && s != "[]");

    let affected = conn
        .execute(
            "UPDATE accounts SET \
                quota_session_used       = ?1, \
                quota_session_limit      = ?2, \
                quota_session_reset_at   = ?3, \
                quota_weekly_used        = ?4, \
                quota_weekly_limit       = ?5, \
                quota_weekly_reset_at    = ?6, \
                quota_plan_name          = ?7, \
                quota_last_fetched_at    = ?8, \
                quota_fetch_error        = ?9, \
                quota_model_details      = ?10 \
             WHERE id = ?11",
            params![
                q.session_used,
                q.session_limit,
                q.session_reset_at,
                q.weekly_used,
                q.weekly_limit,
                q.weekly_reset_at,
                q.plan_name,
                q.last_fetched_at,
                q.fetch_error,
                model_details_json,
                id.0,
            ],
        )
        .map_err(openproxy_db::error::map_db_error_ctx(format!(
            "update quota for account {}",
            id.0
        )))?;
    if affected == 0 {
        return Err(CoreError::AccountNotFound(id.0));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use openproxy_db::conn::DbPool;
    use openproxy_db::secrets::MasterKey;
    use openproxy_types::ids::ProviderId;

    use crate::providers::{self, AuthType, ProviderFormat};
    use std::path::PathBuf;

    /// Build a fresh in-process pool: temp dir on disk, migrations applied,
    /// a provider seeded so account FK constraints can be satisfied.
    fn fresh_pool() -> (DbPool, PathBuf) {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!("openproxy-accounts-test-{pid}-{nanos}-{n}"));
        std::fs::create_dir_all(&dir).expect("mkdir tempdir");
        let path = dir.join("accounts.db");
        let pool = DbPool::open(&path).expect("open pool");
        {
            let mut w = pool.writer();
            openproxy_db::migrations::run(&mut w).expect("migrations");
        }
        (pool, path)
    }

    /// Seed a provider so accounts can be created against it.
    fn seed_provider(conn: &Connection, id: &str) {
        providers::create(
            conn,
            providers::NewProvider {
                id: &ProviderId::new(id),
                name: id,
                base_url: "https://example.com",
                auth_type: AuthType::Bearer,
                format: ProviderFormat::Openai,
                extra_headers_json: None,
                auto_activate_keyword: None,
                rate_limit_scope: crate::providers::RateLimitScope::Account,
            },
        )
        .expect("seed provider");
    }

    #[test]
    fn create_and_get() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");

        let mk = MasterKey::generate();
        let id = create(
            &conn,
            &ProviderId::new("openrouter"),
            Some("sk-test-123"),
            &mk,
            Some("primary"),
            10,
            Some(r#"{"org":"acme"}"#),
        )
        .expect("create");

        let acc = get(&conn, id, &mk).expect("get").expect("present");
        assert_eq!(acc.id, id);
        assert_eq!(acc.provider_id, ProviderId::new("openrouter"));
        assert_eq!(acc.label.as_deref(), Some("primary"));
        assert_eq!(acc.priority, 10);
        assert_eq!(acc.extra_config_json.as_deref(), Some(r#"{"org":"acme"}"#));
        assert_eq!(acc.health_status, HealthStatus::Healthy);
        assert!(acc.rate_limited_until.is_none());
        assert!(!acc.created_at.is_empty(), "DB stamps created_at");
        assert_eq!(acc.auth_type.as_ref(), "api_key", "default auth_type");

        // Missing id → None, not error.
        assert!(
            get(&conn, AccountId(9999), &mk)
                .expect("get missing")
                .is_none()
        );
    }

    #[test]
    fn create_encrypts_api_key_at_rest() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");

        let mk = MasterKey::generate();
        let plaintext = "sk-supersecret-DO-NOT-LEAK-9f8a";
        let id = create(
            &conn,
            &ProviderId::new("openrouter"),
            Some(plaintext),
            &mk,
            None,
            100,
            None,
        )
        .expect("create");

        // Read the raw BLOB straight out of SQLite, bypass the typed API.
        let raw: Vec<u8> = conn
            .query_row(
                "SELECT api_key_encrypted FROM accounts WHERE id = ?1",
                params![id.0],
                |r| r.get(0),
            )
            .expect("select blob");

        // The plaintext must not appear anywhere in the ciphertext bytes.
        let raw_str = String::from_utf8_lossy(&raw);
        assert!(
            !raw_str.contains(plaintext),
            "plaintext must not appear in stored blob"
        );
        // And the blob must be at least nonce + tag (12 + 16 bytes) long.
        assert!(raw.len() >= 28, "blob too small: {} bytes", raw.len());
    }

    #[test]
    fn decrypt_api_key_roundtrip() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");

        let mk = MasterKey::generate();
        let plaintext = "sk-roundtrip-xyz";
        let id = create(
            &conn,
            &ProviderId::new("openrouter"),
            Some(plaintext),
            &mk,
            None,
            100,
            None,
        )
        .expect("create");

        let recovered = decrypt_api_key(&conn, id, &mk).expect("decrypt");
        assert_eq!(recovered, plaintext);

        // Missing id → AccountNotFound.
        let err = decrypt_api_key(&conn, AccountId(424_242), &mk).expect_err("missing");
        assert!(matches!(err, CoreError::AccountNotFound(424_242)));

        // Wrong key → decryption failure (Internal).
        let other = MasterKey::generate();
        let err = decrypt_api_key(&conn, id, &other).expect_err("wrong key");
        assert!(matches!(err, CoreError::Internal(_)));
    }

    #[test]
    fn decrypt_api_key_and_label_roundtrip() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "cloudflare");

        let mk = MasterKey::generate();
        let plaintext = "sk-roundtrip-xyz";
        let label = "my-cf-account-id";

        let id = create(
            &conn,
            &ProviderId::new("cloudflare"),
            Some(plaintext),
            &mk,
            Some(label),
            100,
            None,
        )
        .expect("create");

        let (recovered_key, recovered_label) =
            decrypt_api_key_and_label(&conn, id, &mk).expect("decrypt");
        assert_eq!(recovered_key, plaintext);
        assert_eq!(recovered_label.as_deref(), Some(label));

        // Missing id → AccountNotFound.
        let err = decrypt_api_key_and_label(&conn, AccountId(424_242), &mk).expect_err("missing");
        assert!(matches!(err, CoreError::AccountNotFound(424_242)));

        // Wrong key → decryption failure (Internal).
        let other = MasterKey::generate();
        let err = decrypt_api_key_and_label(&conn, id, &other).expect_err("wrong key");
        assert!(matches!(err, CoreError::Internal(_)));

        // Test account without a label
        let id_no_label = create(
            &conn,
            &ProviderId::new("cloudflare"),
            Some("sk-roundtrip-no-label"),
            &mk,
            None,
            100,
            None,
        )
        .expect("create no label");

        let (recovered_key_no_label, recovered_label_none) =
            decrypt_api_key_and_label(&conn, id_no_label, &mk).expect("decrypt");
        assert_eq!(recovered_key_no_label, "sk-roundtrip-no-label");
        assert_eq!(recovered_label_none, None);
    }

    #[test]
    fn list_filters_by_provider() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");
        seed_provider(&conn, "anthropic");

        let mk = MasterKey::generate();
        for (pid, prio) in [("openrouter", 10), ("openrouter", 20), ("anthropic", 5)] {
            create(
                &conn,
                &ProviderId::new(pid),
                Some("sk-x"),
                &mk,
                None,
                prio,
                None,
            )
            .expect("create");
        }

        let all = list(&conn, None, &mk).expect("list all");
        assert_eq!(all.len(), 3);

        let only_or = list(&conn, Some(&ProviderId::new("openrouter")), &mk).expect("list or");
        assert_eq!(only_or.len(), 2);
        // Ordered by priority ASC.
        assert_eq!(only_or[0].priority, 10);
        assert_eq!(only_or[1].priority, 20);
        for a in &only_or {
            assert_eq!(a.provider_id, ProviderId::new("openrouter"));
        }

        let only_an = list(&conn, Some(&ProviderId::new("anthropic")), &mk).expect("list an");
        assert_eq!(only_an.len(), 1);
        assert_eq!(only_an[0].provider_id, ProviderId::new("anthropic"));

        let none = list(&conn, Some(&ProviderId::new("nope")), &mk).expect("list nope");
        assert!(none.is_empty());
    }

    #[test]
    fn set_health_updates_status() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");

        let mk = MasterKey::generate();
        let id = create(
            &conn,
            &ProviderId::new("openrouter"),
            Some("sk-x"),
            &mk,
            None,
            100,
            None,
        )
        .expect("create");

        set_health(&conn, id, HealthStatus::Degraded).expect("set degraded");
        let a = get(&conn, id, &mk).expect("get").expect("present");
        assert_eq!(a.health_status, HealthStatus::Degraded);

        set_health(&conn, id, HealthStatus::Unhealthy).expect("set unhealthy");
        let a = get(&conn, id, &mk).expect("get").expect("present");
        assert_eq!(a.health_status, HealthStatus::Unhealthy);

        set_health(&conn, id, HealthStatus::Healthy).expect("back to healthy");
        let a = get(&conn, id, &mk).expect("get").expect("present");
        assert_eq!(a.health_status, HealthStatus::Healthy);

        // Missing id → AccountNotFound.
        let err = set_health(&conn, AccountId(7777), HealthStatus::Healthy).expect_err("missing");
        assert!(matches!(err, CoreError::AccountNotFound(7777)));
    }

    #[test]
    fn set_rate_limited_updates() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");

        let mk = MasterKey::generate();
        let id = create(
            &conn,
            &ProviderId::new("openrouter"),
            Some("sk-x"),
            &mk,
            None,
            100,
            None,
        )
        .expect("create");

        // Initially None.
        let a = get(&conn, id, &mk).expect("get").expect("present");
        assert!(a.rate_limited_until.is_none());

        set_rate_limited_until(&conn, id, Some("2026-06-13T12:34:56Z")).expect("set");
        let a = get(&conn, id, &mk).expect("get").expect("present");
        assert_eq!(
            a.rate_limited_until.as_deref(),
            Some("2026-06-13T12:34:56Z")
        );

        // Clear with None.
        set_rate_limited_until(&conn, id, None).expect("clear");
        let a = get(&conn, id, &mk).expect("get").expect("present");
        assert!(a.rate_limited_until.is_none());

        // Missing id → AccountNotFound.
        let err = set_rate_limited_until(&conn, AccountId(12321), Some("x")).expect_err("missing");
        assert!(matches!(err, CoreError::AccountNotFound(12321)));
    }

    #[test]
    fn delete_removes_account() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");

        let mk = MasterKey::generate();
        let id = create(
            &conn,
            &ProviderId::new("openrouter"),
            Some("sk-x"),
            &mk,
            None,
            100,
            None,
        )
        .expect("create");
        assert!(get(&conn, id, &mk).expect("get").is_some());

        delete(&conn, id).expect("delete");
        assert!(get(&conn, id, &mk).expect("get after delete").is_none());

        // Idempotent: a second delete is a no-op, not an error.
        delete(&conn, id).expect("delete again is fine");
    }

    #[test]
    fn set_quota_roundtrip() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "minimax");

        let mk = MasterKey::generate();
        let id = create(
            &conn,
            &ProviderId::new("minimax"),
            Some("sk-quota"),
            &mk,
            Some("quota-test"),
            10,
            None,
        )
        .expect("create");

        // Initially: every quota_* column is NULL.
        let a = get(&conn, id, &mk).expect("get").expect("present");
        assert!(a.quota_session_used.is_none());
        assert!(a.quota_session_limit.is_none());
        assert!(a.quota_session_reset_at.is_none());
        assert!(a.quota_weekly_used.is_none());
        assert!(a.quota_weekly_limit.is_none());
        assert!(a.quota_weekly_reset_at.is_none());
        assert!(a.quota_plan_name.is_none());
        assert!(a.quota_last_fetched_at.is_none());
        assert!(a.quota_fetch_error.is_none());

        // Stamp a snapshot.
        let q = crate::quota::AccountQuota {
            session_used: Some(1234),
            session_limit: Some(5000),
            session_reset_at: Some("1700000000".into()),
            weekly_used: Some(80000),
            weekly_limit: Some(500_000),
            weekly_reset_at: Some("1700003600".into()),
            plan_name: Some("Coding Plan".into()),
            last_fetched_at: "1700000001".into(),
            fetch_error: None,
            model_details: None,
        };
        set_quota(&conn, id, &q).expect("set_quota");

        // Re-read: every field survives the round-trip.
        let a = get(&conn, id, &mk).expect("get").expect("present");
        assert_eq!(a.quota_session_used, Some(1234));
        assert_eq!(a.quota_session_limit, Some(5000));
        assert_eq!(a.quota_session_reset_at.as_deref(), Some("1700000000"));
        assert_eq!(a.quota_weekly_used, Some(80000));
        assert_eq!(a.quota_weekly_limit, Some(500_000));
        assert_eq!(a.quota_weekly_reset_at.as_deref(), Some("1700003600"));
        assert_eq!(a.quota_plan_name.as_deref(), Some("Coding Plan"));
        assert_eq!(a.quota_last_fetched_at.as_deref(), Some("1700000001"));
        assert!(a.quota_fetch_error.is_none());

        // Also visible through `list`.
        let all = list(&conn, Some(&ProviderId::new("minimax")), &mk).expect("list");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].quota_session_used, Some(1234));
    }

    #[test]
    fn set_quota_records_error() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "minimax");

        let mk = MasterKey::generate();
        let id = create(
            &conn,
            &ProviderId::new("minimax"),
            Some("sk-x"),
            &mk,
            None,
            100,
            None,
        )
        .expect("create");

        // A failed quota fetch: all numeric fields stay None, the
        // error message is stamped on, and last_fetched_at is set so
        // the UI can distinguish "tried" from "never tried".
        let q = crate::quota::AccountQuota {
            session_used: None,
            session_limit: None,
            session_reset_at: None,
            weekly_used: None,
            weekly_limit: None,
            weekly_reset_at: None,
            plan_name: None,
            last_fetched_at: "1700000099".into(),
            fetch_error: Some("minimax 401".into()),
            model_details: None,
        };
        set_quota(&conn, id, &q).expect("set_quota");

        let a = get(&conn, id, &mk).expect("get").expect("present");
        assert_eq!(a.quota_fetch_error.as_deref(), Some("minimax 401"));
        assert_eq!(a.quota_last_fetched_at.as_deref(), Some("1700000099"));
        assert!(a.quota_session_used.is_none());
    }

    #[test]
    fn set_quota_missing_account_errors() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "minimax");

        let q = crate::quota::AccountQuota {
            session_used: None,
            session_limit: None,
            session_reset_at: None,
            weekly_used: None,
            weekly_limit: None,
            weekly_reset_at: None,
            plan_name: None,
            last_fetched_at: "0".into(),
            fetch_error: None,
            model_details: None,
        };
        let err = set_quota(&conn, AccountId(99999), &q).expect_err("missing");
        assert!(matches!(err, CoreError::AccountNotFound(99999)));
    }

    #[test]
    fn health_status_parse_roundtrip() {
        for (variant, s) in [
            (HealthStatus::Healthy, "healthy"),
            (HealthStatus::Degraded, "degraded"),
            (HealthStatus::Unhealthy, "unhealthy"),
        ] {
            assert_eq!(variant.as_str(), s);
            assert_eq!(HealthStatus::parse(s).expect("parse"), variant);
        }
        assert!(HealthStatus::parse("bogus").is_err());
    }

    // =====================================================================
    // OAuth token encrypt/decrypt roundtrip tests
    // =====================================================================

    #[test]
    fn oauth_access_token_roundtrip() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");

        let mk = MasterKey::generate();
        let id = create(
            &conn,
            &ProviderId::new("openrouter"),
            None,
            &mk,
            None,
            10,
            None,
        )
        .expect("create");

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
        .expect("store");

        let decrypted = decrypt_access_token(&conn, id, &mk).expect("decrypt");
        assert_eq!(decrypted, access);
    }

    #[test]
    fn oauth_refresh_token_roundtrip() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");

        let mk = MasterKey::generate();
        let id = create(
            &conn,
            &ProviderId::new("openrouter"),
            None,
            &mk,
            None,
            10,
            None,
        )
        .expect("create");

        let access = "ya29.access";
        let refresh = "1//0test-refresh-token_xyz";
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
        .expect("store");

        let decrypted_rt = decrypt_refresh_token(&conn, id, &mk).expect("decrypt refresh");
        assert_eq!(decrypted_rt.as_deref(), Some(refresh));
    }

    #[test]
    fn oauth_no_refresh_token_returns_none() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");

        let mk = MasterKey::generate();
        let id = create(
            &conn,
            &ProviderId::new("openrouter"),
            None,
            &mk,
            None,
            10,
            None,
        )
        .expect("create");

        // Store with no refresh token.
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
        .expect("store");

        let rt = decrypt_refresh_token(&conn, id, &mk).expect("decrypt");
        assert!(rt.is_none());
    }

    #[test]
    fn oauth_empty_access_token() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");

        let mk = MasterKey::generate();
        let id = create(
            &conn,
            &ProviderId::new("openrouter"),
            None,
            &mk,
            None,
            10,
            None,
        )
        .expect("create");

        store_oauth_tokens(
            &conn,
            id,
            &mk,
            StoreOAuthTokensParams {
                access_token: "",
                token_type: "Bearer",
                ..Default::default()
            },
        )
        .expect("store empty access token");

        let decrypted = decrypt_access_token(&conn, id, &mk).expect("decrypt");
        assert_eq!(decrypted, "");
    }

    #[test]
    fn oauth_empty_refresh_token() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");

        let mk = MasterKey::generate();
        let id = create(
            &conn,
            &ProviderId::new("openrouter"),
            None,
            &mk,
            None,
            10,
            None,
        )
        .expect("create");

        store_oauth_tokens(
            &conn,
            id,
            &mk,
            StoreOAuthTokensParams {
                access_token: "access",
                refresh_token: Some(""),
                token_type: "Bearer",
                ..Default::default()
            },
        )
        .expect("store");

        let rt = decrypt_refresh_token(&conn, id, &mk).expect("decrypt");
        assert_eq!(rt.as_deref(), Some(""));
    }

    #[test]
    fn oauth_very_long_tokens() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");

        let mk = MasterKey::generate();
        let id = create(
            &conn,
            &ProviderId::new("openrouter"),
            None,
            &mk,
            None,
            10,
            None,
        )
        .expect("create");

        let long_access = "a".repeat(10_000);
        let long_refresh = "r".repeat(10_000);
        store_oauth_tokens(
            &conn,
            id,
            &mk,
            StoreOAuthTokensParams {
                access_token: &long_access,
                refresh_token: Some(&long_refresh),
                token_type: "Bearer",
                ..Default::default()
            },
        )
        .expect("store long tokens");

        let decrypted_a = decrypt_access_token(&conn, id, &mk).expect("decrypt access");
        assert_eq!(decrypted_a, long_access);

        let decrypted_r = decrypt_refresh_token(&conn, id, &mk).expect("decrypt refresh");
        assert_eq!(decrypted_r.as_deref(), Some(long_refresh.as_str()));
    }

    #[test]
    fn oauth_unicode_tokens() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");

        let mk = MasterKey::generate();
        let id = create(
            &conn,
            &ProviderId::new("openrouter"),
            None,
            &mk,
            None,
            10,
            None,
        )
        .expect("create");

        let unicode_access = "tok_日本語🔑_emoji_🎉_ñ";
        store_oauth_tokens(
            &conn,
            id,
            &mk,
            StoreOAuthTokensParams {
                access_token: unicode_access,
                token_type: "Bearer",
                ..Default::default()
            },
        )
        .expect("store unicode");

        let decrypted = decrypt_access_token(&conn, id, &mk).expect("decrypt");
        assert_eq!(decrypted, unicode_access);
    }

    #[test]
    fn oauth_wrong_key_fails_decrypt() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");

        let mk = MasterKey::generate();
        let id = create(
            &conn,
            &ProviderId::new("openrouter"),
            None,
            &mk,
            None,
            10,
            None,
        )
        .expect("create");

        store_oauth_tokens(
            &conn,
            id,
            &mk,
            StoreOAuthTokensParams {
                access_token: "secret-token",
                refresh_token: Some("secret-refresh"),
                token_type: "Bearer",
                ..Default::default()
            },
        )
        .expect("store");

        let wrong_mk = MasterKey::generate();
        let err = decrypt_access_token(&conn, id, &wrong_mk).unwrap_err();
        assert!(matches!(err, CoreError::Internal(_)));
    }

    #[test]
    fn oauth_access_token_on_missing_account() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        let mk = MasterKey::generate();
        let err = decrypt_access_token(&conn, AccountId(99999), &mk).unwrap_err();
        assert!(matches!(err, CoreError::AccountNotFound(99999)));
    }

    #[test]
    fn oauth_refresh_token_on_missing_account() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        let mk = MasterKey::generate();
        let err = decrypt_refresh_token(&conn, AccountId(99999), &mk).unwrap_err();
        assert!(matches!(err, CoreError::AccountNotFound(99999)));
    }

    #[test]
    fn oauth_store_tokens_on_missing_account() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        let mk = MasterKey::generate();
        let err = store_oauth_tokens(
            &conn,
            AccountId(99999),
            &mk,
            StoreOAuthTokensParams {
                access_token: "access",
                token_type: "Bearer",
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(err, CoreError::AccountNotFound(99999)));
    }

    #[test]
    fn oauth_replacing_tokens_overwrites_old() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");

        let mk = MasterKey::generate();
        let id = create(
            &conn,
            &ProviderId::new("openrouter"),
            None,
            &mk,
            None,
            10,
            None,
        )
        .expect("create");

        // First store.
        store_oauth_tokens(
            &conn,
            id,
            &mk,
            StoreOAuthTokensParams {
                access_token: "old-access",
                refresh_token: Some("old-refresh"),
                token_type: "Bearer",
                ..Default::default()
            },
        )
        .expect("store1");

        // Overwrite.
        store_oauth_tokens(
            &conn,
            id,
            &mk,
            StoreOAuthTokensParams {
                access_token: "new-access",
                refresh_token: Some("new-refresh"),
                token_type: "Bearer",
                ..Default::default()
            },
        )
        .expect("store2");

        assert_eq!(decrypt_access_token(&conn, id, &mk).unwrap(), "new-access");
        assert_eq!(
            decrypt_refresh_token(&conn, id, &mk).unwrap().as_deref(),
            Some("new-refresh")
        );
    }

    #[test]
    fn store_oauth_tokens_defaults_expires_at_when_none() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");

        let mk = MasterKey::generate();
        let id = create(
            &conn,
            &ProviderId::new("openrouter"),
            None,
            &mk,
            None,
            10,
            None,
        )
        .expect("create");

        // Store with expires_at = None.
        store_oauth_tokens(
            &conn,
            id,
            &mk,
            StoreOAuthTokensParams {
                access_token: "access-token",
                refresh_token: Some("refresh-token"),
                token_type: "Bearer",
                ..Default::default()
            },
        )
        .expect("store");

        // The account should now have a non-NULL expires_at ~1 hour in the future.
        let acc = get(&conn, id, &mk).expect("get").expect("present");
        let expires = acc.expires_at.expect("expires_at should be populated");
        let parsed = openproxy_types::timestamp::parse_timestamp(&expires)
            .expect("valid ISO-8601")
            .with_timezone(&chrono::Utc);
        let now = chrono::Utc::now();
        let diff = parsed.signed_duration_since(now);
        assert!(
            diff.num_seconds() > 3500 && diff.num_seconds() <= 3600,
            "expires_at should be ~1 hour from now, got {}s",
            diff.num_seconds()
        );
    }

    #[test]
    fn store_oauth_tokens_preserves_explicit_expires_at() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");

        let mk = MasterKey::generate();
        let id = create(
            &conn,
            &ProviderId::new("openrouter"),
            None,
            &mk,
            None,
            10,
            None,
        )
        .expect("create");

        let explicit = "2099-01-01T00:00:00Z";
        store_oauth_tokens(
            &conn,
            id,
            &mk,
            StoreOAuthTokensParams {
                access_token: "access-token",
                refresh_token: Some("refresh-token"),
                token_type: "Bearer",
                expires_at: Some(explicit),
                ..Default::default()
            },
        )
        .expect("store");

        let acc = get(&conn, id, &mk).expect("get").expect("present");
        assert_eq!(acc.expires_at.as_deref(), Some(explicit));
    }

    #[test]
    fn decrypt_api_key_on_oauth_account_returns_validation_error() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");

        // OAuth account: api_key = None → api_key_encrypted = NULL in DB.
        let mk = MasterKey::generate();
        let id = create(
            &conn,
            &ProviderId::new("openrouter"),
            None,
            &mk,
            None,
            10,
            None,
        )
        .expect("create");

        let err = decrypt_api_key(&conn, id, &mk).expect_err("OAuth account has no key");
        assert!(
            matches!(err, CoreError::Validation(ref msg) if msg.contains("no API key")),
            "expected Validation error about missing API key, got: {err:?}"
        );
    }

    #[test]
    fn update_api_key_roundtrip() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");

        let mk = MasterKey::generate();
        let id = create(
            &conn,
            &ProviderId::new("openrouter"),
            None,
            &mk,
            None,
            10,
            None,
        )
        .expect("create");

        // Initially no key (OAuth account).
        let err = decrypt_api_key(&conn, id, &mk).expect_err("no key yet");
        assert!(matches!(err, CoreError::Validation(_)));

        // Set a key.
        let key = "sk-updated-key-abc123";
        update_api_key(&conn, id, Some(key), &mk).expect("set key");
        let recovered = decrypt_api_key(&conn, id, &mk).expect("decrypt after set");
        assert_eq!(recovered, key);

        // Clear the key (back to OAuth).
        update_api_key(&conn, id, None, &mk).expect("clear key");
        let err = decrypt_api_key(&conn, id, &mk).expect_err("cleared");
        assert!(matches!(err, CoreError::Validation(_)));

        // Missing id → AccountNotFound.
        let err =
            update_api_key(&conn, AccountId(99999), Some("x"), &mk).expect_err("missing account");
        assert!(matches!(err, CoreError::AccountNotFound(99999)));
    }

    #[test]
    fn delete_account_nulls_combo_targets_fk() {
        // Bug: deleting an account that is referenced by combo_targets
        // failed with FOREIGN KEY constraint failed because the FK
        // on combo_targets.account_id does NOT have ON DELETE SET NULL.
        // Fix: accounts::delete now NULLs out combo_targets.account_id
        // before deleting the account row.
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "minimax");

        let mk = MasterKey::generate();
        let account_id = create(
            &conn,
            &ProviderId::new("minimax"),
            Some("sk-test-minimax"),
            &mk,
            Some("primary"),
            10,
            None,
        )
        .expect("create account");

        // Create a combo with a target that pins this account.
        conn.execute(
            "INSERT INTO combos (id, name, strategy, race_size) VALUES (1, 'test-combo', 'priority', 1)",
            [],
        )
        .expect("insert combo");
        conn.execute(
            "INSERT INTO combo_targets (id, combo_id, provider_id, account_id, upstream_model_id, priority_order) \
             VALUES (1, 1, 'minimax', ?1, 'model-1', 0)",
            params![account_id.0],
        )
        .expect("insert combo_target with account_id");

        // Verify the combo_target references the account.
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM combo_targets WHERE account_id = ?1",
                params![account_id.0],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "combo_target should reference the account");

        // Delete the account — must NOT fail with FK constraint error.
        delete(&conn, account_id).expect("delete account with combo_target reference");

        // The account is gone.
        let gone = get(&conn, account_id, &mk).expect("get").is_none();
        assert!(gone, "account should be deleted");

        // The combo_target still exists, but account_id is now NULL
        // (falls back to automatic account selection).
        let target_account_id: Option<i64> = conn
            .query_row(
                "SELECT account_id FROM combo_targets WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            target_account_id.is_none(),
            "combo_target.account_id should be NULL after account delete"
        );
    }

    #[test]
    fn oauth_store_preserves_existing_refresh_token_when_none() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");

        let mk = MasterKey::generate();
        let id = create(
            &conn,
            &ProviderId::new("openrouter"),
            None,
            &mk,
            None,
            10,
            None,
        )
        .expect("create");

        // 1. Initial store with a refresh token.
        let access1 = "access-1";
        let refresh1 = "refresh-1";
        store_oauth_tokens(
            &conn,
            id,
            &mk,
            StoreOAuthTokensParams {
                access_token: access1,
                refresh_token: Some(refresh1),
                token_type: "Bearer",
                provider_specific: Some("initial-provider-spec"),
                email: Some("user@domain.com"),
                ..Default::default()
            },
        )
        .expect("store initial");

        // 2. Perform a refresh passing None for refresh_token and other fields.
        let access2 = "access-2";
        store_oauth_tokens(
            &conn,
            id,
            &mk,
            StoreOAuthTokensParams {
                access_token: access2,
                token_type: "Bearer",
                ..Default::default()
            },
        )
        .expect("store refresh");

        // 3. Verify access_token is updated.
        let decrypted_at = decrypt_access_token(&conn, id, &mk).expect("decrypt access");
        assert_eq!(decrypted_at, access2);

        // 4. Verify refresh_token is preserved.
        let decrypted_rt = decrypt_refresh_token(&conn, id, &mk).expect("decrypt refresh");
        assert_eq!(decrypted_rt.as_deref(), Some(refresh1));

        // 5. Verify email and provider specific metadata are preserved.
        let acc = get(&conn, id, &mk).expect("get").expect("present");
        assert_eq!(acc.email.as_deref(), Some("user@domain.com"));
        assert_eq!(
            acc.oauth_provider_specific.as_deref(),
            Some("initial-provider-spec")
        );
    }
}
