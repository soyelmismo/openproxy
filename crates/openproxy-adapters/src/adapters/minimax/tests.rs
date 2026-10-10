use super::*;
use crate::adapters::discovery::build_discovered_model_full;

#[test]
fn parses_minimax_quota_with_end_times() {
    let json = serde_json::json!({
        "model_remains": [
            {
                "start_time": 1787842800000_i64,
                "end_time": 1787860800000_i64,
                "remains_time": 11409757,
                "current_interval_total_count": 0,
                "current_interval_usage_count": 0,
                "model_name": "general",
                "current_weekly_total_count": 0,
                "current_weekly_usage_count": 0,
                "weekly_start_time": 1787529600000_i64,
                "weekly_end_time": 1788134400000_i64,
                "weekly_remains_time": 285009757,
                "current_interval_status": 2,
                "current_interval_remaining_percent": 0,
                "current_weekly_status": 1,
                "current_weekly_remaining_percent": 79
            }
        ],
        "base_resp": {
            "status_code": 0,
            "status_msg": "success"
        }
    });

    let quota = parse_minimax_quota(&json, "https://api.minimax.io/v1/token_plan/remains")
        .expect("quota parsed successfully");

    assert_eq!(quota.session_reset_at.as_deref(), Some("1787860800"));
    assert_eq!(quota.weekly_reset_at.as_deref(), Some("1788134400"));
    assert_eq!(quota.session_used, Some(100));
    assert_eq!(quota.session_limit, Some(100));
    assert_eq!(quota.weekly_used, Some(21));
    assert_eq!(quota.weekly_limit, Some(100));
}

#[test]
fn parses_minimax_quota_fallback_remains_time() {
    let json = serde_json::json!({
        "model_remains": [
            {
                "remains_time": 3600000,
                "model_name": "coding-plan",
                "current_interval_total_count": 50,
                "current_interval_usage_count": 10
            }
        ]
    });

    let quota = parse_minimax_quota(&json, "https://api.minimax.io/v1/token_plan/remains")
        .expect("quota parsed successfully");

    assert_eq!(quota.session_used, Some(10));
    assert_eq!(quota.session_limit, Some(50));
    assert!(quota.session_reset_at.is_some());
}

#[test]
fn parses_minimax_quota_code_2062_free_tier() {
    let json = serde_json::json!({
        "model_remains": null,
        "base_resp": {
            "status_code": 2062,
            "status_msg": "no active token plan subscription"
        }
    });

    let quota = parse_minimax_quota(
        &json,
        "https://platform.minimax.io/v1/api/openplatform/coding_plan/remains",
    )
    .expect("2062 should parse as Free tier");

    assert_eq!(quota.plan_name, Some("Free".to_string()));
    assert!(quota.fetch_error.is_none());
    assert_eq!(quota.session_used, None);
    assert_eq!(quota.weekly_used, None);
}

#[test]
fn parses_minimax_quota_base_resp_error() {
    let json = serde_json::json!({
        "model_remains": null,
        "base_resp": {
            "status_code": 2001,
            "status_msg": "invalid authorization token"
        }
    });

    let err =
        parse_minimax_quota(&json, "https://api.minimax.io/v1/token_plan/remains").unwrap_err();

    match err {
        CoreError::UpstreamConnection(msg) => {
            assert!(msg.contains("2001"));
            assert!(msg.contains("invalid authorization token"));
        }
        other => panic!("expected UpstreamConnection, got {other:?}"),
    }
}

#[test]
fn test_minimax_chat_url_managed_and_byok() {
    let mut adapter = MiniMaxAdapter::new();
    let model = ModelId::new("MiniMax-M3");

    // Default Managed (OAuth MiniMax Coding)
    assert_eq!(
        adapter.build_chat_url(TargetFormat::Anthropic, &model),
        "https://agent.minimax.io/mavis/api/v1/llm/v1/messages"
    );

    // China Managed endpoint
    adapter.config.base_url = "https://agent.minimax.cn".into();
    assert_eq!(
        adapter.build_chat_url(TargetFormat::Anthropic, &model),
        "https://agent.minimax.cn/mavis/api/v1/llm/v1/messages"
    );

    adapter.config.base_url = "https://agent.minimax.cn/mavis/api/v1/llm/v1".into();
    assert_eq!(
        adapter.build_chat_url(TargetFormat::Anthropic, &model),
        "https://agent.minimax.cn/mavis/api/v1/llm/v1/messages"
    );

    // BYOK endpoint
    adapter.config.base_url = "https://api.minimax.io".into();
    assert_eq!(
        adapter.build_chat_url(TargetFormat::Anthropic, &model),
        "https://api.minimax.io/anthropic/v1/messages?beta=true"
    );
}

