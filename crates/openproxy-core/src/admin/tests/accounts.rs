use super::fresh_pool;
use crate::admin::accounts::{
    BulkCreateAccountItem, BulkCreateAccountsInput, CreateAccountInput, bulk_create_accounts,
    create_account, list_accounts,
};
use crate::admin::providers::{CreateProviderInput, create_provider};
use crate::error::CoreError;
use crate::ids::ProviderId;
use openproxy_db::secrets::MasterKey;

fn seed_prov(conn: &rusqlite::Connection, pid: &str, fmt: &str) {
    create_provider(
        conn,
        CreateProviderInput {
            rate_limit_scope: None,
            id: pid.into(),
            name: pid.into(),
            base_url: format!("https://api.{pid}.com"),
            auth_type: "bearer".into(),
            format: fmt.into(),
            extra_headers_json: None,
        },
    )
    .expect("seed provider");
}

fn cai(pid: &str, key: Option<&str>) -> CreateAccountInput {
    CreateAccountInput {
        provider_id: pid.into(),
        api_key: key.map(Into::into),
        label: None,
        priority: None,
        extra_config_json: None,
    }
}

fn bcai(key: &str, label: Option<&str>, priority: Option<i32>) -> BulkCreateAccountItem {
    BulkCreateAccountItem {
        api_key: key.into(),
        label: label.map(Into::into),
        priority,
        extra_config_json: None,
    }
}

#[test]
fn create_account_encrypts_and_lists() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    seed_prov(&conn, "openrouter", "openai");
    let mk = MasterKey::generate().unwrap();
    let plaintext = "sk-supersecret-DO-NOT-LEAK";

    let mut input = cai("openrouter", Some(plaintext));
    input.label = Some("primary".into());
    input.priority = Some(10);
    let id = create_account(&conn, &mk, input).expect("create account");

    let all = list_accounts(&conn, None, &mk).expect("list all");
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].id, id);
    assert_eq!(all[0].label.as_deref(), Some("primary"));
    assert_eq!(all[0].priority, 10);
    assert_eq!(
        list_accounts(&conn, Some(&ProviderId::new("openrouter")), &mk)
            .expect("list")
            .len(),
        1
    );

    let raw: Vec<u8> = conn
        .query_row(
            "SELECT api_key_encrypted FROM accounts WHERE id = ?1",
            rusqlite::params![id.0],
            |r| r.get(0),
        )
        .expect("raw select");
    assert!(!String::from_utf8_lossy(&raw).contains(plaintext));
    assert_eq!(
        crate::accounts::decrypt_api_key(&conn, id, &mk).expect("decrypt"),
        plaintext
    );

    let id2 = create_account(&conn, &mk, cai("openrouter", Some("sk-another")))
        .expect("create default-prio");
    let a = list_accounts(&conn, None, &mk)
        .expect("list")
        .into_iter()
        .find(|a| a.id == id2)
        .expect("present");
    assert_eq!(a.priority, 100);
    assert_eq!(a.label.as_deref(), Some("sk-another"));
}

#[test]
fn test_bulk_create_accounts() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    seed_prov(&conn, "anthropic", "anthropic");
    let mk = MasterKey::generate().unwrap();

    let items = vec![
        bcai("sk-ant-key1-secret123456789", Some("first-label"), Some(50)),
        bcai("sk-ant-key2-secret987654321", None, None),
        bcai("   ", None, None), // Should be skipped
    ];
    let created_ids = bulk_create_accounts(
        &conn,
        &mk,
        BulkCreateAccountsInput {
            provider_id: "anthropic".into(),
            items,
        },
    )
    .expect("bulk create");
    assert_eq!(created_ids.len(), 2);

    let list = list_accounts(&conn, Some(&ProviderId::new("anthropic")), &mk).expect("list");
    assert_eq!(list.len(), 2);
    let a1 = list.iter().find(|a| a.id == created_ids[0]).unwrap();
    assert_eq!(a1.label.as_deref(), Some("first-label"));
    assert_eq!(a1.priority, 50);
    let a2 = list.iter().find(|a| a.id == created_ids[1]).unwrap();
    assert_eq!(a2.priority, 100);

    let second_batch = vec![
        bcai("sk-ant-key1-secret123456789", None, None), // duplicate from DB
        bcai("sk-ant-key3-secret333333333", Some("key3"), None), // new
        bcai("sk-ant-key3-secret333333333", Some("key3-dup"), None), // duplicate in this batch!
    ];
    let second_ids = bulk_create_accounts(
        &conn,
        &mk,
        BulkCreateAccountsInput {
            provider_id: "anthropic".into(),
            items: second_batch,
        },
    )
    .expect("bulk 2nd");
    assert_eq!(second_ids.len(), 1);
    assert_eq!(
        list_accounts(&conn, Some(&ProviderId::new("anthropic")), &mk)
            .expect("list2")
            .len(),
        3
    );

    let err = create_account(
        &conn,
        &mk,
        cai("anthropic", Some("sk-ant-key1-secret123456789")),
    )
    .unwrap_err();
    assert!(matches!(err, CoreError::Validation(_)));
}
