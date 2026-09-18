use super::*;

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

    let quota = parse_minimax_quota(&json, "https://platform.minimax.io/v1/api/openplatform/coding_plan/remains")
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
    let find_oauth = |key: &str| headers_oauth.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str());

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
    let find_sk = |key: &str| headers_sk.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str());

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
    let cfg = adapter.config_mut().expect("config_mut must be implemented");
    cfg.extra_headers.push(("x-custom-mavis".into(), "val-123".into()));

    // Test in-memory dynamic extra header injection
    set_dynamic_extra_header("x-mavis-feature-flag", "speculative-decoding");

    let headers = adapter.build_headers("test-token", TargetFormat::Anthropic, &model);
    let find = |key: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str());

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
    let find = |key: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str());

    // Strict baseline contract for MiniMax Mavis upstream
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
    let models = adapter.fetch_models(&client, "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9...").await.unwrap();
    assert!(!models.is_empty());
    assert!(models.iter().any(|m| m.model_id.as_str() == "MiniMax-M3"));
    assert!(models.iter().any(|m| m.model_id.as_str() == "MiniMax-M2.7-highspeed"));

    // Empty key also returns catalog safely
    let models_empty = adapter.fetch_models(&client, "").await.unwrap();
    assert_eq!(models.len(), models_empty.len());
}

#[test]
fn test_minimax_builtin_models_structure() {
    let models = minimax_builtin_models();
    assert_eq!(models.len(), 5);
    let m3 = models.iter().find(|m| m.model_id.as_str() == "MiniMax-M3").expect("MiniMax-M3 present");
    assert_eq!(m3.context_length, Some(1_000_000));
    assert_eq!(m3.max_output_tokens, Some(128_000));
    assert_eq!(m3.target_format, TargetFormat::Anthropic);
}
