use super::fresh_pool;
use crate::api_keys::*;

#[test]
fn test_is_model_allowed_strict_glob_matching() {
    let (conn, _p) = fresh_pool();

    // `allowed_models` is additive: an entry grants access on top of the
    // default-permit, it does NOT restrict the catalog. A key whitelisting
    // "openai" still serves every non-blacklisted model.
    let (key1, _) = create(
        &conn,
        CreateApiKeyInput {
            label: Some("strict-test-1".into()),
            scopes: vec!["chat".into()],
            allowed_models: Some(vec!["openai".into()]),
            ..Default::default()
        },
        "admin",
    )
    .expect("create");

    assert!(key1.is_model_allowed("openai/gpt-4o", None));
    assert!(key1.is_model_allowed("gpt-4o", Some("openai")));
    assert!(key1.is_model_allowed("openai", None));
    assert!(key1.is_model_allowed("anthropic/claude-3", None));

    // The allowlist exception still matches strictly: whitelisting the literal
    // "openai" does NOT un-blacklist "openai/gpt-4o" behind a provider
    // blacklist, but "openai/*" does.
    let (key1b, _) = create(
        &conn,
        CreateApiKeyInput {
            label: Some("strict-test-1b".into()),
            scopes: vec!["chat".into()],
            allowed_models: Some(vec!["openai".into()]),
            blacklisted_providers: Some(vec!["openai".into()]),
            ..Default::default()
        },
        "admin",
    )
    .expect("create");

    assert!(!key1b.is_model_allowed("openai/gpt-4o", None));
    assert!(!key1b.is_model_allowed("gpt-4o", Some("openai")));
    assert!(key1b.is_model_allowed("openai", None));

    let (key2, _) = create(
        &conn,
        CreateApiKeyInput {
            label: Some("strict-test-2".into()),
            scopes: vec!["chat".into()],
            allowed_models: Some(vec!["openai/*".into()]),
            blacklisted_providers: Some(vec!["openai".into()]),
            ..Default::default()
        },
        "admin",
    )
    .expect("create");

    assert!(key2.is_model_allowed("openai/gpt-4o", None));
    assert!(key2.is_model_allowed("gpt-4o", Some("openai")));
    // Providers outside the blacklist are unaffected by the allowlist.
    assert!(key2.is_model_allowed("anthropic/claude-3", None));
}

#[test]
fn is_model_allowed_checks_blacklist_providers_and_models() {
    let (conn, _p) = fresh_pool();
    let (key, _) = create(
        &conn,
        CreateApiKeyInput {
            label: Some("blacklist-test".into()),
            scopes: vec!["chat".into()],
            blacklisted_providers: Some(vec!["openai".into(), "groq".into()]),
            blacklisted_models: Some(vec![
                "claude-3-opus*".into(),
                "anthropic/claude-2".into(),
                "bad-model".into(),
            ]),
            ..Default::default()
        },
        "admin",
    )
    .expect("create");

    assert!(!key.is_model_allowed("gpt-4o", Some("openai")));
    assert!(!key.is_model_allowed("openai/gpt-4o", None));
    assert!(!key.is_model_allowed("llama-3", Some("groq")));
    assert!(!key.is_provider_allowed("openai"));
    assert!(!key.is_provider_allowed("groq"));

    assert!(!key.is_model_allowed("claude-3-opus-20240229", Some("anthropic")));
    assert!(!key.is_model_allowed("anthropic/claude-3-opus-20240229", None));
    assert!(!key.is_model_allowed("claude-2", Some("anthropic")));
    assert!(!key.is_model_allowed("anthropic/claude-2", None));
    assert!(!key.is_model_allowed("bad-model", Some("custom")));

    assert!(key.is_model_allowed("claude-3-5-sonnet", Some("anthropic")));
    assert!(key.is_model_allowed("anthropic/claude-3-5-sonnet", None));
    assert!(key.is_provider_allowed("anthropic"));
}

#[test]
fn only_whitelisted_restricts_the_catalog_to_the_allowlist() {
    let (conn, _p) = fresh_pool();

    // Additive mode (default) is the documented baseline: the allowlist grants
    // access on top of the default-permit, it does not restrict anything.
    let (additive, _) = create(
        &conn,
        CreateApiKeyInput {
            label: Some("additive".into()),
            scopes: vec!["chat".into()],
            allowed_models: Some(vec!["openai".into()]),
            ..Default::default()
        },
        "admin",
    )
    .expect("create");
    assert!(additive.is_model_allowed("openai/gpt-4o", None));
    assert!(additive.is_model_allowed("anthropic/claude-3", None));

    // Strict mode: only the listed models are reachable. Note the same
    // allowlist that permitted `anthropic/claude-3` above now denies it.
    let (strict, _) = create(
        &conn,
        CreateApiKeyInput {
            label: Some("strict".into()),
            scopes: vec!["chat".into()],
            allowed_models: Some(vec!["openai/*".into(), "openai".into()]),
            only_whitelisted: true,
            ..Default::default()
        },
        "admin",
    )
    .expect("create");
    assert!(strict.is_model_allowed("openai/gpt-4o", None));
    assert!(strict.is_model_allowed("gpt-4o", Some("openai")));
    assert!(strict.is_model_allowed("openai", None));
    assert!(!strict.is_model_allowed("anthropic/claude-3", None));
    assert!(!strict.is_model_allowed("claude-3", Some("anthropic")));
}

