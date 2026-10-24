use super::*;

#[test]
fn test_claude_code_adapter_defaults() {
    let adapter = ClaudeCodeAdapter::new();
    assert_eq!(adapter.config().id.as_str(), "claude-code");
    assert_eq!(adapter.config().name, "Claude Code");
    assert_eq!(adapter.config().format, AdapterFormat::Anthropic);
    assert_eq!(adapter.config().auth_type, AdapterAuthType::OAuth);
    assert_eq!(
        adapter.models_dev_canonical_ids(),
        &["claude-code", "claude"]
    );
    assert!(adapter.metadata().supports_quota);
    assert!(adapter.metadata().quota_refresh_supported);
    assert!(adapter.metadata().requires_oauth);
}

#[test]
fn test_claude_code_build_chat_url() {
    let adapter = ClaudeCodeAdapter::new();
    let model = ModelId::new("claude-3-7-sonnet");
    assert_eq!(
        adapter.build_chat_url(TargetFormat::Anthropic, &model),
        "https://api.anthropic.com/v1/messages?beta=true"
    );

    let custom = ClaudeCodeAdapter::with_base_url("https://custom.anthropic.com/v1");
    assert_eq!(
        custom.build_chat_url(TargetFormat::Anthropic, &model),
        "https://custom.anthropic.com/v1/messages?beta=true"
    );
}

