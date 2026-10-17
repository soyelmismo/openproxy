use crate::adapters::opencode_common::*;
use crate::adapters::*;
use openproxy_types::TargetFormat;

const ZEN: OpenCodeFlavor = OpenCodeFlavor::Zen;
const GO: OpenCodeFlavor = OpenCodeFlavor::Go;

#[test]
fn opencode_endpoints_use_inference_in_all_dialects() {
    for (adapter, base) in [
        (
            ProviderAdapterEnum::from_provider_id("opencode-zen").unwrap(),
            "https://opencode.ai/inference/v1",
        ),
        (
            ProviderAdapterEnum::from_provider_id("opencode-go").unwrap(),
            "https://opencode.ai/inference/go/v1",
        ),
    ] {
        assert_eq!(adapter.config().base_url, base);
        for (format, path) in [
            (TargetFormat::Openai, "/chat/completions"),
            (TargetFormat::Anthropic, "/messages"),
            (TargetFormat::Responses, "/responses"),
            (
                TargetFormat::Gemini,
                "/models/gemini-3-flash:streamGenerateContent?alt=sse",
            ),
        ] {
            assert_eq!(
                adapter.build_chat_url(format, &openproxy_types::ModelId::new("gemini-3-flash")),
                format!("{base}{path}"),
            );
        }
    }
}

#[test]
fn opencode_configured_headers_preserve_auth_and_drop_foreign_referer() {
    let model = openproxy_types::ModelId::new("step-5-preview-free");
    for provider in ["opencode-zen", "opencode-go"] {
        for (format, auth_name, expected) in [
            (TargetFormat::Openai, "authorization", "Bearer public"),
            (TargetFormat::Responses, "authorization", "Bearer public"),
            (TargetFormat::Anthropic, "x-api-key", "public"),
            (TargetFormat::Gemini, "x-goog-api-key", "public"),
        ] {
            let mut adapter = ProviderAdapterEnum::from_provider_id(provider).unwrap();
            adapter.config_mut().unwrap().extra_headers = vec![
                ("aUtHoRiZaTiOn".into(), String::new()),
                ("X-API-Key".into(), "  ".into()),
                ("X-Goog-Api-Key".into(), String::new()),
                ("hTtP-rEfErEr".into(), "https://example.invalid".into()),
                ("X-Custom".into(), "preservado".into()),
            ];
            let headers = adapter.build_headers("", format, &model);
            assert_eq!(
                headers
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(auth_name))
                    .map(|(_, v)| v.as_str()),
                Some(expected),
            );
            assert!(
                !headers
                    .iter()
                    .any(|(k, _)| k.eq_ignore_ascii_case("http-referer"))
            );
            assert!(!headers.iter().any(|(k, v)| {
                ["authorization", "x-api-key", "x-goog-api-key"]
                    .iter()
                    .any(|a| k.eq_ignore_ascii_case(a))
                    && v.trim().is_empty()
            }));
            assert!(
                headers
                    .iter()
                    .any(|(k, v)| k == "X-Custom" && v == "preservado")
            );
            // La credencial resuelta de una cuenta pagada tampoco se borra.
            let paid = adapter.build_headers("credential-fixture", format, &model);
            let expected_paid = if auth_name == "authorization" {
                "Bearer credential-fixture"
            } else {
                "credential-fixture"
            };
            assert_eq!(
                paid.iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(auth_name))
                    .map(|(_, v)| v.as_str()),
                Some(expected_paid)
            );
            adapter
                .config_mut()
                .unwrap()
                .extra_headers
                .push((auth_name.into(), "override-explicito".into()));
            let explicit = adapter.build_headers("", format, &model);
            assert_eq!(
                explicit
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(auth_name))
                    .map(|(_, v)| v.as_str()),
                Some("override-explicito")
            );
        }
    }
}