#[test]
fn test_parse_minimax_meta() {
    let meta_str = r#"{"op_group_id":"grp_123","region":"cn","token_plan_tier":"Pro"}"#;
    let (group, region, tier) = parse_minimax_meta(Some(meta_str));
    assert_eq!(group.as_deref(), Some("grp_123"));
    assert_eq!(region.as_deref(), Some("cn"));
    assert_eq!(tier.as_deref(), Some("Pro"));
}

#[test]
fn test_minimax_build_headers() {
    let adapter = MiniMaxAdapter::new();
    let model = ModelId::new("MiniMax-M3");

    // 1. OAuth access token (non-sk)
    let headers_oauth = adapter.build_headers("test-token-123", TargetFormat::Anthropic, &model);
    let find_oauth = |key: &str| {
        headers_oauth
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    };

    assert_eq!(find_oauth("Authorization"), Some("Bearer test-token-123"));
    assert_eq!(find_oauth("x-api-key"), None);
    assert_eq!(find_oauth("Content-Type"), Some("application/json"));
    assert_eq!(find_oauth("User-Agent"), Some("MiniMaxAgent"));
    assert_eq!(find_oauth("Anthropic-Version"), Some("2023-06-01"));
    assert_eq!(find_oauth("X-Mavis-Agent-Id"), Some("main"));
    assert_eq!(find_oauth("X-Mavis-Timezone-Offset"), Some("0"));
    assert!(find_oauth("X-Mavis-Session-Id").is_some_and(|s| s.starts_with("session_")));

    // 2. BYOK API key (sk-...)
    let headers_sk = adapter.build_headers("sk-abc123456", TargetFormat::Anthropic, &model);
    let find_sk = |key: &str| {
        headers_sk
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    };

    assert_eq!(find_sk("x-api-key"), Some("sk-abc123456"));
    assert_eq!(find_sk("Authorization"), Some("Bearer sk-abc123456"));
}

#[test]
fn test_minimax_dynamic_headers_and_config_mut() {
    let _guard = MINIMAX_TEST_LOCK.lock().unwrap();
    let mut adapter = MiniMaxAdapter::new();
    let model = ModelId::new("MiniMax-M3");

    // Test dynamic runtime overrides
    set_dynamic_user_agent("MiniMaxCustomAgent/2.0");
    set_dynamic_anthropic_version("2024-10-22");

    assert_eq!(current_user_agent(), "MiniMaxCustomAgent/2.0");
    assert_eq!(current_anthropic_version(), "2024-10-22");

    // Test extra_headers via config_mut
    let cfg = adapter
        .config_mut()
        .expect("config_mut must be implemented");
    cfg.extra_headers
        .push(("x-custom-mavis".into(), "val-123".into()));

    // Test in-memory dynamic extra header injection
    set_dynamic_extra_header("x-mavis-feature-flag", "speculative-decoding");

    let headers = adapter.build_headers("test-token", TargetFormat::Anthropic, &model);
    let find = |key: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    };

    assert_eq!(find("User-Agent"), Some("MiniMaxCustomAgent/2.0"));
    assert_eq!(find("Anthropic-Version"), Some("2024-10-22"));
    assert_eq!(find("x-custom-mavis"), Some("val-123"));
    assert_eq!(find("x-mavis-feature-flag"), Some("speculative-decoding"));

    // Reset
    reset_dynamic_overrides();
    assert_eq!(current_user_agent(), "MiniMaxAgent");
    assert_eq!(current_anthropic_version(), "2023-06-01");
}

