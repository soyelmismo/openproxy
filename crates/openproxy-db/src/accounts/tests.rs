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
    assert_eq!(acc_get.expires_at, acc_oauth.expires_at);
    assert_eq!(acc_get.current_proxy_id.as_deref(), Some("proxy-node-1"));
    assert_eq!(acc_get.current_proxy_id, acc_oauth.current_proxy_id);
    assert_eq!(acc_get.created_at, acc_oauth.created_at);
}
