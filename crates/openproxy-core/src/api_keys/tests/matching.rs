use super::fresh_pool;
use crate::api_keys::*;

#[test]
fn test_is_model_allowed_strict_glob_matching() {
    let (conn, _p) = fresh_pool();

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

    assert!(!key1.is_model_allowed("openai/gpt-4o", None));
    assert!(!key1.is_model_allowed("gpt-4o", Some("openai")));
    assert!(key1.is_model_allowed("openai", None));

    let (key2, _) = create(
        &conn,
        CreateApiKeyInput {
            label: Some("strict-test-2".into()),
            scopes: vec!["chat".into()],
            allowed_models: Some(vec!["openai/*".into()]),
            ..Default::default()
        },
        "admin",
    )
    .expect("create");

    assert!(key2.is_model_allowed("openai/gpt-4o", None));
    assert!(key2.is_model_allowed("gpt-4o", Some("openai")));
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