#[test]
fn test_minimax_default_headers_contract() {
    let _guard = MINIMAX_TEST_LOCK.lock().unwrap();
    reset_dynamic_overrides();
    let adapter = MiniMaxAdapter::new();
    let model = ModelId::new("MiniMax-M3");
    let headers = adapter.build_headers("oauth-token-123", TargetFormat::Anthropic, &model);
    let find = |key: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    };

    // Strict bijective baseline contract for MiniMax Mavis upstream: exactly the expected set of keys
    let actual_keys: std::collections::BTreeSet<String> = headers
        .iter()
        .map(|(k, _)| k.to_ascii_lowercase())
        .collect();
    let expected_keys: std::collections::BTreeSet<String> = [
        "authorization",
        "content-type",
        "user-agent",
        "anthropic-version",
        "x-mavis-agent-id",
        "x-mavis-timezone-offset",
        "x-mavis-session-id",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(
        actual_keys, expected_keys,
        "MiniMax contract breach: header added or removed"
    );

    assert_eq!(find("Authorization"), Some("Bearer oauth-token-123"));
    assert_eq!(find("Content-Type"), Some("application/json"));
    assert_eq!(find("User-Agent"), Some("MiniMaxAgent"));
    assert_eq!(find("Anthropic-Version"), Some("2023-06-01"));
    assert_eq!(find("X-Mavis-Agent-Id"), Some("main"));
    assert_eq!(find("X-Mavis-Timezone-Offset"), Some("0"));
    assert!(find("X-Mavis-Session-Id").is_some_and(|s| s.starts_with("session_")));
}

#[tokio::test]
async fn test_minimax_fetch_models_oauth_returns_builtin_catalog() {
    let adapter = MiniMaxAdapter::new();
    let client = Arc::new(UpstreamClient::new());

    // OAuth tokens (non-sk) must return builtin models immediately without HTTP calls
    let models = adapter
        .fetch_models(&client, "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9...")
        .await
        .unwrap();
    assert!(!models.is_empty());
    assert!(models.iter().any(|m| m.model_id.as_str() == "MiniMax-M3"));
    assert!(
        models
            .iter()
            .any(|m| m.model_id.as_str() == "MiniMax-M2.7-highspeed")
    );

    // Empty key also returns catalog safely
    let models_empty = adapter.fetch_models(&client, "").await.unwrap();
    assert_eq!(models.len(), models_empty.len());
}

#[test]
fn test_minimax_builtin_models_structure() {
    let models = minimax_builtin_models();
    assert_eq!(models.len(), 6);

    let m31 = models
        .iter()
        .find(|m| m.model_id.as_str() == "MiniMax-M3.1-Flash-Preview")
        .expect("MiniMax-M3.1-Flash-Preview present");
    assert_eq!(m31.context_length, Some(1_000_000));
    assert_eq!(m31.max_output_tokens, Some(128_000));
    assert_eq!(m31.target_format, TargetFormat::Anthropic);
    assert!(m31.capabilities.as_ref().unwrap().vision.unwrap());
    assert!(m31.capabilities.as_ref().unwrap().reasoning.unwrap());

    let m3 = models
        .iter()
        .find(|m| m.model_id.as_str() == "MiniMax-M3")
        .expect("MiniMax-M3 present");
    assert_eq!(m3.context_length, Some(1_000_000));
    assert_eq!(m3.max_output_tokens, Some(128_000));
    assert_eq!(m3.target_format, TargetFormat::Anthropic);
}

