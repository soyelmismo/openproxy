use super::*;

#[test]
fn test_claude_code_adapter_defaults() {
    let adapter = ClaudeCodeAdapter::new();
    assert_eq!(adapter.config().id.as_str(), "claude-code");
    assert_eq!(adapter.config().name, "Claude Code");
    assert_eq!(adapter.config().format, AdapterFormat::Anthropic);
    assert_eq!(adapter.config().auth_type, AdapterAuthType::OAuth);
    assert_eq!(adapter.models_dev_canonical_ids(), &["claude-code", "claude"]);
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
        "https://api.anthropic.com/v1/messages"
    );

    let custom = ClaudeCodeAdapter::with_base_url("https://custom.anthropic.com/v1");
    assert_eq!(
        custom.build_chat_url(TargetFormat::Anthropic, &model),
        "https://custom.anthropic.com/v1/messages"
    );
}

#[test]
fn test_claude_code_headers() {
    let adapter = ClaudeCodeAdapter::new();
    let model = ModelId::new("claude-3-7-sonnet");
    let headers = adapter.build_headers("sk-ant-test-token", TargetFormat::Anthropic, &model);

    let get = |k: &str| headers.iter().find(|(hk, _)| hk.eq_ignore_ascii_case(k)).map(|(_, v)| v.as_str());
    assert_eq!(get("Authorization"), Some("Bearer sk-ant-test-token"));
    assert_eq!(get("anthropic-version"), Some(CLAUDE_CODE_ANTHROPIC_VERSION));
    assert!(get("anthropic-beta").unwrap().contains("oauth-2025-04-20"));
    assert!(get("User-Agent").unwrap().starts_with("claude-cli/"));
    assert_eq!(get("Content-Type"), Some("application/json"));
    assert_eq!(get("X-App"), Some("cli"));
    assert_eq!(get("Anthropic-Dangerous-Direct-Browser-Access"), Some("true"));
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
            account_id: None,
            model_row_id: Some(openproxy_types::ModelRowId(1)),
            sub_combo_id: None,
            priority_order: 0,
            weight: 100,
            active: true,
            rate_limit_scope: openproxy_types::providers::RateLimitScope::Account,
            cooldown_mode: None,
            cooldown_base_secs: None,
            cooldown_max_secs: None,
            cooldown_factor: None,
            thinking_effort: None,
            description: None,
        },
        model: openproxy_types::Model {
            row_id: openproxy_types::ModelRowId(1),
            provider_id: openproxy_types::ProviderId::new("claude-code"),
            target_format: openproxy_types::TargetFormat::Anthropic,
            discovered_at: openproxy_types::now_unix_secs_str().into_boxed_str(),
            expires_at: None,
            model_id: model.clone(),
            display_name: None,
            context_length: Some(200_000),
            max_output_tokens: Some(8192),
            model_type: "chat".into(),
            family: Some("claude".into()),
            input_modalities_json: None,
            output_modalities_json: None,
            capabilities_json: None,
            timeout_overrides_json: None,
            active: true,
            last_test_status: None,
            last_test_at: None,
            custom: false,
            manually_disabled_at: None,
        },
        api_key: "test-token".to_string(),
        api_key_label: None,
        custom_meta: Some(openproxy_types::context::CustomProviderMeta {
            access_token: "test-token".to_string(),
            maybe_refresh: None,
            kiro_region: None,
            kiro_profile_arn: None,
            antigravity_project: None,
            antigravity_metadata: None,
            codex_workspace_id: None,
            claude_account_uuid: Some("53098eb0-6bbd-4c76-9f9c-09bc956164bc".to_string()),
            claude_metadata: None,
        }),
    };

    let wrapped = adapter
        .wrap_request_body(input_bytes, TargetFormat::Anthropic, &model, &resolved)
        .expect("wrap_request_body should succeed");

    let val: serde_json::Value = serde_json::from_slice(&wrapped).unwrap();

    let user_id_str = val["metadata"]["user_id"].as_str().expect("metadata.user_id must be string");
    let user_id_obj: serde_json::Value = serde_json::from_str(user_id_str).expect("user_id must be JSON");
    assert_eq!(user_id_obj["account_uuid"], "53098eb0-6bbd-4c76-9f9c-09bc956164bc");
    assert!(user_id_obj["device_id"].as_str().is_some());
    assert!(user_id_obj["session_id"].as_str().is_some());

    let sys_0 = val["system"][0]["text"].as_str().expect("system[0].text must be string");
    assert!(sys_0.starts_with("x-anthropic-billing-header:"));
    assert!(sys_0.contains("cch="));
    assert!(!sys_0.contains("cch=00000;"));
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
    assert_eq!(quota.session_reset_at.as_deref(), Some("2026-10-07T05:00:00Z"));
    assert_eq!(quota.weekly_used, Some(62));
    assert_eq!(quota.weekly_limit, Some(100));
    assert_eq!(quota.weekly_reset_at.as_deref(), Some("2026-10-12T12:00:00Z"));

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
    assert!(models.iter().any(|m| m.model_id.as_str() == "claude-sonnet-5-5"));
    assert!(models.iter().any(|m| m.model_id.as_str() == "claude-opus-4-6"));
    assert!(models.iter().any(|m| m.model_id.as_str() == "claude-haiku-4-5-20251001"));
    assert!(models.iter().all(|m| m.target_format == TargetFormat::Anthropic));
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
    assert_eq!(quota.weekly_reset_at.as_deref(), Some("2026-10-09T10:59:59Z"));
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
    assert_eq!(models[0].capabilities.as_ref().and_then(|c| c.reasoning), Some(true));

    assert_eq!(models[1].model_id.as_str(), "claude-haiku-4-5-20251001");
    assert_eq!(models[1].capabilities.as_ref().and_then(|c| c.reasoning), Some(false));
}
