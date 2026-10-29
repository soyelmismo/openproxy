use super::*;
pub(super) fn resolved_for(model_id: &str) -> openproxy_types::context::ResolvedTarget {
    openproxy_types::context::ResolvedTarget {
        target: openproxy_types::combos::ComboTarget {
            id: openproxy_types::ComboTargetId(1),
            combo_id: openproxy_types::ComboId(1),
            provider_id: openproxy_types::ProviderId::new("claude-code"),
            model_row_id: Some(openproxy_types::ModelRowId(1)),
            weight: 100,
            ..Default::default()
        },
        model: openproxy_types::Model {
            row_id: openproxy_types::ModelRowId(1),
            provider_id: openproxy_types::ProviderId::new("claude-code"),
            target_format: openproxy_types::TargetFormat::Anthropic,
            discovered_at: openproxy_types::now_unix_secs_str().into_boxed_str(),
            model_id: ModelId::new(model_id),
            context_length: Some(200_000),
            max_output_tokens: Some(8192),
            model_type: "chat".into(),
            family: Some("claude".into()),
            active: true,
            ..Default::default()
        },
        api_key: "test-token".to_string(),
        custom_meta: Some(openproxy_types::context::CustomProviderMeta {
            access_token: "test-token".to_string(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

pub(super) fn wrap_then_finalize(
    adapter: &ClaudeCodeAdapter,
    model_id: &str,
    raw: &[u8],
    headers: Vec<(String, String)>,
) -> (serde_json::Value, Vec<(String, String)>) {
    let model = ModelId::new(model_id);
    let resolved = resolved_for(model_id);
    let wrapped = adapter
        .wrap_request_body(
            bytes::Bytes::copy_from_slice(raw),
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .expect("wrap_request_body");
    let mut headers = headers;
    let finalized = adapter
        .finalize_request(
            wrapped,
            &mut headers,
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .expect("finalize_request");
    let json: serde_json::Value = serde_json::from_slice(&finalized).expect("finalized json");
    (json, headers)
}

pub(super) fn cch_of(json: &serde_json::Value) -> String {
    let text = json["system"][0]["text"].as_str().expect("billing text");
    let idx = text.find("cch=").expect("cch placeholder");
    text[idx + 4..].chars().take_while(|c| *c != ';').collect()
}

#[test]
fn test_claude_code_finalize_syncs_session_from_canonical_header() {
    let adapter = ClaudeCodeAdapter::new();
    let raw = br#"{"model":"claude-opus-5-5","messages":[{"role":"user","content":"hello"}]}"#;
    let canonical = "canonical-session-9f8e7d6c";
    let headers = vec![(
        "X-Claude-Code-Session-Id".to_string(),
        canonical.to_string(),
    )];

    let (json, out_headers) = wrap_then_finalize(&adapter, "claude-opus-5-5", raw, headers);

    // Body session metadata now mirrors the canonical header the pipeline set.
    let user_id_str = json["metadata"]["user_id"]
        .as_str()
        .expect("user_id string");
    let user_id: serde_json::Value = serde_json::from_str(user_id_str).expect("user_id json");
    assert_eq!(user_id["session_id"].as_str(), Some(canonical));

    // The canonical header is still present on the way out.
    assert!(
        out_headers
            .iter()
            .any(|(k, v)| { k.eq_ignore_ascii_case("x-claude-code-session-id") && v == canonical })
    );

    // CCH is signed over the final bytes (placeholder replaced).
    assert_ne!(cch_of(&json), "00000");
}

#[test]
fn test_claude_code_finalize_signs_after_strip() {
    let adapter = ClaudeCodeAdapter::new();
    // A provider-configured param that the pipeline strips before finalize.
    let raw = br#"{"model":"claude-opus-5-5","temperature":0.7,"messages":[{"role":"user","content":"hi"}]}"#;

    let model = ModelId::new("claude-opus-5-5");
    let resolved = resolved_for("claude-opus-5-5");
    let wrapped = adapter
        .wrap_request_body(
            bytes::Bytes::from_static(raw),
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .expect("wrap");

    // Simulate the pipeline strip removing a top-level param.
    let mut stripped: serde_json::Value = serde_json::from_slice(&wrapped).unwrap();
    stripped.as_object_mut().unwrap().remove("temperature");
    let stripped_bytes = bytes::Bytes::from(serde_json::to_vec(&stripped).unwrap());

    let finalized = adapter
        .finalize_request(
            stripped_bytes,
            &mut Vec::new(),
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .expect("finalize after strip");

    // temperature is gone and the CCH is a real signature, not the placeholder.
    let json: serde_json::Value = serde_json::from_slice(&finalized).unwrap();
    assert!(json.get("temperature").is_none());
    assert_ne!(cch_of(&json), "00000");

    // The signature describes exactly these bytes: re-signing is idempotent.
    let mut resigned = finalized.to_vec();
    signing::sign_anthropic_messages_body(&mut resigned).expect("re-sign");
    assert_eq!(
        resigned,
        finalized.to_vec(),
        "cch must be stable over the final bytes"
    );
}

#[test]
fn test_claude_code_finalize_betas_from_final_body_preserve_downstream() {
    let adapter = ClaudeCodeAdapter::new();
    // opus-5-5 gains a `fallbacks` entry during wrapping, so the finalized body
    // must advertise the fallback betas.
    let raw = br#"{"model":"claude-opus-5-5","messages":[{"role":"user","content":"hi"}]}"#;
    let downstream = "custom-downstream-beta-2099-01-01";
    let headers = vec![("anthropic-beta".to_string(), downstream.to_string())];

    let (json, out_headers) = wrap_then_finalize(&adapter, "claude-opus-5-5", raw, headers);

    // Body-derived fallback capability is present.
    assert!(json.get("fallbacks").is_some());

    let beta = out_headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("anthropic-beta"))
        .map(|(_, v)| v.clone())
        .expect("anthropic-beta header");
    for expected in [
        downstream,
        "server-side-fallback-2026-06-01",
        "fallback-credit-2026-06-01",
        "claude-code-20250219",
    ] {
        assert!(
            beta.contains(expected),
            "betas `{beta}` missing `{expected}`"
        );
    }
}

#[test]
fn test_claude_code_legacy_and_modern_preserve_tools_and_cache() {
    let adapter = ClaudeCodeAdapter::new();
    for model_id in ["claude-3-7-sonnet-20250219", "claude-opus-5-5"] {
        let raw = format!(
            r#"{{"model":"{model_id}","system":[{{"type":"text","text":"You are helpful","cache_control":{{"type":"ephemeral"}}}}],"tools":[{{"name":"get_weather","description":"w","input_schema":{{"type":"object"}}}},{{"name":"search","description":"s","input_schema":{{"type":"object"}}}}],"messages":[{{"role":"user","content":"hi"}}]}}"#
        );
        let (json, _) = wrap_then_finalize(&adapter, model_id, raw.as_bytes(), Vec::new());

        // Tools survive both legacy and modern cloaking: same count, schema and
        // cache_control, with the names cloaked to TitleCase.
        let tools = json["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 2, "{model_id}: tools must be preserved");
        assert_eq!(tools[0]["name"].as_str(), Some("GetWeather"));
        assert_eq!(tools[0]["input_schema"]["type"].as_str(), Some("object"));
        assert_eq!(tools[1]["name"].as_str(), Some("Search"));
        assert_eq!(tools[1]["description"].as_str(), Some("s"));

        // At least one cache_control breakpoint remains after finalize.
        let has_cc = json
            .get("system")
            .and_then(|s| s.as_array())
            .is_some_and(|a| a.iter().any(|b| b.get("cache_control").is_some()))
            || json
                .get("tools")
                .and_then(|t| t.as_array())
                .is_some_and(|a| a.iter().any(|b| b.get("cache_control").is_some()));
        assert!(has_cc, "{model_id}: cache_control must be preserved");

        // Signed regardless of model generation.
        assert_ne!(cch_of(&json), "00000", "{model_id}: cch must be signed");
    }
}

#[test]
fn test_title_case_tool_name() {
    assert_eq!(
        cloaking::title_case_tool_name("browser_exec"),
        "BrowserExec"
    );
    assert_eq!(cloaking::title_case_tool_name("read_file"), "ReadFile");
    assert_eq!(cloaking::title_case_tool_name("read-file"), "ReadFile");
    assert_eq!(cloaking::title_case_tool_name("terminal"), "Terminal");
    assert_eq!(cloaking::title_case_tool_name("Read"), "Read");
    assert_eq!(
        cloaking::title_case_tool_name("mcp__server__tool"),
        "McpServerTool"
    );
    assert_eq!(cloaking::title_case_tool_name("gpt_4o"), "Gpt4o");
    assert_eq!(cloaking::title_case_tool_name("___"), "");
}

#[test]
fn test_tool_rename_map_skips_identity_and_collisions() {
    let json = serde_json::json!({
        "tools": [
            {"name": "browser_exec"},
            {"name": "Read"},
            {"name": "terminal"}
        ]
    });
    let map = cloaking::build_tool_rename_map(&json);
    assert_eq!(
        map.get("browser_exec").map(String::as_str),
        Some("BrowserExec")
    );
    assert_eq!(map.get("terminal").map(String::as_str), Some("Terminal"));
    // Already TitleCase names are identity and stay out of the map.
    assert!(!map.contains_key("Read"));

    // Two originals that collapse onto the same cloaked form are both left
    // alone, so the upstream keeps seeing two distinguishable tools.
    let colliding = serde_json::json!({
        "tools": [{"name": "read_file"}, {"name": "read-file"}]
    });
    assert!(cloaking::build_tool_rename_map(&colliding).is_empty());
}

#[test]
fn test_claude_code_tool_names_cloaked_and_roundtrippable() {
    let adapter = ClaudeCodeAdapter::new();
    let raw = br#"{
        "model":"claude-opus-5-5",
        "tools":[
            {"name":"browser_exec","description":"browse","input_schema":{"type":"object"}},
            {"name":"Read","description":"read","input_schema":{"type":"object"}},
            {"name":"tool_search","description":"search","input_schema":{"type":"object"}}
        ],
        "tool_choice":{"type":"tool","name":"tool_search"},
        "messages":[
            {"role":"user","content":"go"},
            {"role":"assistant","content":[
                {"type":"tool_use","id":"toolu_1","name":"browser_exec","input":{"url":"x"}}
            ]},
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":"toolu_1","content":"ok"}
            ]}
        ]
    }"#;
    let (json, _) = wrap_then_finalize(&adapter, "claude-opus-5-5", raw, Vec::new());

    // Declared tools, pinned choice and history all carry the cloaked names.
    let tools = json["tools"].as_array().expect("tools array");
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["BrowserExec", "Read", "ToolSearch"]);
    assert_eq!(json["tool_choice"]["name"].as_str(), Some("ToolSearch"));
    assert_eq!(
        json["messages"][1]["content"][0]["name"].as_str(),
        Some("BrowserExec")
    );

    // The client can map every cloaked name back to what it sent.
    let forward = cloaking::build_tool_rename_map(&json_before_cloak(raw));
    assert_eq!(
        forward.get("browser_exec").map(String::as_str),
        Some("BrowserExec")
    );
    assert_eq!(
        forward.get("tool_search").map(String::as_str),
        Some("ToolSearch")
    );
}

pub(super) fn json_before_cloak(raw: &[u8]) -> serde_json::Value {
    serde_json::from_slice(raw).expect("raw json")
}

#[test]
fn test_claude_code_finalize_drops_fallback_betas_after_strip() {
    let adapter = ClaudeCodeAdapter::new();
    let model = ModelId::new("claude-opus-5-5");
    let resolved = resolved_for(model.as_str());
    let raw = bytes::Bytes::from_static(br#"{"messages":[{"role":"user","content":"hi"}]}"#);
    let wrapped = adapter
        .wrap_request_body(raw, TargetFormat::Anthropic, &model, &resolved)
        .expect("wrap");
    let mut json: serde_json::Value = serde_json::from_slice(&wrapped).expect("json");
    json.as_object_mut().expect("object").remove("fallbacks");
    let mut headers = adapter.build_headers("test-key", TargetFormat::Anthropic, &model);
    headers.push((
        "Anthropic-Beta".into(),
        "server-side-fallback-2026-06-01,custom-beta".into(),
    ));
    adapter
        .finalize_request(
            bytes::Bytes::from(serde_json::to_vec(&json).expect("serialize")),
            &mut headers,
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .expect("finalize");
    let betas: Vec<&str> = headers
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case("anthropic-beta"))
        .flat_map(|(_, value)| value.split(','))
        .collect();
    assert!(!betas.contains(&"server-side-fallback-2026-06-01"));
    assert!(!betas.contains(&"fallback-credit-2026-06-01"));
    assert!(betas.contains(&"custom-beta"));
}

#[test]
fn test_claude_code_enum_trait_wrap_and_finalize_parity() {
    let _guard = crate::spoofer::claude_code::CLAUDE_CODE_TEST_LOCK
        .lock()
        .unwrap();
    crate::spoofer::claude_code::reset_dynamic_claude_code_overrides();

    let model = ModelId::new("claude-opus-5-5");
    let resolved = resolved_for(model.as_str());
    let adapter = crate::adapters::ProviderAdapterEnum::from_provider_id("claude-code")
        .expect("claude-code adapter");
    let input = bytes::Bytes::from_static(
        br#"{"model":"claude-opus-5-5","messages":[{"role":"user","content":"hi"}]}"#,
    );
    let inherent = adapter
        .wrap_request_body(input.clone(), TargetFormat::Anthropic, &model, &resolved)
        .expect("inherent wrap");
    let via_trait = ProviderAdapter::wrap_request_body(
        &adapter,
        input,
        TargetFormat::Anthropic,
        &model,
        &resolved,
    )
    .expect("trait wrap");
    assert_eq!(inherent, via_trait, "enum trait must forward wrapper");
    let mut headers = vec![("x-claude-code-session-id".into(), "session-parity".into())];
    let mut trait_headers = headers.clone();
    let inherent = adapter
        .finalize_request(
            inherent,
            &mut headers,
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .expect("inherent finalize");
    let via_trait = ProviderAdapter::finalize_request(
        &adapter,
        via_trait,
        &mut trait_headers,
        TargetFormat::Anthropic,
        &model,
        &resolved,
    )
    .expect("trait finalize");
    assert_eq!(inherent, via_trait, "enum trait must forward finalizer");
    assert_eq!(headers, trait_headers);
}

#[test]
fn test_default_finalize_request_is_identity() {
    // An adapter that does not override the hook must return the body and
    // headers untouched.
    let adapter = crate::adapters::MockAdapter::new("mock", "", AdapterFormat::Openai);
    let model = ModelId::new("gpt-4o");
    let resolved = resolved_for("gpt-4o");
    let body = bytes::Bytes::from_static(br#"{"model":"gpt-4o","messages":[]}"#);
    let mut headers = vec![("x-custom".to_string(), "keep-me".to_string())];

    let out = adapter
        .finalize_request(
            body.clone(),
            &mut headers,
            TargetFormat::Openai,
            &model,
            &resolved,
        )
        .expect("finalize");

    assert_eq!(out, body, "default finalize must not touch the body");
    assert_eq!(
        headers,
        vec![("x-custom".to_string(), "keep-me".to_string())]
    );
}

#[test]
fn finalizer_removes_all_stripped_capabilities() {
    let adapter = ClaudeCodeAdapter::new();
    let model = ModelId::new("claude-opus-5-5");
    let resolved = resolved_for(model.as_str());
    let raw = bytes::Bytes::from_static(br#"{"messages":[{"role":"user","content":"hi"}]}"#);
    let wrapped = adapter
        .wrap_request_body(raw, TargetFormat::Anthropic, &model, &resolved)
        .unwrap();
    let mut json: serde_json::Value = serde_json::from_slice(&wrapped).unwrap();
    for key in [
        "thinking",
        "output_config",
        "context_management",
        "fallbacks",
    ] {
        json.as_object_mut().unwrap().remove(key);
    }
    for section in ["system", "tools"] {
        if let Some(blocks) = json
            .get_mut(section)
            .and_then(serde_json::Value::as_array_mut)
        {
            for block in blocks {
                block.as_object_mut().unwrap().remove("cache_control");
            }
        }
    }
    if let Some(messages) = json
        .get_mut("messages")
        .and_then(serde_json::Value::as_array_mut)
    {
        for message in messages {
            if let Some(content) = message
                .get_mut("content")
                .and_then(serde_json::Value::as_array_mut)
            {
                for block in content {
                    block.as_object_mut().unwrap().remove("cache_control");
                }
            }
        }
    }
    let mut headers = adapter.build_headers("fixture", TargetFormat::Anthropic, &model);
    let out = adapter
        .finalize_request(
            bytes::Bytes::from(serde_json::to_vec(&json).unwrap()),
            &mut headers,
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .unwrap();
    let final_json: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let betas = headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("anthropic-beta"))
        .unwrap()
        .1
        .split(',')
        .collect::<Vec<_>>();
    for beta in [
        cloaking::BETA_THINKING_DISPLAY_UPDATES,
        cloaking::BETA_EFFORT,
        cloaking::BETA_EXTENDED_CACHE_TTL,
        cloaking::BETA_FALLBACKS[0],
        cloaking::BETA_FALLBACKS[1],
    ] {
        assert!(!betas.contains(&beta), "stale beta {beta}");
    }
    for key in [
        "thinking",
        "output_config",
        "context_management",
        "fallbacks",
    ] {
        assert!(final_json.get(key).is_none());
    }
}

#[test]
fn explicit_max_effort_and_forced_tool_survive() {
    let adapter = ClaudeCodeAdapter::new();
    let model = ModelId::new("claude-opus-5-5");
    let resolved = resolved_for(model.as_str());
    let input = serde_json::json!({"messages":[{"role":"user","content":"hi"}],"output_config":{"effort":"max"},"tool_choice":{"type":"tool","name":"GetWeather"}});
    let out = adapter
        .wrap_request_body(
            bytes::Bytes::from(serde_json::to_vec(&input).unwrap()),
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(json["output_config"]["effort"], "max");
    assert_eq!(json["tool_choice"], input["tool_choice"]);
    assert!(json.get("thinking").is_none());
}

pub(super) fn beta_list(value: &str) -> Vec<&str> {
    value
        .split(',')
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .collect()
}

pub(super) fn body_betas_for(model: &str, json: &serde_json::Value) -> Vec<String> {
    beta_list(&super::build_claude_code_betas_for_body(model, json, true))
        .into_iter()
        .map(ToString::to_string)
        .collect()
}

/// Captured body shape from cli-01.json / cli-03.json: adaptive thinking with
/// `display: updates`, effort high, context management and a 1h cache TTL.
pub(super) fn captured_cli_body() -> serde_json::Value {
    serde_json::json!({
        "model": "claude-opus-5-5",
        "messages": [{"role": "user", "content": "hello"}],
        "thinking": {"type": "adaptive", "display": "updates"},
        "context_management": {"edits": [{"type": "clear_thinking_20251015", "keep": "all"}]},
        "output_config": {"effort": "high"},
        "system": [
            {"type": "text", "text": "billing", "cache_control": {"type": "ephemeral"}},
            {"type": "text", "text": "identity", "cache_control": {"type": "ephemeral"}}
        ]
    })
}