#[test]
fn test_minimax_wrap_body_translates_thinking_effort() {
    use openproxy_types::context::ResolvedTarget;
    use openproxy_types::models::Model;
    use openproxy_types::combos::ComboTarget;

    let adapter = MiniMaxAdapter::new();

    let make_target = |effort: Option<&str>| {
        let mut target = ComboTarget::default();
        target.thinking_effort = effort.map(|s| s.to_string().into());
        ResolvedTarget {
            target,
            model: Model::default(),
            api_key: "dummy".to_string(),
            api_key_label: None,
            custom_meta: None,
        }
    };

    // The generic Anthropic translator emits thinking.budget_tokens for an effort;
    // MiniMax must receive its native dialect instead (verified: budget_tokens is ignored upstream).
    let anthropic_body = serde_json::to_vec(&serde_json::json!({
        "model": "MiniMax-M3.1-Flash-Preview",
        "max_tokens": 4096,
        "messages": [{"role": "user", "content": "hi"}],
        "thinking": {"type": "enabled", "budget_tokens": 2048}
    }))
    .unwrap();

    let out = adapter
        .wrap_request_body(
            bytes::Bytes::from(anthropic_body),
            TargetFormat::Anthropic,
            &ModelId::new("MiniMax-M3.1-Flash-Preview"),
            &make_target(Some("low")),
        )
        .expect("wrap succeeds");
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();

    assert_eq!(v["thinking"]["type"], "adaptive", "thinking.type must be adaptive");
    assert_eq!(v["output_config"]["effort"], "low", "effort must map to output_config.effort");
    assert!(v["thinking"].get("budget_tokens").is_none(), "Anthropic budget_tokens must be stripped");

    // effort "none" => adaptive but no output_config
    let out_none = adapter
        .wrap_request_body(
            bytes::Bytes::from(serde_json::to_vec(&serde_json::json!({
                "thinking": {"type": "enabled", "budget_tokens": 2048}
            })).unwrap()),
            TargetFormat::Anthropic,
            &ModelId::new("MiniMax-M3.1-Flash-Preview"),
            &make_target(Some("none")),
        )
        .unwrap();
    let vn: serde_json::Value = serde_json::from_slice(&out_none).unwrap();
    assert_eq!(vn["thinking"]["type"], "adaptive");
    assert!(vn.get("output_config").is_none(), "none must not emit output_config");

    // No thinking key from translator => body untouched (MiniMax applies its default)
    let plain = serde_json::to_vec(&serde_json::json!({"messages": [{"role":"user","content":"hi"}]})).unwrap();
    let out_plain = adapter
        .wrap_request_body(
            bytes::Bytes::from(plain),
            TargetFormat::Anthropic,
            &ModelId::new("MiniMax-M3"),
            &make_target(Some("low")),
        )
        .unwrap();
    let vp: serde_json::Value = serde_json::from_slice(&out_plain).unwrap();
    assert!(vp.get("thinking").is_none(), "no translator thinking => no injection");

    // Non-Anthropic target format => passthrough
    let passthrough = adapter
        .wrap_request_body(
            bytes::Bytes::from(serde_json::to_vec(&serde_json::json!({"thinking": {"budget_tokens": 2048}})).unwrap()),
            TargetFormat::Openai,
            &ModelId::new("MiniMax-M3"),
            &make_target(Some("low")),
        )
        .unwrap();
    let vpt: serde_json::Value = serde_json::from_slice(&passthrough).unwrap();
    assert_eq!(vpt["thinking"]["budget_tokens"], 2048, "non-Anthropic format is passthrough");
}

