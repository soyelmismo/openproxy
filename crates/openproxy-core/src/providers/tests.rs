use super::*;
use crate::error::CoreError;
use openproxy_db::conn::DbPool;
use std::path::PathBuf;

fn fresh_pool() -> (DbPool, PathBuf) {
    let pool = DbPool::test_pool_with_prefix("openproxy-providers-test").expect("open pool");
    let path = pool.path().to_path_buf();
    (pool, path)
}

fn np<'a>(id: &'a ProviderId, name: &'a str) -> NewProvider<'a> {
    NewProvider {
        id,
        name,
        base_url: "https://x.example",
        auth_type: AuthType::Bearer,
        format: ProviderFormat::Openai,
        extra_headers_json: None,
        auto_activate_keyword: None,
        rate_limit_scope: crate::providers::RateLimitScope::Account,
    }
}

#[test]
fn create_and_get() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    let id = ProviderId::new("openrouter");
    let mut p = np(&id, "OpenRouter");
    p.base_url = "https://openrouter.ai/api/v1";
    p.extra_headers_json = Some(r#"{"X-Title":"openproxy"}"#);
    p.auto_activate_keyword = Some("claude");
    create(&conn, p).expect("create");

    let got = get(&conn, &id).expect("get").expect("present");
    assert_eq!(got.id, id);
    assert_eq!(&*got.name, "OpenRouter");
    assert_eq!(&*got.base_url, "https://openrouter.ai/api/v1");
    assert_eq!(got.auth_type, AuthType::Bearer);
    assert_eq!(got.format, ProviderFormat::Openai);
    assert_eq!(
        got.extra_headers_json.as_deref(),
        Some(r#"{"X-Title":"openproxy"}"#)
    );
    assert_eq!(got.auto_activate_keyword.as_deref(), Some("claude"));
    assert!(!got.created_at.is_empty(), "created_at stamped by DB");
}

#[test]
fn create_duplicate_id_fails() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    let id = ProviderId::new("anthropic");
    let mut p = np(&id, "Anthropic");
    p.base_url = "https://api.anthropic.com";
    p.auth_type = AuthType::XApiKey;
    p.format = ProviderFormat::Anthropic;
    create(&conn, p).expect("first create");

    let err = create(&conn, np(&id, "Dup")).expect_err("duplicate must fail");
    match err {
        CoreError::Validation(msg) => assert_eq!(msg, "provider id already exists"),
        other => panic!("expected Validation, got {other:?}"),
    }
}

#[test]
fn list_returns_all() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    for (id, name) in [("a", "A"), ("b", "B"), ("c", "C")] {
        create(&conn, np(&ProviderId::new(id), name)).expect("create");
    }
    let all = list(&conn).expect("list");
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].id, ProviderId::new("a"));
    assert_eq!(all[1].id, ProviderId::new("b"));
    assert_eq!(all[2].id, ProviderId::new("c"));
}

#[test]
fn delete_removes_provider() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    let id = ProviderId::new("to-delete");
    create(&conn, np(&id, "X")).expect("create");

    conn.execute(
        "INSERT INTO accounts(provider_id, api_key_encrypted) VALUES (?1, ?2)",
        rusqlite::params![id.as_str(), &[1u8, 2, 3][..]],
    )
    .expect("seed account");
    let accounts_before: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM accounts WHERE provider_id = ?1",
            rusqlite::params![id.as_str()],
            |r| r.get(0),
        )
        .expect("count");
    assert_eq!(accounts_before, 1, "account seeded");

    delete(&conn, &id).expect("delete");
    assert!(get(&conn, &id).expect("get").is_none(), "provider gone");
    let accounts_after: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM accounts WHERE provider_id = ?1",
            rusqlite::params![id.as_str()],
            |r| r.get(0),
        )
        .expect("count");
    assert_eq!(accounts_after, 0, "FK cascade removed the account");
    delete(&conn, &id).expect("delete again is fine");
}