#[test]
fn test_claude_code_headers() {
    let _guard = crate::spoofer::claude_code::CLAUDE_CODE_TEST_LOCK
        .lock()
        .unwrap();
    crate::spoofer::claude_code::reset_dynamic_claude_code_overrides();
    let adapter = ClaudeCodeAdapter::new();
    let model = ModelId::new("claude-3-7-sonnet");
    let headers = adapter.build_headers("sk-ant-test-token", TargetFormat::Anthropic, &model);

    let get = |k: &str| {
        headers
            .iter()
            .find(|(hk, _)| hk.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(get("Authorization"), Some("Bearer sk-ant-test-token"));
    assert_eq!(
        get("anthropic-version"),
        Some(CLAUDE_CODE_ANTHROPIC_VERSION)
    );
    assert!(get("anthropic-beta").unwrap().contains("oauth-2025-04-20"));
    assert!(get("User-Agent").unwrap().starts_with("claude-cli/"));
    assert_eq!(get("Content-Type"), Some("application/json"));
    assert_eq!(get("X-App"), Some("cli"));
    assert_eq!(
        get("Anthropic-Dangerous-Direct-Browser-Access"),
        Some("true")
    );
    assert_eq!(get("X-Stainless-Retry-Count"), Some("0"));
    assert!(get("X-Claude-Code-Session-Id").is_some());
    assert!(get("x-client-request-id").is_some());
}

#[test]
fn test_claude_code_wrap_request_body() {
    let adapter = ClaudeCodeAdapter::new();
    let model = ModelId::new("claude-3-7-sonnet-20250219");
    let input = serde_json::json!({
        "model": "claude-3-7-sonnet-20250219",
        "system": "You are a helpful assistant from OpenCode.",
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hello"}]}],
        "max_tokens": 1024,
        "stream": true,
    });
    let input_bytes = bytes::Bytes::from(serde_json::to_vec(&input).unwrap());

    let resolved = openproxy_types::context::ResolvedTarget {
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
            model_id: model.clone(),
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
            claude_account_uuid: Some("53098eb0-6bbd-4c76-9f9c-09bc956164bc".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };

    let wrapped = adapter
        .wrap_request_body(input_bytes, TargetFormat::Anthropic, &model, &resolved)
        .expect("wrap_request_body should succeed");

    let val: serde_json::Value = serde_json::from_slice(&wrapped).unwrap();

    let user_id_str = val["metadata"]["user_id"]
        .as_str()
        .expect("metadata.user_id must be string");
    let user_id_obj: serde_json::Value =
        serde_json::from_str(user_id_str).expect("user_id must be JSON");
    assert_eq!(
        user_id_obj["account_uuid"],
        "53098eb0-6bbd-4c76-9f9c-09bc956164bc"
    );
    assert!(user_id_obj["device_id"].as_str().is_some());
    assert!(user_id_obj["session_id"].as_str().is_some());

    let sys_0 = val["system"][0]["text"]
        .as_str()
        .expect("system[0].text must be string");
    assert!(sys_0.starts_with("x-anthropic-billing-header:"));
    assert!(sys_0.contains("cch="));
    assert!(!sys_0.contains("cch=00000;"));
    assert!(sys_0.contains("cc_turn_origin=human;"));

    let sys_1 = val["system"][1]["text"]
        .as_str()
        .expect("system[1].text must be string");
    assert_eq!(sys_1, cloaking::CLAUDE_CODE_CLI_IDENTITY);
    assert_eq!(val["system"][1]["cache_control"]["type"], "ephemeral");

    // Caller system prompt should have been relocated into first user message
    let user_msg_content = &val["messages"][0]["content"];
    let reminder_found = user_msg_content.as_array().unwrap().iter().any(|part| {
        part["text"]
            .as_str()
            .is_some_and(|t| t.contains("<system-reminder>") && t.contains("OpenCode"))
    });
    assert!(
        reminder_found,
        "expected caller system prompt relocated to system-reminder in user message"
    );
}

#[test]
fn test_claude_code_opus_55_fallbacks_and_betas() {
    let adapter = ClaudeCodeAdapter::new();
    let model = ModelId::new("claude-opus-5-5");
    let input = serde_json::json!({
        "model": "claude-opus-5-5",
        "messages": [{"role": "user", "content": "hi"}],
    });
    let input_bytes = bytes::Bytes::from(serde_json::to_vec(&input).unwrap());

    let resolved = openproxy_types::context::ResolvedTarget {
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
            model_id: model.clone(),
            context_length: Some(1_000_000),
            max_output_tokens: Some(128000),
            model_type: "chat".into(),
            family: Some("claude".into()),
            active: true,
            ..Default::default()
        },
        api_key: "test-token".to_string(),
        ..Default::default()
    };

    let wrapped = adapter
        .wrap_request_body(input_bytes, TargetFormat::Anthropic, &model, &resolved)
        .expect("wrap_request_body should succeed");

    let val: serde_json::Value = serde_json::from_slice(&wrapped).unwrap();
    assert_eq!(
        val["fallbacks"][0]["model"], "claude-opus-4-8",
        "Opus 5.5 must include fallbacks to Opus 4.8"
    );

    let mut headers = adapter.build_headers("test-token", TargetFormat::Anthropic, &model);
    // Body-dependent betas are emitted only after the payload is finalized.
    let finalized = adapter
        .finalize_request(
            wrapped,
            &mut headers,
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .expect("finalize_request should succeed");
    let final_body: serde_json::Value = serde_json::from_slice(&finalized).unwrap();
    assert_eq!(final_body["fallbacks"][0]["model"], "claude-opus-4-8");
    let betas = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("anthropic-beta"))
        .map(|(_, v)| v.as_str())
        .expect("anthropic-beta must be present");

    assert!(betas.contains("per-turn-control-2026-07-01"));
    assert!(betas.contains("server-side-fallback-2026-06-01"));
    assert!(betas.contains("fallback-credit-2026-06-01"));
}

#[test]
fn test_parse_claude_code_usage_response() {
    let raw = serde_json::json!({
        "five_hour": {
            "utilization": 22.4,
            "resets_at": "2026-10-07T05:00:00Z"
        },
        "seven_day": {
            "utilization": 61.8,
            "resets_at": "2026-10-12T12:00:00Z"
        },
        "limits": [
            {
                "scope": {
                    "model": {
                        "display_name": "Fable"
                    }
                },
                "utilization": 95.0,
                "resets_at": "2026-10-12T12:00:00Z"
            }
        ]
    });

    let quota = parse_claude_code_usage_response(&raw);
    assert_eq!(quota.session_used, Some(22));
    assert_eq!(quota.session_limit, Some(100));
    assert_eq!(
        quota.session_reset_at.as_deref(),
        Some("2026-10-07T05:00:00Z")
    );
    assert_eq!(quota.weekly_used, Some(62));
    assert_eq!(quota.weekly_limit, Some(100));
    assert_eq!(
        quota.weekly_reset_at.as_deref(),
        Some("2026-10-12T12:00:00Z")
    );

    let details = quota.model_details.expect("model details");
    assert_eq!(details.len(), 1);
    assert_eq!(details[0].model_id, "Fable");
    assert_eq!(details[0].session_used, 95);
    assert_eq!(details[0].session_limit, 100);
}

#[test]
fn test_claude_code_builtin_models() {
    let models = claude_code_builtin_models();
    assert!(!models.is_empty());
    assert!(
        models
            .iter()
            .any(|m| m.model_id.as_str() == "claude-sonnet-5-5")
    );
    assert!(
        models
            .iter()
            .any(|m| m.model_id.as_str() == "claude-opus-4-6")
    );
    assert!(
        models
            .iter()
            .any(|m| m.model_id.as_str() == "claude-haiku-4-5-20251001")
    );
    assert!(
        models
            .iter()
            .all(|m| m.target_format == TargetFormat::Anthropic)
    );
}

#[test]
fn test_parse_claude_code_usage_response_skips_generic_limits() {
    let raw = serde_json::json!({
        "five_hour": {
            "utilization": 0.0,
            "resets_at": null
        },
        "seven_day": {
            "utilization": 100.0,
            "resets_at": "2026-10-09T10:59:59Z"
        },
        "limits": [
            {
                "kind": "session",
                "group": "session",
                "percent": 0,
                "severity": "normal",
                "resets_at": null,
                "scope": null,
                "is_active": false
            },
            {
                "kind": "weekly_all",
                "group": "weekly",
                "percent": 100,
                "severity": "critical",
                "resets_at": "2026-10-09T10:59:59Z",
                "scope": null,
                "is_active": true
            }
        ]
    });

    let quota = parse_claude_code_usage_response(&raw);
    assert_eq!(quota.session_used, Some(0));
    assert_eq!(quota.session_limit, Some(100));
    assert_eq!(quota.weekly_used, Some(100));
    assert_eq!(quota.weekly_limit, Some(100));
    assert_eq!(
        quota.weekly_reset_at.as_deref(),
        Some("2026-10-09T10:59:59Z")
    );
    // Generic session/weekly limits must not produce fake "Model" entries
    assert!(quota.model_details.is_none());
}

#[test]
fn test_parse_claude_models_response() {
    let raw = serde_json::json!({
        "data": [
            {
                "id": "claude-sonnet-5-5",
                "display_name": "Claude Sonnet 5.5",
                "max_input_tokens": 1000000,
                "max_tokens": 128000,
                "capabilities": {
                    "effort": { "supported": true }
                }
            },
            {
                "id": "claude-haiku-4-5-20251001",
                "display_name": "Claude Haiku 4.5",
                "max_input_tokens": 200000,
                "max_tokens": 64000,
                "capabilities": {
                    "effort": { "supported": false }
                }
            }
        ]
    });

    let models = parse_claude_models_response(&raw).expect("parsed dynamic models");
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].model_id.as_str(), "claude-sonnet-5-5");
    assert_eq!(models[0].display_name.as_deref(), Some("Claude Sonnet 5.5"));
    assert_eq!(models[0].context_length, Some(1_000_000));
    assert_eq!(models[0].max_output_tokens, Some(128_000));
    assert_eq!(
        models[0].capabilities.as_ref().and_then(|c| c.reasoning),
        Some(true)
    );

    assert_eq!(models[1].model_id.as_str(), "claude-haiku-4-5-20251001");
    assert_eq!(
        models[1].capabilities.as_ref().and_then(|c| c.reasoning),
        Some(false)
    );
}

