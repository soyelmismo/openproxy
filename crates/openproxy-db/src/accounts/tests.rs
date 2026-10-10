use super::*;
use crate::secrets::MasterKey;
use crate::test_db::{open_in_memory, seed_antigravity_provider};
use openproxy_types::Result;

#[test]
fn test_summarize_api_key() {
    for (k, expected) in [
        ("sk-12345", "sk-12345"),
        ("1234567890", "1234567890"),
        ("sk-proj-1234567890abcdef", "sk-pro...cdef"),
        ("AIzaSyD1234567890abcdefgh", "AIzaSy...efgh"),
    ] {
        assert_eq!(summarize_api_key(k), expected);
    }
}

#[test]
fn read_provider_meta_and_project_cases() {
    let cases = [
        (
            Some(r#"{"projectId":"my-proj"}"#),
            Some("my-proj"),
            Some("my-proj"),
        ),
        (
            Some(r#"{"project_id":"my-proj-snake"}"#),
            Some("my-proj-snake"),
            Some("my-proj-snake"),
        ),
        (None, None, None),
        (Some(r#"{"unrelated":"x"}"#), None, None),
        (Some(r#"{"projectId":""}"#), Some(""), None),
        (Some(r#"{"projectId":"   "}"#), Some("   "), None),
    ];

    for (json, exp_meta, exp_proj) in cases {
        let conn = open_in_memory();
        seed_antigravity_provider(&conn);
        conn.execute("INSERT INTO accounts (provider_id, oauth_provider_specific) VALUES ('antigravity', ?1)", rusqlite::params![json]).unwrap();

        let meta: Option<AntigravityMeta> = read_provider_meta(&conn, None, 1).unwrap();
        assert_eq!(
            meta.as_ref().and_then(|m| m.project_id.as_deref()),
            exp_meta
        );
        assert_eq!(
            read_antigravity_project(&conn, None, 1).unwrap().as_deref(),
            exp_proj
        );
    }
}

#[test]
fn read_provider_meta_works_with_other_structs() {
    #[derive(serde::Deserialize, Debug, PartialEq)]
    struct KiroMeta {
        region: Option<String>,
    }

    let conn = open_in_memory();
    seed_antigravity_provider(&conn);
    conn.execute(
        "INSERT INTO accounts (provider_id, oauth_provider_specific) VALUES ('antigravity', ?1)",
        [Some(r#"{"region":"us-east-1"}"#)],
    )
    .unwrap();
    let meta: Option<KiroMeta> = read_provider_meta(&conn, None, 1).unwrap();
    assert_eq!(
        meta,
        Some(KiroMeta {
            region: Some("us-east-1".into())
        })
    );
}

#[test]
fn read_provider_meta_aes_encryption_and_decrypt_failure() {
    let conn = open_in_memory();
    seed_antigravity_provider(&conn);
    let master = MasterKey::generate().unwrap();
    let blob = master
        .encrypt(r#"{"project_id":"encrypted-proj"}"#)
        .unwrap();
    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &blob);
    conn.execute(
        "INSERT INTO accounts (provider_id, oauth_provider_specific) VALUES ('antigravity', ?1)",
        [Some(b64)],
    )
    .unwrap();

    let meta: Option<AntigravityMeta> = read_provider_meta(&conn, Some(&master), 1).unwrap();
    assert_eq!(
        meta.and_then(|m| m.project_id).as_deref(),
        Some("encrypted-proj")
    );

    let garbage =
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"not valid aes");
    conn.execute(
        "INSERT INTO accounts (provider_id, oauth_provider_specific) VALUES ('antigravity', ?1)",
        [Some(garbage)],
    )
    .unwrap();
    let res: Result<Option<AntigravityMeta>> = read_provider_meta(&conn, Some(&master), 2);
    assert!(res.unwrap().is_none());
}

#[test]
fn read_provider_meta_batch_returns_map_for_valid_ids() {
    use std::collections::HashMap;
    let conn = open_in_memory();
    seed_antigravity_provider(&conn);
    conn.execute("INSERT INTO accounts (provider_id, oauth_provider_specific) VALUES ('antigravity', ?1), ('antigravity', ?2), ('antigravity', NULL)", [Some(r#"{"projectId":"a"}"#), Some(r#"{"project_id":"b"}"#)]).unwrap();

    let map: HashMap<i64, AntigravityMeta> =
        read_provider_meta_batch(&conn, None, &[1, 2, 3, 99]).unwrap();
    assert_eq!(map.len(), 2);
    assert_eq!(
        map.get(&1).and_then(|m| m.project_id.clone()).as_deref(),
        Some("a")
    );
    assert_eq!(
        map.get(&2).and_then(|m| m.project_id.clone()).as_deref(),
        Some("b")
    );
    assert!(!map.contains_key(&3));
    assert!(!map.contains_key(&99));
}

#[test]
fn update_antigravity_project_id_round_trips() {
    let conn = open_in_memory();
    seed_antigravity_provider(&conn);
    conn.execute(
        "INSERT INTO accounts (provider_id, oauth_provider_specific) VALUES ('antigravity', ?1)",
        [Some(r#"{"projectId":"old"}"#)],
    )
    .unwrap();
    update_antigravity_project_id(&conn, 1, "new").unwrap();

    assert_eq!(
        read_antigravity_project(&conn, None, 1).unwrap().as_deref(),
        Some("new")
    );
    let generic: Option<AntigravityMeta> = read_provider_meta(&conn, None, 1).unwrap();
    assert_eq!(generic.and_then(|m| m.project_id).as_deref(), Some("new"));

    let raw: Option<Option<String>> = conn
        .query_row(
            "SELECT oauth_provider_specific FROM accounts WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let val: serde_json::Value = serde_json::from_str(&raw.flatten().unwrap()).unwrap();
    assert!(val.get("projectId").is_none());
    assert_eq!(val["project_id"], "new");
}

#[test]
fn test_account_select_canonical_projection() {
    let conn = open_in_memory();
    seed_antigravity_provider(&conn);
    let master = MasterKey::generate().unwrap();

    let provider = openproxy_types::ProviderId::new("antigravity");
    let acc_id = create(
        &conn,
        &provider,
        Some("sk-test-secret-key"),
        &master,
        Some("test-label"),
        42,
        Some(r#"{"custom_key":"custom_value"}"#),
    )
    .unwrap();

    conn.execute(
        "INSERT OR IGNORE INTO free_proxies (id, host, port, type) VALUES ('proxy-node-1', '127.0.0.1', 8080, 'http')",
        [],
    )
    .ok();

    store_oauth_tokens(
        &conn,
        acc_id,
        &master,
        openproxy_types::accounts::StoreOAuthTokensParams {
            access_token: "test-access-token",
            refresh_token: Some("test-refresh-token"),
            token_type: "Bearer",
            expires_at: Some("2026-10-01T22:00:00Z"),
            scope: Some("openid profile email"),
            provider_specific: Some(r#"{"project_id":"canonical-test"}"#),
            email: Some("developer@example.com"),
        },
    )
    .unwrap();

    update_current_proxy(&conn, acc_id, Some("proxy-node-1")).unwrap();

    let quota = openproxy_types::quota::AccountQuota {
        session_used: Some(150),
        session_limit: Some(1000),
        session_reset_at: Some("2026-10-01T23:00:00Z".into()),
        weekly_used: Some(500),
        weekly_limit: Some(5000),
        weekly_reset_at: Some("2026-10-08T00:00:00Z".into()),
        plan_name: Some("Pro-Tier".into()),
        last_fetched_at: "2026-10-01T21:00:00Z".into(),
        fetch_error: None,
        model_details: None,
        pools: None,
    };
    set_quota(&conn, acc_id, &quota).unwrap();

    let acc_get = get(&conn, acc_id, &master)
        .unwrap()
        .expect("account exists");
    let accs_exp = list_expiring_oauth_accounts(&conn, 86400 * 365, &master).unwrap();
    assert_eq!(accs_exp.len(), 1);
    let acc_oauth = &accs_exp[0];

    assert_eq!(acc_get.id, acc_id);
    assert_eq!(acc_get.id, acc_oauth.id);
    assert_eq!(acc_get.provider_id.as_str(), "antigravity");
    assert_eq!(acc_get.provider_id, acc_oauth.provider_id);
    assert_eq!(acc_get.label.as_deref(), Some("test-label"));
    assert_eq!(acc_get.label, acc_oauth.label);
    assert_eq!(acc_get.priority, 42);
    assert_eq!(acc_get.priority, acc_oauth.priority);
    assert_eq!(
        acc_get.extra_config_json.as_deref(),
        Some(r#"{"custom_key":"custom_value"}"#)
    );
    assert_eq!(acc_get.extra_config_json, acc_oauth.extra_config_json);
    assert_eq!(
        acc_get.health_status,
        openproxy_types::HealthStatus::Healthy
    );
    assert_eq!(acc_get.health_status, acc_oauth.health_status);
    assert_eq!(acc_get.rate_limited_until, None);
    assert_eq!(acc_get.quota_session_used, Some(150));
    assert_eq!(acc_get.quota_session_used, acc_oauth.quota_session_used);
    assert_eq!(acc_get.quota_session_limit, Some(1000));
    assert_eq!(acc_get.quota_session_limit, acc_oauth.quota_session_limit);
    assert_eq!(acc_get.quota_weekly_used, Some(500));
    assert_eq!(acc_get.quota_weekly_used, acc_oauth.quota_weekly_used);
    assert_eq!(acc_get.quota_weekly_limit, Some(5000));
    assert_eq!(acc_get.quota_weekly_limit, acc_oauth.quota_weekly_limit);
    assert_eq!(acc_get.quota_plan_name.as_deref(), Some("Pro-Tier"));
    assert_eq!(acc_get.quota_plan_name, acc_oauth.quota_plan_name);
    assert_eq!(acc_get.auth_type.as_ref(), "oauth");
    assert_eq!(acc_get.auth_type, acc_oauth.auth_type);
    assert_eq!(acc_get.email.as_deref(), Some("developer@example.com"));
    assert_eq!(acc_get.email, acc_oauth.email);
    assert_eq!(acc_get.oauth_scope.as_deref(), Some("openid profile email"));
    assert_eq!(acc_get.oauth_scope, acc_oauth.oauth_scope);
    assert_eq!(
        acc_get.oauth_provider_specific.as_deref(),
        Some(r#"{"project_id":"canonical-test"}"#)
    );
    assert_eq!(
        acc_get.oauth_provider_specific,
        acc_oauth.oauth_provider_specific
    );
    assert_eq!(acc_get.expires_at.as_deref(), Some("2026-10-01T22:00:00Z"));
    assert_eq!(acc_get.created_at, acc_oauth.created_at);

    clear_oauth_expires_at(&conn, acc_id.0).expect("clear succeeds");
    let acc_cleared = get(&conn, acc_id, &master).unwrap().unwrap();
    assert_eq!(acc_cleared.expires_at, None);
}

// ===== quota_pools persistence (migration 000089) =====

use openproxy_types::AccountId;
use openproxy_types::quota::{
    AccountQuota, QuotaPool, QuotaPoolStatus, QuotaSource, zai_remaining_fraction,
};

const NOW: u64 = 1_700_000_000;

fn zai_account() -> (rusqlite::Connection, crate::secrets::MasterKey, AccountId) {
    let conn = open_in_memory();
    seed_antigravity_provider(&conn);
    let master = MasterKey::generate().unwrap();
    let id = create(
        &conn,
        &openproxy_types::ProviderId::new("antigravity"),
        None,
        &master,
        Some("zai-account"),
        10,
        None,
    )
    .unwrap();
    (conn, master, id)
}

fn starter_pool(remaining: Option<i64>, status: QuotaPoolStatus) -> QuotaPool {
    QuotaPool {
        id: "starter-glm".into(),
        source: QuotaSource::ZcodeStarter,
        plan_name: Some("ZCode Starter".into()),
        status,
        unit: "requests".into(),
        used: Some(100 - remaining.unwrap_or(0)),
        limit: Some(100),
        remaining,
        reset_at: None,
        expires_at: None,
        starts_at: None,
        model_ids: vec!["glm-4.6".into()],
        model_details: None,
        fetch_error: None,
        last_fetched_at: "1700000000".into(),
    }
}

fn coding_pool(remaining: Option<i64>, status: QuotaPoolStatus) -> QuotaPool {
    QuotaPool {
        id: "coding-plan".into(),
        source: QuotaSource::CodingPlan,
        plan_name: Some("Coding Plan Pro".into()),
        status,
        unit: "requests".into(),
        used: Some(1000 - remaining.unwrap_or(0)),
        limit: Some(1000),
        remaining,
        reset_at: None,
        expires_at: None,
        starts_at: None,
        model_ids: Vec::new(),
        model_details: None,
        fetch_error: None,
        last_fetched_at: "1700000000".into(),
    }
}

#[test]
fn migration_000089_adds_quota_pools_column() {
    let conn = open_in_memory();
    let has: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('accounts') WHERE name = 'quota_pools'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(has, 1, "accounts.quota_pools must exist");

    let versions: Vec<i64> = {
        let mut stmt = conn
            .prepare("SELECT version FROM schema_migrations WHERE version = 89")
            .unwrap();
        let rows = stmt.query_map([], |r| r.get(0)).unwrap();
        rows.map(|r| r.unwrap()).collect()
    };
    assert_eq!(
        versions,
        vec![89],
        "migration 89 must be embedded and applied"
    );
}

#[test]
fn set_quota_roundtrips_both_pools_through_get_and_list() {
    let (conn, master, id) = zai_account();
    let pools: Vec<QuotaPool> = vec![
        starter_pool(Some(40), QuotaPoolStatus::Active),
        coding_pool(Some(250), QuotaPoolStatus::Active),
    ];
    let q = AccountQuota {
        session_used: Some(7),
        session_limit: Some(100),
        pools: Some(pools.into_boxed_slice()),
        ..AccountQuota::empty()
    };
    set_quota(&conn, id, &q).unwrap();

    // get(): projection index 24 must land in quota_pools.
    let acc = get(&conn, id, &master).unwrap().unwrap();
    let stored = acc.quota_pools.as_deref().expect("pools present after get");
    assert_eq!(stored.len(), 2);
    assert_eq!(stored[0].source, QuotaSource::ZcodeStarter);
    assert_eq!(stored[0].remaining, Some(40));
    assert_eq!(stored[1].source, QuotaSource::CodingPlan);
    assert_eq!(stored[1].limit, Some(1000));
    // Legacy aggregate columns untouched.
    assert_eq!(acc.quota_session_used, Some(7));

    // list(): same projection, so the mapper index must not have drifted.
    let listed = list(&conn, None, &master).unwrap();
    let listed = listed
        .into_iter()
        .find(|a| a.id == id)
        .expect("account in list");
    assert_eq!(listed.quota_pools, acc.quota_pools);

    // The two pools stay independent: the starter's half-consumed bucket does
    // not pull the coding plan's 0.25 down to 0.2.
    let frac = zai_remaining_fraction(stored, "glm-4.6", NOW);
    assert!((frac.unwrap() - 0.4).abs() < 1e-9, "got {frac:?}");

    // And the starter is authoritative for its model only.
    let other = zai_remaining_fraction(stored, "other-model", NOW);
    assert!((other.unwrap() - 0.25).abs() < 1e-9, "got {other:?}");
}

#[test]
fn account_without_pools_deserializes_as_none() {
    let (conn, master, id) = zai_account();
    let q = AccountQuota {
        session_used: Some(3),
        session_limit: Some(10),
        ..AccountQuota::empty()
    };
    set_quota(&conn, id, &q).unwrap();

    let acc = get(&conn, id, &master).unwrap().unwrap();
    assert!(
        acc.quota_pools.is_none(),
        "legacy row must read pools = None"
    );

    // A pre-000089 row (column added by ALTER, therefore NULL) is the same case:
    // serialize the account and confirm `quota_pools` is skipped, not emitted.
    let json = serde_json::to_string(&acc).unwrap();
    assert!(!json.contains("quota_pools"), "must skip None: {json}");
}

#[test]
fn exhausted_starter_does_not_retire_the_coding_plan() {
    let (conn, master, id) = zai_account();
    let pools: Vec<QuotaPool> = vec![
        starter_pool(Some(0), QuotaPoolStatus::Exhausted),
        coding_pool(Some(500), QuotaPoolStatus::Active),
    ];
    let q = AccountQuota {
        pools: Some(pools.into_boxed_slice()),
        ..AccountQuota::empty()
    };
    set_quota(&conn, id, &q).unwrap();

    let acc = get(&conn, id, &master).unwrap().unwrap();
    let stored = acc.quota_pools.as_deref().unwrap();
    let frac = zai_remaining_fraction(stored, "glm-4.6", NOW);
    assert!((frac.unwrap() - 0.5).abs() < 1e-9, "got {frac:?}");
}

#[test]
fn unreadable_source_stays_unknown_not_zero() {
    let (conn, master, id) = zai_account();
    let mut unreadable = coding_pool(None, QuotaPoolStatus::Unavailable);
    unreadable.fetch_error = Some("upstream 500".into());
    let q = AccountQuota {
        pools: Some(vec![unreadable].into_boxed_slice()),
        ..AccountQuota::empty()
    };
    set_quota(&conn, id, &q).unwrap();

    let acc = get(&conn, id, &master).unwrap().unwrap();
    let stored = acc.quota_pools.as_deref().unwrap();
    assert_eq!(
        zai_remaining_fraction(stored, "glm-4.6", NOW),
        None,
        "a fetch failure is unknown, not exhausted"
    );
}

#[test]
fn empty_pools_are_stored_as_null() {
    let (conn, master, id) = zai_account();
    let q = AccountQuota {
        pools: Some(Vec::new().into_boxed_slice()),
        ..AccountQuota::empty()
    };
    set_quota(&conn, id, &q).unwrap();

    let raw: Option<String> = conn
        .query_row(
            "SELECT quota_pools FROM accounts WHERE id = ?1",
            rusqlite::params![id.0],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(raw, None, "empty payload collapses to NULL on disk");
    let acc = get(&conn, id, &master).unwrap().unwrap();
    assert!(acc.quota_pools.is_none());
}

#[test]
fn set_quota_clears_pools_without_touching_other_accounts() {
    let (conn, master, id_a) = zai_account();
    let id_b = create(
        &conn,
        &openproxy_types::ProviderId::new("antigravity"),
        None,
        &master,
        Some("other"),
        20,
        None,
    )
    .unwrap();

    let q = AccountQuota {
        pools: Some(vec![starter_pool(Some(50), QuotaPoolStatus::Active)].into_boxed_slice()),
        ..AccountQuota::empty()
    };
    set_quota(&conn, id_a, &q).unwrap();

    // Explicit empty pool snapshot clears A only; missing pools is legacy data.
    set_quota(
        &conn,
        id_a,
        &AccountQuota {
            pools: Some(Vec::new().into_boxed_slice()),
            ..AccountQuota::empty()
        },
    )
    .unwrap();
    let a = get(&conn, id_a, &master).unwrap().unwrap();
    assert!(a.quota_pools.is_none());
    let b = get(&conn, id_b, &master).unwrap().unwrap();
    assert!(b.quota_pools.is_none());

    // Unknown account still reports not-found rather than silently succeeding.
    let err = set_quota(&conn, AccountId(9_999_999), &AccountQuota::empty()).unwrap_err();
    assert!(matches!(
        err,
        openproxy_types::CoreError::AccountNotFound(_)
    ));
}
