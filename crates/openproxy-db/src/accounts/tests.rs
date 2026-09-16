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