#[test]
fn test_classify_opencode_target_format() {
    assert_eq!(
        classify_opencode_target_format(ZEN, "claude-3-opus"),
        TargetFormat::Anthropic
    );
    assert_eq!(
        classify_opencode_target_format(ZEN, "minimax-abab6"),
        TargetFormat::Anthropic
    );
    assert_eq!(
        classify_opencode_target_format(ZEN, "CLAUDE-3-SONNET"),
        TargetFormat::Anthropic
    );
    assert_eq!(
        classify_opencode_target_format(ZEN, "gpt-4-turbo"),
        TargetFormat::Openai
    );
    assert_eq!(
        classify_opencode_target_format(ZEN, "gemini-3-flash"),
        TargetFormat::Gemini
    );
    assert_eq!(
        classify_opencode_target_format(ZEN, "qwen3.6-plus"),
        TargetFormat::Anthropic
    );
    assert_eq!(
        classify_opencode_target_format(ZEN, "muse-spark-1.3-contributor-free"),
        TargetFormat::Responses
    );
    assert_eq!(
        classify_opencode_target_format(ZEN, "muse-spark-1.3"),
        TargetFormat::Responses
    );
    assert_eq!(
        classify_opencode_target_format(GO, "muse-spark-1.2"),
        TargetFormat::Responses
    );
    assert_eq!(
        classify_opencode_target_format(ZEN, "gpt-5.6-terra"),
        TargetFormat::Responses
    );
    assert_eq!(
        classify_opencode_target_format(ZEN, "grok-4.6"),
        TargetFormat::Responses
    );
    assert_eq!(
        classify_opencode_target_format(ZEN, "mimo-v2.5-free"),
        TargetFormat::Openai
    );
    assert_eq!(
        classify_opencode_target_format(ZEN, "unknown-model"),
        TargetFormat::Openai
    );
}

/// `union-alpha` is only served by `/zen/v1/messages` on both flavors, and no
/// substring in the id distinguishes it, so the exact-ID override has to carry it.
#[test]
fn union_alpha_uses_anthropic_messages_on_both_flavors() {
    assert_eq!(
        classify_opencode_target_format(ZEN, "union-alpha"),
        TargetFormat::Anthropic
    );
    assert_eq!(
        classify_opencode_target_format(GO, "union-alpha"),
        TargetFormat::Anthropic
    );
    assert_eq!(
        classify_opencode_target_format(ZEN, "UNION-ALPHA"),
        TargetFormat::Anthropic
    );
}

/// Zen answers the paid MiniMax variants and the `-coder`/`-code` extras on
/// OpenAI-compatible `/chat/completions`; Go keeps them on `/messages`.
#[test]
fn zen_openai_compatible_exceptions() {
    for id in [
        "minimax-m3",
        "minimax-m2.7",
        "minimax-m2.5",
        "minimax-m2.1",
        "qwen3-coder",
        "grok-code",
    ] {
        assert_eq!(
            classify_opencode_target_format(ZEN, id),
            TargetFormat::Openai,
            "{id}"
        );
    }

    // Free MiniMax tiers stay on the Anthropic Messages API.
    assert_eq!(
        classify_opencode_target_format(ZEN, "minimax-m3-free"),
        TargetFormat::Anthropic
    );
    // Go routes the same aliases through `/messages`.
    assert_eq!(
        classify_opencode_target_format(GO, "minimax-m3"),
        TargetFormat::Anthropic
    );
    assert_eq!(
        classify_opencode_target_format(GO, "qwen3.7-max"),
        TargetFormat::Anthropic
    );
}

#[test]
fn flavor_aliases_match_core_classifier() {
    assert_eq!(
        classify_zen_target_format("minimax-m3"),
        TargetFormat::Openai
    );
    assert_eq!(
        classify_go_target_format("minimax-m3"),
        TargetFormat::Anthropic
    );
    assert_eq!(
        classify_zen_target_format("union-alpha"),
        TargetFormat::Anthropic
    );
}