#[test]
fn only_whitelisted_still_subtracts_the_model_blacklist() {
    let (conn, _p) = fresh_pool();
    let (key, _) = create(
        &conn,
        CreateApiKeyInput {
            label: Some("strict-blacklist".into()),
            scopes: vec!["chat".into()],
            allowed_models: Some(vec!["openai/*".into()]),
            blacklisted_models: Some(vec!["gpt-3.5*".into()]),
            only_whitelisted: true,
            ..Default::default()
        },
        "admin",
    )
    .expect("create");

    assert!(key.is_model_allowed("openai/gpt-4o", None));
    assert!(key.is_model_allowed("gpt-4o", Some("openai")));
    // The blacklist keeps subtracting from the allowlist.
    assert!(!key.is_model_allowed("openai/gpt-3.5-turbo", None));
    assert!(!key.is_model_allowed("gpt-3.5-turbo", Some("openai")));
    // Outside the allowlist, regardless of the blacklist.
    assert!(!key.is_model_allowed("anthropic/claude-3", None));
}

#[test]
fn only_whitelisted_with_empty_allowlist_denies_everything() {
    let (conn, _p) = fresh_pool();

    // An empty allowlist under additive mode is documented as "no restriction".
    let (additive, _) = create(
        &conn,
        CreateApiKeyInput {
            label: Some("additive-empty".into()),
            scopes: vec!["chat".into()],
            allowed_models: Some(vec![]),
            ..Default::default()
        },
        "admin",
    )
    .expect("create");
    assert!(additive.is_model_allowed("openai/gpt-4o", None));

    // Under strict mode it is the coherent reading of "only the selected
    // models": nothing is selected, so nothing is served.
    let (strict, _) = create(
        &conn,
        CreateApiKeyInput {
            label: Some("strict-empty".into()),
            scopes: vec!["chat".into()],
            allowed_models: Some(vec![]),
            only_whitelisted: true,
            ..Default::default()
        },
        "admin",
    )
    .expect("create");
    assert!(!strict.is_model_allowed("openai/gpt-4o", None));
    assert!(!strict.is_model_allowed("gpt-4o", Some("openai")));

    // `None` (never configured) behaves the same as empty under strict mode.
    let (strict_none, _) = create(
        &conn,
        CreateApiKeyInput {
            label: Some("strict-none".into()),
            scopes: vec!["chat".into()],
            only_whitelisted: true,
            ..Default::default()
        },
        "admin",
    )
    .expect("create");
    assert!(!strict_none.is_model_allowed("openai/gpt-4o", None));
}

#[test]
fn only_whitelisted_survives_a_round_trip_through_the_db() {
    let (conn, _p) = fresh_pool();
    let (created, _) = create(
        &conn,
        CreateApiKeyInput {
            label: Some("roundtrip".into()),
            scopes: vec!["chat".into()],
            allowed_models: Some(vec!["openai".into()]),
            only_whitelisted: true,
            ..Default::default()
        },
        "admin",
    )
    .expect("create");

    // The flag must be persisted and re-read, not just held in the returned row.
    let loaded = get_by_id(&conn, created.id)
        .expect("get_by_id")
        .expect("row");
    assert!(loaded.only_whitelisted);
    assert!(!loaded.is_model_allowed("anthropic/claude-3", None));

    // And it must be flippable through the update path.
    update(
        &conn,
        created.id,
        UpdateParams {
            only_whitelisted: Some(false),
            ..Default::default()
        },
    )
    .expect("update");
    let reloaded = get_by_id(&conn, created.id)
        .expect("get_by_id")
        .expect("row");
    assert!(!reloaded.only_whitelisted);
    assert!(reloaded.is_model_allowed("anthropic/claude-3", None));
}

#[test]
fn allowed_providers_whitelists_only_selected_providers() {
    let (conn, _p) = fresh_pool();
    let (key, _) = create(
        &conn,
        CreateApiKeyInput {
            label: Some("allowed-prov-test".into()),
            scopes: vec!["chat".into()],
            allowed_providers: Some(vec!["xkiro".into()]),
            ..Default::default()
        },
        "admin",
    )
    .expect("create");

    assert!(key.is_provider_allowed("xkiro"));
    assert!(!key.is_provider_allowed("anyrouter"));
    assert!(!key.is_provider_allowed("openai"));

    // When provider is passed explicitly
    assert!(key.is_model_allowed("gpt-4o", Some("xkiro")));
    assert!(!key.is_model_allowed("gpt-4o", Some("anyrouter")));

    // When model has provider prefix
    assert!(key.is_model_allowed("xkiro/openai/gpt-4o", None));
    assert!(!key.is_model_allowed("anyrouter/openai/gpt-4o", None));
}
