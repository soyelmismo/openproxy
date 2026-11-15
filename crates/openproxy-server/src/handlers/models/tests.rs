use super::*;
use openproxy_core::models::{Model, TargetFormat};
use openproxy_types::{ModelId, ModelRowId, ProviderId};

fn empty_model() -> Model {
    Model {
        row_id: ModelRowId(1),
        provider_id: ProviderId::new("openrouter"),
        model_id: ModelId::new("openai/gpt-4o"),
        display_name: None,
        target_format: TargetFormat::Openai,
        discovered_at: "2024-01-01 00:00:00".into(),
        expires_at: None,
        timeout_overrides_json: None,
        active: true,
        last_test_status: None,
        last_test_at: None,
        custom: false,
        // All metadata empty: exercises the heuristic fallback.
        context_length: None,
        max_output_tokens: None,
        capabilities_json: None,
        family: None,
        model_type: "chat".into(),
        input_modalities_json: None,
        output_modalities_json: None,
        ..Default::default()
    }
}

#[test]
fn gpt4o_falls_back_to_heuristic() {
    let m = empty_model();
    let v = build_model_entry(&m);
    // Vision should be detected from the model_id heuristic.
    let caps = v.get("capabilities").and_then(|c| c.get("vision")).unwrap();
    assert_eq!(caps, &serde_json::Value::Bool(true));
    // Context length should be the heuristic-known 128_000.
    assert_eq!(v.get("context_length").unwrap().as_i64(), Some(128_000));
}

#[test]
fn db_values_override_heuristic() {
    let mut m = empty_model();
    m.context_length = Some(999_999);
    m.capabilities_json = Some(r#"{"vision": false}"#.into());
    let v = build_model_entry(&m);
    // DB value wins, and an explicit `false` is present rather than omitted.
    let caps = v.get("capabilities").unwrap();
    assert_eq!(caps.get("vision"), Some(&serde_json::Value::Bool(false)));
    assert_eq!(v.get("context_length").unwrap().as_i64(), Some(999_999));
}

#[test]
fn capabilities_object_omits_nulls() {
    let m = empty_model();
    let v = build_model_entry(&m);
    let caps = v.get("capabilities").and_then(|c| c.as_object()).unwrap();
    // A heuristic-inferred gpt-4o row yields every inferable capability;
    // `reasoning`/`thinking` match no keyword and must be omitted — the
    // exact omit-on-null contract under test.
    for key in [
        "vision",
        "tool_calling",
        "structured_output",
        "temperature",
        "attachment",
    ] {
        assert!(caps.contains_key(key), "missing key {key}");
    }
    assert!(
        !caps.contains_key("reasoning"),
        "reasoning should be omitted for a non-reasoning model"
    );
    // `created` is a non-zero unix timestamp; `object`/`owned_by` round-trip.
    assert!(v.get("created").unwrap().as_i64().unwrap() > 0);
    assert_eq!(v.get("object").unwrap().as_str(), Some("model"));
    assert_eq!(v.get("owned_by").unwrap().as_str(), Some("openrouter"));
}

#[test]
fn id_is_provider_prefixed() {
    // The provider prefix keeps chat-endpoint round-trips unambiguous; exact
    // shape `<provider>/<upstream_id>` is pinned here.
    let m = empty_model();
    let v = build_model_entry(&m);
    let id = v
        .get("id")
        .and_then(|x| x.as_str())
        .expect("id is a string");
    // empty_model() = provider "openrouter" + upstream "openai/gpt-4o".
    assert_eq!(
        id, "openrouter/openai/gpt-4o",
        "id must be provider-prefixed"
    );
    // `root` mirrors `id` for SDKs that compare them.
    let root = v
        .get("root")
        .and_then(|x| x.as_str())
        .expect("root is a string");
    assert_eq!(root, id, "root mirrors id");
}

#[test]
fn id_handles_already_prefixed_upstream_id() {
    // An upstream id that already contains `/` (e.g. OpenRouter's
    // `nex-agi/nex-n2-pro:free`) yields two slashes. Expected: only the first
    // `/` separates provider from upstream; later ones belong to the model name.
    let mut m = empty_model();
    m.model_id = ModelId::new("nex-agi/nex-n2-pro:free");
    let v = build_model_entry(&m);
    let id = v
        .get("id")
        .and_then(|x| x.as_str())
        .expect("id is a string");
    assert_eq!(id, "openrouter/nex-agi/nex-n2-pro:free");
}

#[test]
fn api_key_model_filtering_logic() {
    let key = openproxy_core::api_keys::ApiKey {
        id: openproxy_types::ApiKeyId(1),
        key_hash: "hash".into(),
        key_prefix: Some("op_live_test".into()),
        label: Some("test".into()),
        scopes: vec!["chat".into()],
        allowed_models: None,
        allowed_combos: None,
        blacklisted_providers: Some(vec!["openrouter".into()]),
        allowed_providers: None,
        blacklisted_models: Some(vec!["gpt-3.5*".into()]),
        only_whitelisted: false,
        is_active: true,
        revoked_at: None,
        expires_at: None,
        last_used_at: None,
        created_at: "2024-01-01".into(),
        created_by: None,
    };

    let m1 = empty_model(); // provider: openrouter, model: openai/gpt-4o
    let mut m2 = empty_model();
    m2.provider_id = ProviderId::new("openai");
    m2.model_id = ModelId::new("gpt-4o");

    let mut m3 = empty_model();
    m3.provider_id = ProviderId::new("openai");
    m3.model_id = ModelId::new("gpt-3.5-turbo");

    let list = vec![m1, m2, m3];
    let filtered: Vec<_> = list
        .into_iter()
        .filter(|m| key.is_model_allowed(m.model_id.as_str(), Some(m.provider_id.as_str())))
        .collect();

    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].provider_id.as_str(), "openai");
    assert_eq!(filtered[0].model_id.as_str(), "gpt-4o");
}