#[test]
fn test_is_free_opencode_tier() {
    let m_free = openproxy_types::ModelId::new("minimax-m3-free");
    let m_pickle = openproxy_types::ModelId::new("big-pickle");
    let m_alpha = openproxy_types::ModelId::new("union-alpha");
    let m_paid = openproxy_types::ModelId::new("claude-3-5-sonnet");

    assert!(is_free_opencode_tier(ZEN, "public", &m_free));
    assert!(is_free_opencode_tier(ZEN, "", &m_paid));
    assert!(is_free_opencode_tier(ZEN, "some-key", &m_pickle));
    assert!(is_free_opencode_tier(ZEN, "some-key", &m_alpha));
    assert!(!is_free_opencode_tier(ZEN, "sk-paid", &m_paid));
    assert!(is_free_opencode_tier(GO, "public", &m_free));
    assert!(is_free_opencode_tier(GO, "", &m_paid));
    assert!(!is_free_opencode_tier(GO, "sk-paid", &m_paid));
}

#[test]
fn test_inject_opencode_agent_quartet_tools_openai() {
    let mut obj = serde_json::Map::new();
    inject_opencode_agent_quartet_tools(&mut obj, TargetFormat::Openai);

    let tools = obj.get("tools").unwrap().as_array().unwrap();
    assert_eq!(tools.len(), 4);
    let names: Vec<&str> = tools
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["bash", "glob", "grep", "read"]);

    // When some tools already exist, only missing ones are appended
    let mut obj2 = serde_json::Map::new();
    obj2.insert(
        "tools".to_string(),
        serde_json::json!([
            {
                "type": "function",
                "function": { "name": "bash" }
            }
        ]),
    );
    inject_opencode_agent_quartet_tools(&mut obj2, TargetFormat::Openai);
    let tools2 = obj2.get("tools").unwrap().as_array().unwrap();
    assert_eq!(tools2.len(), 4);
}

