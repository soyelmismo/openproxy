use super::fresh_pool;
use crate::admin::providers::{
    CreateProviderInput, UpdateProviderInput, create_provider, delete_provider, list_providers,
    update_provider,
};
use crate::error::CoreError;
use crate::ids::ProviderId;
use crate::providers::{AuthType, ProviderFormat};

fn cp(id: &str, name: &str) -> CreateProviderInput {
    CreateProviderInput {
        rate_limit_scope: None,
        id: id.into(),
        name: name.into(),
        base_url: "https://example.com".into(),
        auth_type: "bearer".into(),
        format: "openai".into(),
        extra_headers_json: None,
    }
}

#[test]
fn create_provider_then_list() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    let mut input = cp("openrouter", "OpenRouter");
    input.base_url = "https://openrouter.ai/api/v1".into();
    input.extra_headers_json = Some(r#"{"X-Title":"openproxy"}"#.into());
    let id = create_provider(&conn, input).expect("create provider");
    assert_eq!(id, ProviderId::new("openrouter"));

    let listed = list_providers(&conn).expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, id);
    assert_eq!(listed[0].auth_type, AuthType::Bearer);
    assert_eq!(listed[0].format, ProviderFormat::Openai);

    let mut bad = cp("bad", "x");
    bad.auth_type = "magic".into();
    assert!(matches!(
        create_provider(&conn, bad).expect_err("invalid auth_type"),
        CoreError::Validation(_)
    ));
}

#[test]
fn test_quota_capability_anti_drift() {
    let adapters = openproxy_adapters::adapters::builtin_adapters();
    let quota_ids = [
        "minimax",
        "minimax-cn",
        "antigravity",
        "agy",
        "codex",
        "kiro",
        "horde",
        "commandcodego",
    ];
    for adapter in adapters {
        let id = adapter.id().as_str();
        let meta = adapter.metadata();
        let expected = quota_ids.contains(&id);
        assert_eq!(
            meta.supports_quota, expected,
            "supports_quota mismatch for {id}"
        );
        assert_eq!(
            meta.quota_refresh_supported, expected,
            "quota_refresh mismatch for {id}"
        );
    }
}

#[test]
fn update_provider_changes_name_and_keyword() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    create_provider(&conn, cp("p", "Original")).expect("seed");

    update_provider(
        &conn,
        &ProviderId::new("p"),
        &UpdateProviderInput {
            name: Some("Renamed".into()),
            auto_activate_keyword: Some(Some("claude".into())),
            ..Default::default()
        },
    )
    .expect("update");
    let p = list_providers(&conn).expect("list").pop().expect("present");
    assert_eq!(&*p.name, "Renamed");
    assert_eq!(p.auto_activate_keyword.as_deref(), Some("claude"));

    update_provider(
        &conn,
        &ProviderId::new("p"),
        &UpdateProviderInput {
            auto_activate_keyword: Some(None),
            ..Default::default()
        },
    )
    .expect("clear");
    let p = list_providers(&conn).expect("list").pop().expect("present");
    assert_eq!(p.auto_activate_keyword, None);

    assert!(matches!(
        update_provider(
            &conn,
            &ProviderId::new("nope"),
            &UpdateProviderInput::default()
        )
        .expect_err("missing"),
        CoreError::ProviderNotFound(_)
    ));
}

#[test]
fn update_provider_patch_sets_notif_keyword_only() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    create_provider(&conn, cp("p", "Original")).expect("seed");

    let sel = || {
        conn.query_row(
            "SELECT notif_keyword_only FROM providers WHERE id = 'p'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .expect("select")
    };
    assert_eq!(sel(), 0);

    update_provider(
        &conn,
        &ProviderId::new("p"),
        &UpdateProviderInput {
            notif_keyword_only: Some(Some(true)),
            ..Default::default()
        },
    )
    .expect("patch on");
    assert_eq!(sel(), 1);
    assert_eq!(
        &*list_providers(&conn).expect("list").pop().unwrap().name,
        "Original"
    );

    update_provider(
        &conn,
        &ProviderId::new("p"),
        &UpdateProviderInput {
            notif_keyword_only: Some(Some(false)),
            ..Default::default()
        },
    )
    .expect("patch off");
    assert_eq!(sel(), 0);

    update_provider(
        &conn,
        &ProviderId::new("p"),
        &UpdateProviderInput::default(),
    )
    .expect("default no-op");
    assert_eq!(sel(), 0);
}

#[test]
fn update_provider_input_three_state_deserialize() {
    let cases = [
        ("{}", false, None),
        (r#"{"auto_activate_keyword": null}"#, true, None),
        (
            r#"{"auto_activate_keyword": "claude"}"#,
            true,
            Some("claude"),
        ),
    ];
    for (json, has_opt, val) in cases {
        let input: UpdateProviderInput = serde_json::from_str(json).unwrap();
        assert_eq!(input.auto_activate_keyword.is_some(), has_opt);
        if let Some(inner) = input.auto_activate_keyword {
            assert_eq!(inner.as_deref(), val);
        }
    }
    assert!(
        serde_json::from_str::<UpdateProviderInput>(r#"{"auto_activate_keyword": 42}"#).is_err()
    );
}

#[test]
fn update_provider_input_notif_keyword_only_three_state_deserialize() {
    let cases = [
        ("{}", false, None),
        (r#"{"notif_keyword_only": null}"#, true, None),
        (r#"{"notif_keyword_only": true}"#, true, Some(true)),
        (r#"{"notif_keyword_only": false}"#, true, Some(false)),
    ];
    for (json, has_opt, val) in cases {
        let input: UpdateProviderInput = serde_json::from_str(json).unwrap();
        assert_eq!(input.notif_keyword_only.is_some(), has_opt);
        if let Some(inner) = input.notif_keyword_only {
            assert_eq!(inner, val);
        }
    }
    assert!(serde_json::from_str::<UpdateProviderInput>(r#"{"notif_keyword_only": 42}"#).is_err());
}

#[test]
fn delete_provider_rejects_builtin() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    crate::seed::seed_builtin_providers(&conn).expect("seed builtins");

    for builtin_id in crate::seed::builtin_provider_ids() {
        let id = ProviderId::new(&builtin_id);
        let err = delete_provider(&conn, &id).expect_err("built-in delete must fail");
        let CoreError::Validation(msg) = err else {
            panic!("expected Validation, got {err:?}");
        };
        assert!(msg.contains("built-in") && msg.contains(&builtin_id));
        assert!(crate::providers::get(&conn, &id).expect("get").is_some());
    }
}

#[test]
fn delete_provider_allows_custom() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    create_provider(&conn, cp("my-custom", "My Custom")).expect("create custom");
    let id = ProviderId::new("my-custom");
    delete_provider(&conn, &id).expect("delete");
    assert!(crate::providers::get(&conn, &id).expect("get").is_none());
    delete_provider(&conn, &id).expect("idempotent");
}