#[test]
fn strict_key_with_combo_allowlist_lists_its_combos() {
    // A tier like "Lite": only_whitelisted = 1, empty model allowlist and
    // two combos. The combo ids are the authoritative gate, so the catalog
    // must list them even though `is_model_allowed("combo:...")` is false
    // under the strict model gate.
    let key = openproxy_core::api_keys::ApiKey {
        id: openproxy_types::ApiKeyId(1),
        key_hash: "hash".into(),
        key_prefix: Some("op_live_test".into()),
        label: Some("test".into()),
        scopes: vec!["chat".into()],
        allowed_models: Some(vec![]),
        allowed_combos: Some(vec![607, 608]),
        blacklisted_providers: None,
        allowed_providers: None,
        blacklisted_models: None,
        only_whitelisted: true,
        is_active: true,
        revoked_at: None,
        expires_at: None,
        last_used_at: None,
        created_at: "2024-01-01".into(),
        created_by: None,
    };
    let combo = |id: i64, name: &str| openproxy_types::Combo {
        id: openproxy_types::ComboId(id),
        name: name.into(),
        strategy: openproxy_types::combos::Strategy::Priority,
        race_size: 1,
        preventive_rate_limit: false,
        created_at: "2024-01-01".into(),
        context_window: None,
        max_output_tokens: None,
        priority_mode: Default::default(),
        cooldown_mode: Default::default(),
        cooldown_base_secs: None,
        cooldown_max_secs: None,
        cooldown_factor: None,
        lkgp_exploration_rate: None,
        selection_window_secs: None,
        decision_model: None,
        decision_timeout_ms: None,
        modality_gate: None,
        sanitize_response: None,
        real_model_id: None,
        purge_reasoning: None,
        pricing_input_per_1m: None,
        pricing_output_per_1m: None,
        pricing_cached_per_1m: None,
        pricing_schedule: None,
    };
    let list = vec![
        combo(607, "deepseek-v4.1-flash"),
        combo(608, "glm-5.3-flash"),
        combo(1, "someone-elses-combo"),
    ];
    let filtered = filter_combos_for_key(list, Some(&key));
    let names: Vec<&str> = filtered.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["deepseek-v4.1-flash", "glm-5.3-flash"]);
}

#[test]
fn strict_key_without_combo_allowlist_lists_no_combos() {
    // Guard the other side: a strict key with no model and no combo
    // allowlist must not see the catalog.
    let key = openproxy_core::api_keys::ApiKey {
        id: openproxy_types::ApiKeyId(1),
        key_hash: "hash".into(),
        key_prefix: Some("op_live_test".into()),
        label: Some("test".into()),
        scopes: vec!["chat".into()],
        allowed_models: Some(vec![]),
        allowed_combos: None,
        blacklisted_providers: None,
        allowed_providers: None,
        blacklisted_models: None,
        only_whitelisted: true,
        is_active: true,
        revoked_at: None,
        expires_at: None,
        last_used_at: None,
        created_at: "2024-01-01".into(),
        created_by: None,
    };
    let combo = openproxy_types::Combo {
        id: openproxy_types::ComboId(607),
        name: "deepseek-v4.1-flash".into(),
        strategy: openproxy_types::combos::Strategy::Priority,
        race_size: 1,
        preventive_rate_limit: false,
        created_at: "2024-01-01".into(),
        context_window: None,
        max_output_tokens: None,
        priority_mode: Default::default(),
        cooldown_mode: Default::default(),
        cooldown_base_secs: None,
        cooldown_max_secs: None,
        cooldown_factor: None,
        lkgp_exploration_rate: None,
        selection_window_secs: None,
        decision_model: None,
        decision_timeout_ms: None,
        modality_gate: None,
        sanitize_response: None,
        real_model_id: None,
        purge_reasoning: None,
        pricing_input_per_1m: None,
        pricing_output_per_1m: None,
        pricing_cached_per_1m: None,
        pricing_schedule: None,
    };
    assert!(filter_combos_for_key(vec![combo], Some(&key)).is_empty());
}