#[test]
fn test_claude_code_rolling_cache_breakpoint_advances_multi_turn() {
    let adapter = ClaudeCodeAdapter::new();
    let model = ModelId::new("claude-opus-5-5");

    let multi_turn_input = serde_json::json!({
        "model": "claude-opus-5-5",
        "messages": [
            {"role": "user", "content": "turn 1 user"},
            {"role": "assistant", "content": "turn 1 assistant"},
            {"role": "user", "content": "turn 2 user"},
            {"role": "assistant", "content": "turn 2 assistant"},
            {"role": "user", "content": "turn 3 user"}
        ]
    });
    let input_bytes = bytes::Bytes::from(serde_json::to_vec(&multi_turn_input).unwrap());

    let resolved = openproxy_types::context::ResolvedTarget {
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
            model_id: model.clone(),
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
            claude_account_uuid: Some("53098eb0-6bbd-4c76-9f9c-09bc956164bc".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };

    let wrapped = adapter
        .wrap_request_body(input_bytes, TargetFormat::Anthropic, &model, &resolved)
        .expect("wrap_request_body should succeed");

    let val: serde_json::Value = serde_json::from_slice(&wrapped).unwrap();

    // System prompt retains identity cache breakpoint
    assert_eq!(val["system"][1]["cache_control"]["type"], "ephemeral");

    // The rolling breakpoint MUST land on the latest user turn (index 4)
    assert!(val["messages"][0].get("cache_control").is_none());
    assert!(val["messages"][2].get("cache_control").is_none());
    assert_eq!(
        val["messages"][4]["content"][0]["cache_control"]["type"],
        "ephemeral"
    );

    // Total cache control count must be <= 4
    let total_cc = crate::anthropic_cache::count_cache_controls(&val);
    assert_eq!(total_cc, 2);
}

#[test]
fn test_claude_dynamic_version_in_billing_header() {
    let _guard = crate::spoofer::claude_code::CLAUDE_CODE_TEST_LOCK
        .lock()
        .unwrap();
    crate::spoofer::claude_code::reset_dynamic_claude_code_overrides();

    let adapter = ClaudeCodeAdapter::new();
    let model = ModelId::new("claude-sonnet-5");
    let resolved = openproxy_types::context::ResolvedTarget {
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
            model_id: model.clone(),
            context_length: Some(200_000),
            max_output_tokens: Some(8192),
            model_type: "chat".into(),
            family: Some("claude".into()),
            active: true,
            ..Default::default()
        },
        api_key: "test-token".to_string(),
        ..Default::default()
    };

    // 1. In-memory dynamic version override
    crate::spoofer::claude_code::set_dynamic_claude_code_version("2.1.295");
    let input = serde_json::json!({
        "messages": [
            {"role": "user", "content": "Hello dynamic claude"}
        ]
    });
    let input_bytes = bytes::Bytes::from(serde_json::to_vec(&input).unwrap());
    let wrapped = adapter
        .wrap_request_body(input_bytes, TargetFormat::Anthropic, &model, &resolved)
        .expect("wrap_request_body");
    let val: serde_json::Value = serde_json::from_slice(&wrapped).unwrap();
    let billing = val["system"][0]["text"].as_str().unwrap();
    assert!(
        billing.starts_with("x-anthropic-billing-header: cc_version=2.1.295."),
        "expected dynamic version 2.1.295, got: {billing}"
    );

    // 2. Caller provided higher version in billing header is preserved
    let input_with_caller_ver = serde_json::json!({
        "system": [
            {"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.305.abc; cc_entrypoint=cli; cch=00000;"}
        ],
        "messages": [
            {"role": "user", "content": "Hello newer claude version"}
        ]
    });
    let input_bytes2 = bytes::Bytes::from(serde_json::to_vec(&input_with_caller_ver).unwrap());
    let wrapped2 = adapter
        .wrap_request_body(input_bytes2, TargetFormat::Anthropic, &model, &resolved)
        .expect("wrap_request_body");
    let val2: serde_json::Value = serde_json::from_slice(&wrapped2).unwrap();
    let billing2 = val2["system"][0]["text"].as_str().unwrap();
    assert!(
        billing2.starts_with("x-anthropic-billing-header: cc_version=2.1.305."),
        "expected caller version 2.1.305 preserved, got: {billing2}"
    );

    crate::spoofer::claude_code::reset_dynamic_claude_code_overrides();
}

