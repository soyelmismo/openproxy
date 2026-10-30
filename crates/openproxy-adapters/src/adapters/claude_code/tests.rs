use super::adapter_impl::*;
use super::betas::*;
use super::*;
use finalize_tests::resolved_target_for;

mod beta_tests;
mod finalize_tests;

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

    let resolved = resolved_target_for(
        model.as_str(),
        200_000,
        8192,
        Some(openproxy_types::context::CustomProviderMeta {
            access_token: "test-token".to_string(),
            claude_account_uuid: Some("53098eb0-6bbd-4c76-9f9c-09bc956164bc".to_string()),
            ..Default::default()
        }),
    );

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

    let resolved = resolved_target_for(model.as_str(), 1_000_000, 128_000, None);

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

    let resolved = resolved_target_for(
        model.as_str(),
        200_000,
        8192,
        Some(openproxy_types::context::CustomProviderMeta {
            access_token: "test-token".to_string(),
            claude_account_uuid: Some("53098eb0-6bbd-4c76-9f9c-09bc956164bc".to_string()),
            ..Default::default()
        }),
    );

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
    let resolved = resolved_target_for(model.as_str(), 200_000, 8192, None);

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