#[test]
fn gemini_flash_lite_model_type_is_chat() {
    let mut m = empty_model();
    m.provider_id = ProviderId::new("gemini");
    m.model_id = ModelId::new("gemini-2.0-flash-lite");
    m.model_type = "audio".into(); // Simulate stale/corrupt DB entry
    m.custom = false;

    let v = build_model_entry(&m);
    assert_eq!(
        v.get("type").and_then(|t| t.as_str()),
        Some("chat"),
        "Gemini flash-lite must be categorized as chat even if DB had stale audio"
    );
}

#[test]
fn test_build_capabilities_object_includes_decisions() {
    let caps = capabilities::ModelCapabilities {
        decisions: Some(true),
        vision: Some(true),
        ..Default::default()
    };

    let obj = build_capabilities_object(&caps);
    assert_eq!(obj.get("decisions"), Some(&serde_json::Value::Bool(true)));
    assert_eq!(obj.get("vision"), Some(&serde_json::Value::Bool(true)));
    assert_eq!(obj.get("thinking"), None);
}

#[test]
fn test_reasoning_and_supported_parameters_populated() {
    let mut m = empty_model();
    m.capabilities_json = Some(r#"{"reasoning": true, "tool_calling": true}"#.into());
    let v = build_model_entry(&m);

    assert_eq!(
        v.get("supports_reasoning"),
        Some(&serde_json::Value::Bool(true))
    );
    let params: Vec<&str> = v
        .get("supported_parameters")
        .and_then(|p| p.as_array())
        .unwrap()
        .iter()
        .filter_map(|s| s.as_str())
        .collect();
    assert!(params.contains(&"reasoning_effort"));
    assert!(params.contains(&"thinking"));
    assert!(params.contains(&"tools"));
}

#[test]
fn test_combo_entry_reasoning_and_supported_parameters() {
    let combo = openproxy_types::Combo {
        id: openproxy_types::ComboId(1),
        name: "test-ninja".into(),
        strategy: openproxy_types::combos::Strategy::Priority,
        race_size: 1,
        preventive_rate_limit: false,
        created_at: "2024-01-01".into(),
        context_window: None,
        max_output_tokens: None,
        priority_mode: Default::default(),
        cooldown_mode: Default::default(),
        cooldown_base_secs: None,
        cooldown_max_secs: None,
        cooldown_factor: None,
        lkgp_exploration_rate: None,
        selection_window_secs: None,
        decision_model: None,
        decision_timeout_ms: None,
        modality_gate: None,
        sanitize_response: None,
        real_model_id: None,
        purge_reasoning: None,
        pricing_input_per_1m: None,
        pricing_output_per_1m: None,
        pricing_cached_per_1m: None,
        pricing_schedule: None,
    };
    let mut caps = capabilities::ModelCapabilities::empty();
    caps.thinking = Some(true);
    caps.tool_calling = Some(true);

    let v = build_combo_entry(
        &combo,
        Some("test-ninja"),
        Some(128_000),
        Some(16_384),
        Some(&caps),
    );
    assert_eq!(
        v.get("id"),
        Some(&serde_json::Value::String("test-ninja".into()))
    );
    assert_eq!(
        v.get("supports_reasoning"),
        Some(&serde_json::Value::Bool(true))
    );
    let params: Vec<&str> = v
        .get("supported_parameters")
        .and_then(|p| p.as_array())
        .unwrap()
        .iter()
        .filter_map(|s| s.as_str())
        .collect();
    assert!(params.contains(&"reasoning_effort"));
    assert!(params.contains(&"thinking"));
    assert!(params.contains(&"tools"));
    let caps_obj = v.get("capabilities").and_then(|c| c.as_object()).unwrap();
    assert_eq!(
        caps_obj.get("thinking"),
        Some(&serde_json::Value::Bool(true))
    );
}