fn resolved_for(model_id: &str) -> openproxy_types::context::ResolvedTarget {
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

fn wrap_then_finalize(
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

fn cch_of(json: &serde_json::Value) -> String {
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

fn json_before_cloak(raw: &[u8]) -> serde_json::Value {
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

fn beta_list(value: &str) -> Vec<&str> {
    value
        .split(',')
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .collect()
}

fn body_betas_for(model: &str, json: &serde_json::Value) -> Vec<String> {
    beta_list(&cloaking::build_claude_code_betas_for_body(
        model, json, true,
    ))
    .into_iter()
    .map(ToString::to_string)
    .collect()
}

/// Captured body shape from cli-01.json / cli-03.json: adaptive thinking with
/// `display: updates`, effort high, context management and a 1h cache TTL.
fn captured_cli_body() -> serde_json::Value {
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

#[test]
fn test_betas_from_body_display_updates_drops_redact_thinking() {
    let json = captured_cli_body();
    let betas = body_betas_for("claude-opus-5-5", &json);

    // The display-update gate beta is present exactly once.
    assert_eq!(
        betas
            .iter()
            .filter(|b| *b == cloaking::BETA_THINKING_DISPLAY_UPDATES)
            .count(),
        1,
        "captured cli body must emit the display-update beta once: {betas:?}"
    );
    // Thinking is active, so redact-thinking is withdrawn.
    assert!(
        !betas.iter().any(|b| b == "redact-thinking-2026-02-12"),
        "redact-thinking must be dropped when thinking is active: {betas:?}"
    );
    // The captured base set is preserved verbatim.
    for expected in [
        "claude-code-20250219",
        "oauth-2025-04-20",
        cloaking::BETA_MID_CONV_SYSTEM,
        cloaking::BETA_MID_CONV_TOOL,
        "per-turn-control-2026-07-01",
        "dangerous-tool-use-2026-09-03",
        "afk-mode-2026-01-31",
        "effort-2025-11-24",
    ] {
        assert!(
            betas.iter().any(|b| b == expected),
            "missing {expected}: {betas:?}"
        );
    }
}

#[test]
fn test_betas_from_body_extended_ttl_gate() {
    // cli-00 style: no thinking, no 1h TTL.
    let without_ttl = serde_json::json!({
        "model": "claude-opus-5-5",
        "messages": [],
        "system": [{"type": "text", "text": "x"}],
    });
    let betas = body_betas_for("claude-opus-5-5", &without_ttl);
    assert!(
        !betas.iter().any(|b| b == "extended-cache-ttl-2025-04-11"),
        "no 1h TTL in body means no extended-cache-ttl beta: {betas:?}"
    );

    // A 1h TTL anywhere in the final body enables it.
    let with_ttl = serde_json::json!({
        "model": "claude-opus-5-5",
        "messages": [],
        "system": [{"type": "text", "text": "x", "cache_control": {"type": "ephemeral", "ttl": "1h"}}],
    });
    let betas = body_betas_for("claude-opus-5-5", &with_ttl);
    assert!(
        betas.iter().any(|b| b == "extended-cache-ttl-2025-04-11"),
        "1h TTL in body must add extended-cache-ttl: {betas:?}"
    );

    // A caller-pinned 5m TTL is never promoted by the beta set.
    let five_minutes = serde_json::json!({
        "model": "claude-opus-5-5",
        "messages": [],
        "system": [{"type": "text", "text": "x", "cache_control": {"type": "ephemeral", "ttl": "5m"}}],
    });
    let betas = body_betas_for("claude-opus-5-5", &five_minutes);
    assert!(
        !betas.iter().any(|b| b == "extended-cache-ttl-2025-04-11"),
        "explicit 5m TTL must not gain the 1h TTL beta: {betas:?}"
    );
}

#[test]
fn test_betas_structured_outputs_from_body() {
    // cli-00.json carries an output_config.format json_schema and no thinking.
    let json = serde_json::json!({
        "model": "claude-opus-5-5",
        "messages": [],
        "output_config": {
            "effort": "medium",
            "format": {"type": "json_schema", "schema": {"type": "object"}}
        },
    });
    let betas = body_betas_for("claude-opus-5-5", &json);
    assert!(
        betas.iter().any(|b| b == "structured-outputs-2025-12-15"),
        "structured output body must advertise structured-outputs: {betas:?}"
    );
    // No thinking present, so redact-thinking stays and the display beta is gone.
    assert!(
        betas.iter().any(|b| b == "redact-thinking-2026-02-12"),
        "redact-thinking is the default when thinking is off: {betas:?}"
    );
    assert!(
        !betas
            .iter()
            .any(|b| b == cloaking::BETA_THINKING_DISPLAY_UPDATES),
        "display updates need active updates thinking: {betas:?}"
    );
}

#[test]
fn test_legacy_models_do_not_get_thinking_defaults() {
    let adapter = ClaudeCodeAdapter::new();
    let model = ModelId::new("claude-3-7-sonnet-20250219");
    let raw =
        br#"{"model":"claude-3-7-sonnet-20250219","messages":[{"role":"user","content":"hi"}]}"#;
    let resolved = resolved_for(model.as_str());
    let wrapped = adapter
        .wrap_request_body(
            bytes::Bytes::from_static(raw),
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .expect("wrap");
    let json: serde_json::Value = serde_json::from_slice(&wrapped).unwrap();

    // Legacy models never receive adaptive thinking or an effort default.
    assert!(
        json.get("thinking").is_none(),
        "legacy model must not gain thinking: {json}"
    );
    assert!(
        json.get("output_config").is_none(),
        "legacy model must not gain effort defaults: {json}"
    );

    let betas = body_betas_for(model.as_str(), &json);
    assert!(
        !betas.iter().any(|b| b == cloaking::BETA_MID_CONV_SYSTEM),
        "legacy model must not get the mid-conversation system beta: {betas:?}"
    );
    assert!(
        !betas
            .iter()
            .any(|b| b == cloaking::BETA_THINKING_DISPLAY_UPDATES),
        "legacy model must never get display updates: {betas:?}"
    );
    assert!(
        !betas.iter().any(|b| b == "effort-2025-11-24"),
        "legacy model must not advertise the effort beta: {betas:?}"
    );
}

#[test]
fn test_thinking_defaults_are_injected_for_supported_models() {
    let adapter = ClaudeCodeAdapter::new();
    let model = ModelId::new("claude-opus-5-5");
    let raw = br#"{"model":"claude-opus-5-5","messages":[{"role":"user","content":"hi"}]}"#;
    let resolved = resolved_for(model.as_str());
    let wrapped = adapter
        .wrap_request_body(
            bytes::Bytes::from_static(raw),
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .expect("wrap");
    let json: serde_json::Value = serde_json::from_slice(&wrapped).unwrap();

    assert_eq!(json["thinking"]["type"], "adaptive");
    assert_eq!(json["thinking"]["display"], "updates");
    assert_eq!(json["output_config"]["effort"], "high");
    assert_eq!(
        json["context_management"]["edits"][0]["type"],
        "clear_thinking_20251015"
    );
    assert_eq!(json["context_management"]["edits"][0]["keep"], "all");
    // max_tokens is never invented here.
    assert!(json.get("max_tokens").is_none());
}

#[test]
fn test_explicit_user_fields_are_never_overwritten() {
    let adapter = ClaudeCodeAdapter::new();
    let model = ModelId::new("claude-opus-5-5");
    let raw = br#"{"model":"claude-opus-5-5","messages":[{"role":"user","content":"hi"}],"max_tokens":2048,"thinking":{"type":"disabled"},"output_config":{"effort":"medium"},"context_management":{"edits":[]},"tool_choice":{"type":"tool","name":"GetWeather"}}"#;
    let resolved = resolved_for(model.as_str());
    let wrapped = adapter
        .wrap_request_body(
            bytes::Bytes::from_static(raw),
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .expect("wrap");
    let json: serde_json::Value = serde_json::from_slice(&wrapped).unwrap();

    assert_eq!(json["max_tokens"], 2048, "caller max_tokens must survive");
    assert_eq!(
        json["thinking"]["type"], "disabled",
        "explicit thinking preserved"
    );
    assert_eq!(
        json["output_config"]["effort"], "medium",
        "explicit effort preserved"
    );
    assert_eq!(
        json["context_management"]["edits"]
            .as_array()
            .unwrap()
            .len(),
        0,
        "empty context management preserved"
    );
    // No active thinking means a tool-pinned tool_choice is not demoted.
    assert_eq!(json["tool_choice"]["type"], "tool");
    assert_eq!(json["tool_choice"]["name"], "GetWeather");
}

#[test]
fn test_cache_ttl_promotion_preserves_explicit_ttl() {
    let adapter = ClaudeCodeAdapter::new();
    let model = ModelId::new("claude-opus-5-5");
    let raw = br#"{"model":"claude-opus-5-5","messages":[{"role":"user","content":"hi"}],"system":[{"type":"text","text":"prompt","cache_control":{"type":"ephemeral","ttl":"5m"}}]}"#;
    let resolved = resolved_for(model.as_str());
    let wrapped = adapter
        .wrap_request_body(
            bytes::Bytes::from_static(raw),
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .expect("wrap");
    let json: serde_json::Value = serde_json::from_slice(&wrapped).unwrap();

    // The injected identity breakpoint carries the native 1h TTL.
    let identity = json["system"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["text"].as_str() == Some(cloaking::CLAUDE_CODE_CLI_IDENTITY))
        .expect("identity block");
    assert_eq!(identity["cache_control"]["type"], "ephemeral");
    assert_eq!(identity["cache_control"]["ttl"], "5m");

    // A caller's explicit 5m TTL on their own prompt block is never promoted.
    let caller_ttl = json["system"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|b| b.get("cache_control").and_then(|c| c.get("ttl")))
        .filter_map(|t| t.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        caller_ttl,
        vec!["5m"],
        "explicit 5m TTL must survive verbatim"
    );
}

#[test]
fn test_finalizer_pins_billing_version_and_identity_headers() {
    let _guard = crate::spoofer::claude_code::CLAUDE_CODE_TEST_LOCK
        .lock()
        .unwrap();
    crate::spoofer::claude_code::reset_dynamic_claude_code_overrides();

    let adapter = ClaudeCodeAdapter::new();
    let model = ModelId::new("claude-opus-5-5");
    let resolved = resolved_for(model.as_str());
    let raw = br#"{"model":"claude-opus-5-5","messages":[{"role":"user","content":"hi"}]}"#;
    let wrapped = adapter
        .wrap_request_body(
            bytes::Bytes::from_static(raw),
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .expect("wrap");

    let mut headers = adapter.build_headers("test-token", TargetFormat::Anthropic, &model);
    // A stale UA from an older profile must be brought back into agreement.
    for (k, v) in &mut headers {
        if k.eq_ignore_ascii_case("user-agent") {
            *v = "claude-cli/2.1.200 (external, cli)".into();
        }
    }

    let finalized = adapter
        .finalize_request(
            wrapped,
            &mut headers,
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .expect("finalize");
    let json: serde_json::Value = serde_json::from_slice(&finalized).unwrap();

    let version = crate::spoofer::claude_code::current_claude_code_version();
    let billing = json["system"][0]["text"].as_str().unwrap();
    assert!(
        billing.starts_with(&format!(
            "x-anthropic-billing-header: cc_version={version}."
        )),
        "billing version must match the pinned snapshot: {billing}"
    );

    let get = |key: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.clone())
    };
    assert_eq!(
        get("user-agent").as_deref(),
        Some(crate::spoofer::claude_code::DEFAULT_CLAUDE_CODE_UA)
    );
    assert_eq!(
        get("x-stainless-package-version").as_deref(),
        Some(crate::spoofer::claude_code::DEFAULT_CLAUDE_CODE_PACKAGE_VERSION)
    );
    assert_eq!(
        get("x-stainless-runtime-version").as_deref(),
        Some(crate::spoofer::claude_code::DEFAULT_CLAUDE_CODE_RUNTIME_VERSION)
    );

    // The body is signed over the final bytes.
    assert_ne!(cch_of(&json), "00000");
    let mut resigned = finalized.to_vec();
    signing::sign_anthropic_messages_body(&mut resigned).expect("re-sign");
    assert_eq!(resigned, finalized.to_vec(), "cch must be stable");
}

#[test]
fn test_finalizer_does_not_invent_identity_headers() {
    let adapter = ClaudeCodeAdapter::new();
    let model = ModelId::new("claude-opus-5-5");
    let resolved = resolved_for(model.as_str());
    let raw = br#"{"model":"claude-opus-5-5","messages":[{"role":"user","content":"hi"}]}"#;
    let wrapped = adapter
        .wrap_request_body(
            bytes::Bytes::from_static(raw),
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .expect("wrap");

    let mut headers = vec![("x-claude-code-session-id".to_string(), "s".to_string())];
    let finalized = adapter
        .finalize_request(
            wrapped,
            &mut headers,
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .expect("finalize");
    let _ = finalized;

    assert!(
        !headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("user-agent")),
        "finalizer must not inject an identity header the caller never sent"
    );
    assert_eq!(
        headers.len(),
        2,
        "only session and betas headers remain: {headers:?}"
    );
}

#[test]
fn test_finalizer_betas_derived_from_post_strip_body() {
    let adapter = ClaudeCodeAdapter::new();
    let model = ModelId::new("claude-opus-5-5");
    let resolved = resolved_for(model.as_str());
    let raw = br#"{"model":"claude-opus-5-5","messages":[{"role":"user","content":"hi"}],"thinking":{"type":"adaptive","display":"omitted"}}"#;

    let wrapped = adapter
        .wrap_request_body(
            bytes::Bytes::from_static(raw),
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .expect("wrap");

    let mut headers = adapter.build_headers("k", TargetFormat::Anthropic, &model);
    // Simulate the pipeline slotting a provider param that strips right before
    // the finalizer runs.
    let mut body: serde_json::Value = serde_json::from_slice(&wrapped).unwrap();
    body.as_object_mut()
        .unwrap()
        .insert("temperature".into(), serde_json::json!(0.3));
    let stripped = bytes::Bytes::from(serde_json::to_vec(&body).unwrap());
    let mut stripped: serde_json::Value = serde_json::from_slice(&stripped).unwrap();
    stripped.as_object_mut().unwrap().remove("temperature");
    stripped.as_object_mut().unwrap().remove("fallbacks");
    let stripped = bytes::Bytes::from(serde_json::to_vec(&stripped).unwrap());

    let finalized = adapter
        .finalize_request(
            stripped,
            &mut headers,
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .expect("finalize");
    let json: serde_json::Value = serde_json::from_slice(&finalized).unwrap();

    let betas = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("anthropic-beta"))
        .map(|(_, v)| {
            beta_list(v)
                .into_iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
        .expect("beta header");
    // An explicit omitted display must not advertise update streaming.
    assert!(
        !betas
            .iter()
            .any(|b| b == "thinking-display-updates-2026-08-18"),
        "unexpected display updates beta: {betas:?}"
    );
    assert!(
        !betas.iter().any(|b| b == "redact-thinking-2026-02-12"),
        "thinking is active so redact-thinking must be absent: {betas:?}"
    );
    assert!(
        !betas.iter().any(|b| b == "fallback-credit-2026-06-01"),
        "stale fallback betas must be dropped after strip: {betas:?}"
    );
    assert!(
        json.get("temperature").is_none(),
        "stripped param must not be re-filled after strip"
    );
}

#[test]
fn test_finalizer_reconciles_ua_with_pinned_version() {
    let _guard = crate::spoofer::claude_code::CLAUDE_CODE_TEST_LOCK
        .lock()
        .unwrap();
    crate::spoofer::claude_code::reset_dynamic_claude_code_overrides();
    crate::spoofer::claude_code::set_dynamic_claude_code_version("9.9.999");
    crate::spoofer::claude_code::set_dynamic_claude_code_ua("claude-cli/1.0.0 (external, cli)");

    let adapter = ClaudeCodeAdapter::new();
    let model = ModelId::new("claude-opus-5-5");
    let resolved = resolved_for(model.as_str());
    let raw = br#"{"model":"claude-opus-5-5","messages":[{"role":"user","content":"hi"}]}"#;
    let wrapped = adapter
        .wrap_request_body(
            bytes::Bytes::from_static(raw),
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .expect("wrap");
    let mut headers = adapter.build_headers("k", TargetFormat::Anthropic, &model);

    let finalized = adapter
        .finalize_request(
            wrapped,
            &mut headers,
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .expect("finalize");
    let json: serde_json::Value = serde_json::from_slice(&finalized).unwrap();

    // The UA is left exactly as overridden; it is never rewritten.
    let ua = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("user-agent"))
        .map(|(_, v)| v.clone())
        .expect("user-agent present");
    assert_eq!(ua, "claude-cli/1.0.0 (external, cli)");

    // The billing cc_version follows the UA marker, so payload and UA agree
    // even though the version override disagreed.
    let billing = json["system"][0]["text"].as_str().unwrap();
    assert!(
        billing.starts_with("x-anthropic-billing-header: cc_version=1.0.0."),
        "expected UA-derived version 1.0.0, got: {billing}"
    );

    let billing = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("anthropic-version"))
        .map(|(_, v)| v.clone());
    assert!(
        billing.is_some(),
        "anthropic-version header must survive: {headers:?}"
    );

    crate::spoofer::claude_code::reset_dynamic_claude_code_overrides();
}

#[test]
fn test_finalizer_preserves_explicit_ua_override() {
    let _guard = crate::spoofer::claude_code::CLAUDE_CODE_TEST_LOCK
        .lock()
        .unwrap();
    crate::spoofer::claude_code::reset_dynamic_claude_code_overrides();
    crate::spoofer::claude_code::set_dynamic_claude_code_ua("custom-agent/7.7.7");

    let adapter = ClaudeCodeAdapter::new();
    let model = ModelId::new("claude-opus-5-5");
    let resolved = resolved_for(model.as_str());
    let raw = br#"{"model":"claude-opus-5-5","messages":[{"role":"user","content":"hi"}]}"#;
    let wrapped = adapter
        .wrap_request_body(
            bytes::Bytes::from_static(raw),
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .expect("wrap");
    let mut headers = adapter.build_headers("k", TargetFormat::Anthropic, &model);

    let finalized = adapter
        .finalize_request(
            wrapped,
            &mut headers,
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .expect("finalize");
    let json: serde_json::Value = serde_json::from_slice(&finalized).unwrap();

    let ua = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("user-agent"))
        .map(|(_, v)| v.clone())
        .expect("user-agent present");
    // A UA that is not in the CLI's derived shape is an explicit override and
    // is left untouched.
    assert_eq!(ua, "custom-agent/7.7.7");

    // Without a claude-cli marker the profile version still drives billing.
    let billing = json["system"][0]["text"].as_str().unwrap();
    let profile_version = crate::spoofer::claude_code_profile::current_profile().cli_version;
    assert!(
        billing.starts_with(&format!(
            "x-anthropic-billing-header: cc_version={profile_version}."
        )),
        "expected profile version {profile_version}, got: {billing}"
    );

    crate::spoofer::claude_code::reset_dynamic_claude_code_overrides();
}

/// El pin congela UNA generación: headers, body, betas y TTL salen del snapshot
/// de la petición, no del perfil activo del runtime. Así un refresco en
/// paralelo no puede mezclar la versión de los headers con los defaults del
/// body.
#[test]
fn test_pinned_contract_drives_headers_and_body_of_one_generation() {
    use crate::client_contract::{
        Candidate, ClientIdentity, ContractId, IDENTITY_ORIGIN_OFFICIAL, Snapshot,
    };
    use crate::spoofer::claude_code_profile::ClaudeCodeProfile;

    let _guard = crate::spoofer::claude_code::CLAUDE_CODE_TEST_LOCK
        .lock()
        .unwrap();
    crate::spoofer::claude_code::reset_dynamic_claude_code_overrides();

    // Una generación distinta a la activa, y con un TTL que delata la fuga.
    let profile = ClaudeCodeProfile {
        cli_version: "2.1.999".to_string(),
        package_version: "0.999.0".to_string(),
        runtime_version: "v99.0.0".to_string(),
        cache_ttl: "5m".to_string(),
        betas: vec!["claude-code-20250219".to_string()],
        request_defaults: serde_json::json!({
            "thinking": {"type": "adaptive", "display": "updates"},
            "output_config": {"effort": "high"},
            "context_management": {}
        }),
    };
    let candidate = Candidate::without_catalog(
        ContractId::ClaudeCode,
        ClientIdentity::versioned("2.1.999", IDENTITY_ORIGIN_OFFICIAL),
        crate::spoofer::claude_code_profile::CLAUDE_CODE_RUNTIME_RULE_SET_ID,
        crate::spoofer::claude_code_profile::CLAUDE_CODE_RUNTIME_EXTRACTOR_VERSION,
        Vec::new(),
        profile,
    );
    let snapshot = Snapshot::from_candidate(candidate).expect("snapshot de prueba");

    let adapter = ClaudeCodeAdapter::new().with_pinned_contract(Arc::new(snapshot));
    assert!(adapter.pinned_contract().is_some());

    let model = ModelId::new("claude-opus-5-5");
    let resolved = resolved_for(model.as_str());
    let raw = br#"{"model":"claude-opus-5-5","messages":[{"role":"user","content":"hi"}],"system":[{"type":"text","text":"base"}]}"#;
    let wrapped = adapter
        .wrap_request_body(
            bytes::Bytes::from_static(raw),
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .expect("wrap");
    let mut headers = adapter.build_headers("k", TargetFormat::Anthropic, &model);
    let finalized = adapter
        .finalize_request(
            wrapped,
            &mut headers,
            TargetFormat::Anthropic,
            &model,
            &resolved,
        )
        .expect("finalize");
    let json: serde_json::Value = serde_json::from_slice(&finalized).unwrap();

    // La UA y las versiones stainless son del snapshot, no del runtime.
    let find = |k: &str| {
        headers
            .iter()
            .find(|(hk, _)| hk.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.clone())
    };
    assert_eq!(
        find("user-agent").as_deref(),
        Some("claude-cli/2.1.999 (external, cli)")
    );
    assert_eq!(
        find("x-stainless-package-version").as_deref(),
        Some("0.999.0")
    );
    assert_eq!(
        find("x-stainless-runtime-version").as_deref(),
        Some("v99.0.0")
    );

    // El billing viaja con la MISMA versión que los headers.
    let billing = json["system"][0]["text"].as_str().unwrap();
    assert!(
        billing.starts_with("x-anthropic-billing-header: cc_version=2.1.999."),
        "expected pinned version 2.1.999, got: {billing}"
    );

    // Los defaults y el TTL del cache también son de la generación congelada.
    assert_eq!(json["thinking"]["display"], "updates");
    assert_eq!(json["output_config"]["effort"], "high");
    let cc = &json["system"][1]["cache_control"];
    assert_eq!(
        cc["ttl"], "5m",
        "el TTL debe salir del snapshot congelado: {cc}"
    );

    crate::spoofer::claude_code::reset_dynamic_claude_code_overrides();
}