#[test]
fn test_parse_minimax_config_ts_upstream_sample() {
    let ts_sample = r#"
import type { ModelConfig } from './types';

const MINIMAX_M3_FILE_API_CAPABILITIES = {
  fileApi: true,
};

const MINIMAX_MODELS: Record<string, ModelConfig> = {
  "MiniMax-M3": {
    name: "MiniMax-M3",
    attachment: true,
    reasoning: true,
    tool_call: true,
    temperature: true,
    modalities: { input: ["text", "image", "video"], output: ["text"] },
    limit: { context: 512000, output: 128000 },
    contextWindowOptions: [512000, 1000000],
    contextWindowOptionHints: { "1000000": "higher_usage" },
    options: { reasoningSummary: "auto" },
    thinking_config: { mode: "switchable", default_value: "true" },
    variants: {
      "none-thinking": { thinking: { type: "disabled" } },
      thinking: { thinking: { type: "adaptive" } },
    },
    capabilities: MINIMAX_M3_FILE_API_CAPABILITIES,
  },
  "MiniMax-M2.7-highspeed": {
    name: "MiniMax-M2.7-highspeed",
    attachment: false,
    reasoning: true,
    tool_call: true,
    temperature: true,
    modalities: { input: ["text"], output: ["text"] },
    limit: { context: 200000, output: 128000 },
  },
  "MiniMax-M2.7": {
    name: "MiniMax-M2.7",
    attachment: false,
    reasoning: true,
    tool_call: true,
    temperature: true,
    modalities: { input: ["text"], output: ["text"] },
    limit: { context: 200000, output: 128000 },
  },
  "MiniMax-M4": {
    name: "MiniMax-M4 Ultra",
    attachment: true,
    reasoning: true,
    tool_call: true,
    temperature: true,
    modalities: { input: ["text", "image"], output: ["text"] },
    limit: { context: 2000000, output: 256000 },
  },
};

export const MINIMAX_API_MODEL_CATALOG: Record<string, ModelConfig> = MINIMAX_MODELS;
"#;

    let parsed = parse_minimax_config_ts(ts_sample).expect("successfully parsed models");
    assert_eq!(parsed.len(), 4);

    let m3 = parsed
        .iter()
        .find(|m| m.model_id.as_str() == "MiniMax-M3")
        .unwrap();
    assert_eq!(m3.display_name.as_deref(), Some("MiniMax-M3"));
    assert_eq!(m3.context_length, Some(1_000_000)); // picked max from contextWindowOptions [512000, 1000000]
    assert_eq!(m3.max_output_tokens, Some(128_000));
    assert_eq!(m3.target_format, TargetFormat::Anthropic);
    assert!(m3.capabilities.as_ref().unwrap().vision.unwrap());
    assert!(m3.capabilities.as_ref().unwrap().tool_calling.unwrap());
    assert!(m3.capabilities.as_ref().unwrap().reasoning.unwrap());
    let in_mods = m3.input_modalities.as_ref().unwrap();
    assert!(in_mods.contains(&"image".to_string()));
    assert!(in_mods.contains(&"video".to_string()));

    let m27_hs = parsed
        .iter()
        .find(|m| m.model_id.as_str() == "MiniMax-M2.7-highspeed")
        .unwrap();
    assert_eq!(m27_hs.context_length, Some(200_000));
    assert_eq!(m27_hs.max_output_tokens, Some(128_000));
    assert!(!m27_hs.capabilities.as_ref().unwrap().vision.unwrap());

    let m4 = parsed
        .iter()
        .find(|m| m.model_id.as_str() == "MiniMax-M4")
        .unwrap();
    assert_eq!(m4.display_name.as_deref(), Some("MiniMax-M4 Ultra"));
    assert_eq!(m4.context_length, Some(2_000_000));
    assert_eq!(m4.max_output_tokens, Some(256_000));
    assert!(m4.capabilities.as_ref().unwrap().vision.unwrap());
}

#[test]
fn test_merge_minimax_models_preserves_base_and_appends_new() {
    let base = minimax_builtin_models();
    let upstream = vec![
        build_discovered_model_full(
            "MiniMax-M3".into(),
            Some("MiniMax-M3 (Updated)".into()),
            TargetFormat::Anthropic,
            Some(2_000_000),
            Some(256_000),
        ),
        build_discovered_model_full(
            "MiniMax-M4".into(),
            Some("MiniMax-M4".into()),
            TargetFormat::Anthropic,
            Some(3_000_000),
            Some(512_000),
        ),
    ];

    let merged = merge_minimax_models(base, upstream);
    // Base had 6 models; M3 was updated in-place; M4 was added => total 7
    assert_eq!(merged.len(), 7);

    let m3 = merged
        .iter()
        .find(|m| m.model_id.as_str() == "MiniMax-M3")
        .unwrap();
    assert_eq!(m3.display_name.as_deref(), Some("MiniMax-M3 (Updated)"));
    assert_eq!(m3.context_length, Some(2_000_000));

    let m4 = merged
        .iter()
        .find(|m| m.model_id.as_str() == "MiniMax-M4")
        .unwrap();
    assert_eq!(m4.context_length, Some(3_000_000));

    // Legacy models preserved
    assert!(merged.iter().any(|m| m.model_id.as_str() == "minimax-m2.1"));
    assert!(merged.iter().any(|m| m.model_id.as_str() == "MiniMax-M2"));
}

#[test]
fn test_dynamic_minimax_models_runtime_lifecycle() {
    reset_dynamic_minimax_models();
    assert!(current_dynamic_minimax_models().is_none());

    let test_catalog = vec![build_discovered_model_full(
        "MiniMax-M-Test".into(),
        Some("MiniMax-M-Test".into()),
        TargetFormat::Anthropic,
        Some(500_000),
        Some(64_000),
    )];

    set_dynamic_minimax_models(test_catalog);
    let cur = current_dynamic_minimax_models().expect("dynamic models must be present");
    assert_eq!(cur.len(), 1);
    assert_eq!(cur[0].model_id.as_str(), "MiniMax-M-Test");

    reset_dynamic_minimax_models();
    assert!(current_dynamic_minimax_models().is_none());
}