#[test]
fn update_modifies_fields() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    let id = ProviderId::new("upd");
    let mut p0 = np(&id, "Original");
    p0.base_url = "https://original.example";
    p0.extra_headers_json = Some(r#"{"old":true}"#);
    create(&conn, p0).expect("create");

    update(
        &conn,
        &id,
        UpdateProviderParams {
            name: Some("Renamed"),
            ..Default::default()
        },
    )
    .expect("update name");
    let p = get(&conn, &id).expect("get").expect("present");
    assert_eq!(&*p.name, "Renamed");
    assert_eq!(&*p.base_url, "https://original.example");
    assert_eq!(p.extra_headers_json.as_deref(), Some(r#"{"old":true}"#));
    assert_eq!(p.auto_activate_keyword, None);

    update(
        &conn,
        &id,
        UpdateProviderParams {
            base_url: Some("https://new.example"),
            extra_headers_json: Some(Some(r#"{"new":true}"#)),
            auto_activate_keyword: Some(Some("claude")),
            ..Default::default()
        },
    )
    .expect("update");
    let p = get(&conn, &id).expect("get").expect("present");
    assert_eq!(&*p.name, "Renamed");
    assert_eq!(&*p.base_url, "https://new.example");
    assert_eq!(p.extra_headers_json.as_deref(), Some(r#"{"new":true}"#));
    assert_eq!(p.auto_activate_keyword.as_deref(), Some("claude"));

    update(
        &conn,
        &id,
        UpdateProviderParams {
            auto_activate_keyword: Some(None),
            ..Default::default()
        },
    )
    .expect("clear keyword");
    let p = get(&conn, &id).expect("get").expect("present");
    assert_eq!(p.auto_activate_keyword, None);

    update(&conn, &id, UpdateProviderParams::default()).expect("no-op");
    let p = get(&conn, &id).expect("get").expect("present");
    assert_eq!(&*p.base_url, "https://new.example");

    let err = update(
        &conn,
        &ProviderId::new("nope"),
        UpdateProviderParams {
            name: Some("X"),
            ..Default::default()
        },
    )
    .expect_err("missing id");
    assert!(matches!(err, CoreError::ProviderNotFound(_)));
}

#[test]
fn provider_format_parse_roundtrip() {
    for (variant, s) in [
        (ProviderFormat::Openai, "openai"),
        (ProviderFormat::Anthropic, "anthropic"),
        (ProviderFormat::Mixed, "mixed"),
        (ProviderFormat::Gemini, "gemini"),
    ] {
        assert_eq!(variant.as_str(), s);
        assert_eq!(ProviderFormat::parse(s).expect("parse"), variant);
    }
    assert!(ProviderFormat::parse("bogus").is_err());
}

#[test]
fn auth_type_parse_roundtrip() {
    for (variant, s) in [
        (AuthType::Bearer, "bearer"),
        (AuthType::XApiKey, "x-api-key"),
        (AuthType::GoogApiKey, "goog-api-key"),
        (AuthType::OAuth, "oauth"),
        (AuthType::None, "none"),
    ] {
        assert_eq!(variant.as_str(), s);
        assert_eq!(AuthType::parse(s).expect("parse"), variant);
    }
    assert!(AuthType::parse("basic").is_err());
}

#[test]
fn new_providers_default_to_active() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    let id = ProviderId::new("active-by-default");
    create(&conn, np(&id, "X")).expect("create");
    let got = get(&conn, &id).expect("get").expect("present");
    assert!(got.active, "freshly created providers are active");
}

#[test]
fn set_active_flips_and_idempotent() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    let id = ProviderId::new("toggle");
    create(&conn, np(&id, "T")).expect("create");

    set_active(&conn, &id, false).expect("deactivate");
    let p = get(&conn, &id).expect("get").expect("present");
    assert!(!p.active, "deactivated");

    set_active(&conn, &id, false).expect("re-apply is a no-op");
    let p = get(&conn, &id).expect("get").expect("present");
    assert!(!p.active);

    set_active(&conn, &id, true).expect("reactivate");
    let p = get(&conn, &id).expect("get").expect("present");
    assert!(p.active, "reactivated");

    set_active(&conn, &ProviderId::new("does-not-exist"), false).expect("missing id is a no-op");
}

#[test]
fn list_active_filters_out_inactive() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    for (id, name) in [("a", "A"), ("b", "B"), ("c", "C")] {
        create(&conn, np(&ProviderId::new(id), name)).expect("create");
    }

    let active = list_active(&conn).expect("list active");
    assert_eq!(active.len(), 3);
    assert_eq!(list(&conn).expect("list").len(), 3);

    set_active(&conn, &ProviderId::new("b"), false).expect("deactivate b");
    let active = list_active(&conn).expect("list active");
    assert_eq!(active.len(), 2);
    let ids: Vec<&str> = active.iter().map(|p| p.id.as_str()).collect();
    assert!(ids.contains(&"a") && ids.contains(&"c") && !ids.contains(&"b"));

    let all = list(&conn).expect("list");
    assert_eq!(all.len(), 3);
    assert!(
        !all.iter()
            .find(|p| p.id == ProviderId::new("b"))
            .unwrap()
            .active
    );
}

#[test]
fn list_and_list_active_hide_virtual_combo_provider() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    for (id, name) in [("a", "A"), ("b", "B")] {
        create(&conn, np(&ProviderId::new(id), name)).expect("create");
    }
    crate::seed::seed_virtual_combo_provider(&conn).expect("seed virtual");

    let raw_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM providers", [], |r| r.get(0))
        .expect("count");
    assert_eq!(raw_count, 3);

    let all = list(&conn).expect("list");
    assert_eq!(all.len(), 2);
    assert!(
        all.iter()
            .all(|p| p.id.as_str() != crate::seed::VIRTUAL_COMBO_PROVIDER_ID)
    );

    let active = list_active(&conn).expect("list_active");
    assert_eq!(active.len(), 2);
    assert!(
        active
            .iter()
            .all(|p| p.id.as_str() != crate::seed::VIRTUAL_COMBO_PROVIDER_ID)
    );

    let got = get(
        &conn,
        &ProviderId::new(crate::seed::VIRTUAL_COMBO_PROVIDER_ID),
    )
    .expect("get")
    .expect("present");
    assert_eq!(got.id.as_str(), crate::seed::VIRTUAL_COMBO_PROVIDER_ID);
}