#[test]
fn test_inject_opencode_agent_quartet_tools_gemini() {
    let mut obj = serde_json::Map::new();
    inject_opencode_agent_quartet_tools(&mut obj, TargetFormat::Gemini);

    let tools = obj.get("tools").unwrap().as_array().unwrap();
    assert_eq!(tools.len(), 1);
    let decls = tools[0]["functionDeclarations"].as_array().unwrap();
    assert_eq!(decls.len(), 4);
    let names: Vec<&str> = decls.iter().map(|d| d["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["bash", "glob", "grep", "read"]);
}

#[test]
fn test_opencode_build_chat_url() {
    let zen = OpenCodeZenAdapter::new();
    let go = OpenCodeGoAdapter::new();
    let gemini_model = openproxy_types::ModelId::new("gemini-2.5-flash");
    let openai_model = openproxy_types::ModelId::new("big-pickle");
    let anthropic_model = openproxy_types::ModelId::new("claude-3-5-sonnet");

    assert_eq!(
        zen.build_chat_url(TargetFormat::Gemini, &gemini_model),
        "https://opencode.ai/inference/v1/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
    );
    assert_eq!(
        go.build_chat_url(TargetFormat::Gemini, &gemini_model),
        "https://opencode.ai/inference/go/v1/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
    );
    assert_eq!(
        zen.build_chat_url(TargetFormat::Openai, &openai_model),
        "https://opencode.ai/inference/v1/chat/completions"
    );
    assert_eq!(
        zen.build_chat_url(TargetFormat::Anthropic, &anthropic_model),
        "https://opencode.ai/inference/v1/messages"
    );
}

#[test]
fn test_wrap_request_body_free_tier() {
    let adapter = OpenCodeZenAdapter::new();
    let target = openproxy_types::context::ResolvedTarget {
        target: openproxy_types::combos::ComboTarget {
            id: openproxy_types::ids::ComboTargetId(0),
            combo_id: openproxy_types::ids::ComboId(0),
            provider_id: openproxy_types::ids::ProviderId::new("opencode-zen"),
            account_id: None,
            model_row_id: None,
            sub_combo_id: None,
            priority_order: 0,
            weight: 1,
            active: true,
            rate_limit_scope: openproxy_types::providers::RateLimitScope::Account,
            cooldown_mode: None,
            cooldown_base_secs: None,
            cooldown_max_secs: None,
            cooldown_factor: None,
            thinking_effort: None,
            description: None,
            selected_accounts: None,
        },
        model: openproxy_types::models::Model {
            row_id: openproxy_types::ids::ModelRowId(1),
            provider_id: openproxy_types::ids::ProviderId::new("opencode-zen"),
            model_id: openproxy_types::ids::ModelId::new("big-pickle"),
            target_format: TargetFormat::Openai,
            ..Default::default()
        },
        api_key: "public".into(),
        api_key_label: None,
        custom_meta: None,
    };

    let initial_body = bytes::Bytes::from(
        r#"{"model":"big-pickle","messages":[{"role":"user","content":"hi"}],"stream":false}"#,
    );
    let wrapped = adapter
        .wrap_request_body(
            initial_body,
            TargetFormat::Openai,
            &target.model.model_id,
            &target,
        )
        .unwrap();

    let parsed: serde_json::Value = serde_json::from_slice(&wrapped).unwrap();
    assert_eq!(parsed["stream"], true);
    let tools = parsed["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 4);
}

#[test]
fn test_wrap_request_body_muse_spark_responses() {
    let adapter = OpenCodeZenAdapter::new();
    let target = openproxy_types::context::ResolvedTarget {
        target: openproxy_types::combos::ComboTarget {
            id: openproxy_types::ids::ComboTargetId(0),
            combo_id: openproxy_types::ids::ComboId(0),
            provider_id: openproxy_types::ids::ProviderId::new("opencode-zen"),
            account_id: None,
            model_row_id: None,
            sub_combo_id: None,
            priority_order: 0,
            weight: 1,
            active: true,
            rate_limit_scope: openproxy_types::providers::RateLimitScope::Account,
            cooldown_mode: None,
            cooldown_base_secs: None,
            cooldown_max_secs: None,
            cooldown_factor: None,
            thinking_effort: None,
            description: None,
            selected_accounts: None,
        },
        model: openproxy_types::models::Model {
            row_id: openproxy_types::ids::ModelRowId(1),
            provider_id: openproxy_types::ids::ProviderId::new("opencode-zen"),
            model_id: openproxy_types::ids::ModelId::new("muse-spark-1.3"),
            target_format: TargetFormat::Responses,
            ..Default::default()
        },
        api_key: "public".into(),
        api_key_label: None,
        custom_meta: None,
    };

    let initial_body = bytes::Bytes::from(
        r#"{"model":"muse-spark-1.3","input":[{"role":"user","content":"hi"}],"stream":false}"#,
    );
    let wrapped = adapter
        .wrap_request_body(
            initial_body,
            TargetFormat::Responses,
            &target.model.model_id,
            &target,
        )
        .unwrap();

    let parsed: serde_json::Value = serde_json::from_slice(&wrapped).unwrap();
    assert_eq!(parsed["stream"], true);
    assert_eq!(parsed["model"], "muse-spark-1.3-contributor-free");
    let tools = parsed["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 4);
    // Responses tools format: {"type": "function", "name": "bash", ...}
    assert_eq!(tools[0]["type"], "function");
    assert_eq!(tools[0]["name"], "bash");
}

#[test]
fn test_opencode_dynamic_headers_and_config_mut() {
    use crate::spoofer::{
        OPENCODE_TEST_LOCK, current_opencode_ua, current_opencode_version,
        reset_dynamic_opencode_overrides, set_dynamic_opencode_extra_header,
        set_dynamic_opencode_version,
    };

    let _guard = OPENCODE_TEST_LOCK.lock().unwrap();
    reset_dynamic_opencode_overrides();
    assert_eq!(current_opencode_version(), "2.0.22");

    let mut adapter = OpenCodeZenAdapter::new();

    // 1. Verify config_mut works and allows setting extra_headers
    let cfg = adapter
        .config_mut()
        .expect("config_mut must be implemented");
    cfg.extra_headers
        .push(("x-admin-injected".into(), "true".into()));
    cfg.extra_headers
        .push(("x-custom-rule".into(), "rule-42".into()));

    let model_id = openproxy_types::ModelId::new("claude-3-5-sonnet");
    let headers = adapter.build_headers("my-key", TargetFormat::Anthropic, &model_id);

    let find = |k: &str| {
        headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
    };

    assert_eq!(find("x-admin-injected"), Some("true"));
    assert_eq!(find("x-custom-rule"), Some("rule-42"));
    assert_eq!(find("User-Agent"), Some("opencode/latest/2.0.22/cli"));

    // 2. Test in-memory dynamic version upgrade without recompilation
    set_dynamic_opencode_version("1.25.0");
    assert_eq!(current_opencode_version(), "1.25.0");
    assert_eq!(current_opencode_ua(), "opencode/latest/1.25.0/cli");

    let headers_updated = adapter.build_headers("my-key", TargetFormat::Anthropic, &model_id);
    let find_up = |k: &str| {
        headers_updated
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(find_up("User-Agent"), Some("opencode/latest/1.25.0/cli"));

    // 3. Test in-memory dynamic extra header injection without recompilation
    set_dynamic_opencode_extra_header("x-opencode-experimental", "fast-routing");
    let headers_dyn = adapter.build_headers("my-key", TargetFormat::Anthropic, &model_id);
    let find_dyn = |k: &str| {
        headers_dyn
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(find_dyn("x-opencode-experimental"), Some("fast-routing"));

    // 4. Test Go adapter as well
    let mut go_adapter = OpenCodeGoAdapter::new();
    let go_cfg = go_adapter
        .config_mut()
        .expect("Go adapter config_mut must be implemented");
    go_cfg
        .extra_headers
        .push(("x-go-test".into(), "active".into()));
    let go_headers = go_adapter.build_headers("go-key", TargetFormat::Openai, &model_id);
    let find_go = |k: &str| {
        go_headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(find_go("x-go-test"), Some("active"));
    assert_eq!(find_go("User-Agent"), Some("opencode/latest/1.25.0/cli"));
    assert_eq!(find_go("x-opencode-experimental"), Some("fast-routing"));

    // Clean up
    reset_dynamic_opencode_overrides();
    assert_eq!(current_opencode_version(), "2.0.22");
}

#[test]
fn test_opencode_default_headers_contract() {
    use crate::spoofer::{
        OPENCODE_TEST_LOCK, current_opencode_ua, reset_dynamic_opencode_overrides,
    };

    let _guard = OPENCODE_TEST_LOCK.lock().unwrap();
    reset_dynamic_opencode_overrides();
    let adapter = OpenCodeZenAdapter::new();
    let model_id = openproxy_types::ModelId::new("claude-3-5-sonnet");
    let headers = adapter.build_headers("opencode-token", TargetFormat::Anthropic, &model_id);
    let find = |k: &str| {
        headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
    };

    // Strict bijective baseline contract for OpenCode upstream (Anthropic format): exactly the expected set of keys
    let actual_keys: std::collections::BTreeSet<String> = headers
        .iter()
        .map(|(k, _)| k.to_ascii_lowercase())
        .collect();
    let expected_keys: std::collections::BTreeSet<String> = [
        "content-type",
        "x-api-key",
        "anthropic-version",
        "user-agent",
        "x-opencode-client",
        "x-opencode-project",
        "x-opencode-session",
        "x-opencode-session-id",
        "x-session-affinity",
        "x-session-id",
        "b3",
        "traceparent",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(
        actual_keys, expected_keys,
        "OpenCode contract breach: header added or removed"
    );

    assert_eq!(find("Content-Type"), Some("application/json"));
    assert_eq!(find("x-api-key"), Some("opencode-token"));
    assert_eq!(find("Anthropic-Version"), Some("2023-06-01"));
    assert_eq!(find("User-Agent"), Some(current_opencode_ua().as_str()));
    assert_eq!(find("x-opencode-client"), Some("cli"));
    assert_eq!(find("x-opencode-project"), Some("global"));
    assert!(find("x-opencode-session").is_some_and(|s| s.starts_with("ses_")));
    assert!(find("x-opencode-session-id").is_some());
    assert!(find("x-session-affinity").is_some());
    assert!(find("x-session-id").is_some());
    assert!(find("b3").is_some());
    assert!(find("traceparent").is_some());
    // x-opencode-request must NOT be present (not sent by real CLI)
    assert!(find("x-opencode-request").is_none());
}
