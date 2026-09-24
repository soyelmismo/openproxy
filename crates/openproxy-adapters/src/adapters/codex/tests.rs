use super::*;
use crate::spoofer::{
    CODEX_SPOOFING_HEADERS, CODEX_TEST_LOCK, ClientSpoofer, CodexSpoofer, current_codex_ua,
    current_codex_version, has_valid_codex_version, reset_dynamic_codex_overrides,
    set_dynamic_codex_extra_header,
};
use openproxy_types::{ModelId, TargetFormat};
use serde_json::json;

#[test]
fn test_parse_codex_usage_quota_valid() {
    let body = json!({
        "rate_limit": {
            "primary_window": {
                "used_percent": 42.5,
                "reset_at": 1_700_000_000.0
            },
            "secondary_window": {
                "usedPercent": 85.1,
                "resetAfterSeconds": 3600.0
            }
        }
    });

    let quota = quota::parse_codex_usage_quota(&body).expect("should parse");
    assert_eq!(quota.session_used, Some(43));
    assert_eq!(quota.session_reset_at, Some("1700000000".to_string()));
    assert_eq!(quota.weekly_used, Some(85));
    assert!(quota.weekly_reset_at.is_some());
}

#[test]
fn test_parse_codex_usage_quota_missing_rate_limit() {
    let body = json!({});
    let err = quota::parse_codex_usage_quota(&body).unwrap_err();
    assert!(err.to_string().contains("codex quota missing rate_limit"));
}

#[test]
fn test_apply_codex_spoofing_headers() {
    let mut req = UpstreamRequest::post_json("http://dummy.com", bytes::Bytes::new());
    apply_codex_spoofing_headers(&mut req);

    for &(k, v) in CODEX_SPOOFING_HEADERS {
        let header_value = req.headers.get(k).expect("header missing");
        if k == "user-agent" {
            assert_eq!(header_value, current_codex_ua().as_str());
        } else if k == "version" {
            assert_eq!(header_value, current_codex_version().as_str());
        } else {
            assert_eq!(header_value, http::HeaderValue::from_str(v).unwrap());
        }
    }
}

#[test]
fn test_wrap_request_body_enforces_stream_true() {
    let adapter = CodexAdapter::new();
    let json_body = serde_json::json!({
        "model": "gpt-5.6-luna",
        "input": [{"role": "user", "content": "hi"}],
        "stream": false,
        "temperature": 0.7,
        "top_p": 0.9,
        "max_tokens": 4096,
        "stop": ["\n"],
        "reasoning_effort": "high"
    });
    let body_bytes = bytes::Bytes::from(serde_json::to_vec(&json_body).unwrap());

    let resolved_target = openproxy_types::context::ResolvedTarget {
        target: openproxy_types::combos::ComboTarget {
            id: openproxy_types::ComboTargetId(1),
            combo_id: openproxy_types::ComboId(1),
            provider_id: openproxy_types::ProviderId::new("codex"),
            account_id: None,
            model_row_id: Some(openproxy_types::ModelRowId(1)),
            sub_combo_id: None,
            priority_order: 0,
            weight: 100,
            active: true,
            rate_limit_scope: openproxy_types::RateLimitScope::Account,
            cooldown_mode: None,
            cooldown_base_secs: None,
            cooldown_max_secs: None,
            cooldown_factor: None,
            thinking_effort: None,
            description: None,
        },
        model: openproxy_types::Model {
            row_id: openproxy_types::ModelRowId(1),
            provider_id: openproxy_types::ProviderId::new("codex"),
            model_id: openproxy_types::ModelId::new("gpt-5.6-luna"),
            target_format: openproxy_types::TargetFormat::Responses,
            discovered_at: openproxy_types::now_unix_secs_str().into_boxed_str(),
            context_length: Some(272_000),
            max_output_tokens: Some(32_768),
            model_type: "chat".into(),
            ..Default::default()
        },
        api_key: "tok".to_string(),
        api_key_label: None,
        custom_meta: None,
    };

    let wrapped = adapter
        .wrap_request_body(
            body_bytes,
            TargetFormat::Responses,
            &ModelId::new("gpt-5.6-luna"),
            &resolved_target,
        )
        .expect("wrap should succeed");

    let val: serde_json::Value = serde_json::from_slice(&wrapped).unwrap();
    assert_eq!(
        val.get("stream").and_then(serde_json::Value::as_bool),
        Some(true),
        "Codex must ALWAYS enforce stream: true on request payload"
    );
    assert!(
        val.get("temperature").is_none(),
        "Codex must strip temperature"
    );
    assert!(val.get("top_p").is_none(), "Codex must strip top_p");
    assert!(
        val.get("max_tokens").is_none(),
        "Codex must strip max_tokens"
    );
    assert!(val.get("stop").is_none(), "Codex must strip stop");
    assert_eq!(val["reasoning"]["effort"], "high");
}

#[test]
fn test_codex_build_headers_with_extra_and_dynamic_overrides() {
    let _guard = CODEX_TEST_LOCK.lock().unwrap();
    reset_dynamic_codex_overrides();

    let mut adapter = CodexAdapter::new();
    adapter.config_mut().unwrap().extra_headers = vec![
        ("x-codex-workspace".to_string(), "ws-123".to_string()),
        ("user-agent".to_string(), "CustomCodex/2.0".to_string()),
    ];

    set_dynamic_codex_extra_header("x-dynamic-header", "dynamic-val");

    let headers = adapter.build_headers(
        "my-key",
        TargetFormat::Responses,
        &ModelId::new("gpt-5.6-luna"),
    );

    let find = |k: &str| {
        headers
            .iter()
            .find(|(hk, _)| hk.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
    };

    assert_eq!(find("Authorization"), Some("Bearer my-key"));
    assert_eq!(find("Content-Type"), Some("application/json"));
    assert_eq!(find("x-codex-workspace"), Some("ws-123"));
    assert_eq!(find("user-agent"), Some("CustomCodex/2.0"));
    assert_eq!(find("x-dynamic-header"), Some("dynamic-val"));
    assert_eq!(find("origin"), Some("https://chatgpt.com"));
    assert_eq!(find("originator"), Some("codex_cli_rs"));

    reset_dynamic_codex_overrides();
}

#[test]
fn test_codex_default_headers_contract() {
    let _guard = CODEX_TEST_LOCK.lock().unwrap();
    reset_dynamic_codex_overrides();

    let adapter = CodexAdapter::new();
    let headers = adapter.build_headers(
        "codex-token-xyz",
        TargetFormat::Responses,
        &ModelId::new("gpt-5.6-luna"),
    );
    let find = |k: &str| {
        headers
            .iter()
            .find(|(hk, _)| hk.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
    };

    let actual_keys: std::collections::BTreeSet<String> = headers
        .iter()
        .map(|(k, _)| k.to_ascii_lowercase())
        .collect();
    let expected_keys: std::collections::BTreeSet<String> = [
        "authorization",
        "content-type",
        "origin",
        "originator",
        "version",
        "user-agent",
    ]
    .into_iter()
    .map(String::from)
    .collect();

    assert_eq!(
        actual_keys, expected_keys,
        "Codex contract breach: header added or removed"
    );

    assert_eq!(find("authorization"), Some("Bearer codex-token-xyz"));
    assert_eq!(find("content-type"), Some("application/json"));
    assert_eq!(find("origin"), Some("https://chatgpt.com"));
    assert_eq!(find("originator"), Some("codex_cli_rs"));
    assert_eq!(find("version"), Some(current_codex_version().as_str()));
    assert_eq!(find("user-agent"), Some(current_codex_ua().as_str()));
}

#[test]
fn test_codex_hardcoded_models_parity() {
    let models = codex_static_models();
    assert_eq!(models.len(), 10);
    let ids: Vec<&str> = models.iter().map(|m| m.model_id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "gpt-6-astra",
            "gpt-6-sol",
            "gpt-6-luna",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
            "gpt-daybreak-blue-latest",
            "gpt-daybreak-red-latest",
            "gpt-5.5",
            "gpt-5.4",
        ]
    );
    for m in &models {
        assert_eq!(m.target_format, TargetFormat::Responses);
        assert!(m.context_length.unwrap_or(0) >= 272_000);
        assert_eq!(m.max_output_tokens, Some(32_768));
        let caps = m.capabilities.as_ref().expect("capabilities");
        assert_eq!(caps.vision, Some(true));
        assert_eq!(caps.tool_calling, Some(true));
        assert_eq!(caps.reasoning, Some(true));
        assert_eq!(caps.thinking, Some(true));
    }
}

#[test]
fn test_has_valid_codex_version() {
    assert!(has_valid_codex_version(
        "codex-cli/0.156.1 (Windows 10.0.26200; x64)"
    ));
    assert!(has_valid_codex_version("codex-cli/0.158.0"));
    assert!(has_valid_codex_version("codex/1.0.0"));
    assert!(!has_valid_codex_version(
        "codex-cli/0.144.0 (Windows 10.0.26200; x64)"
    ));
    assert!(!has_valid_codex_version("curl/7.68.0"));
    assert!(!has_valid_codex_version(""));
}

#[test]
fn test_codex_spoofer_preserves_valid_and_upgrades_invalid_ua() {
    let _guard = CODEX_TEST_LOCK.lock().unwrap();
    reset_dynamic_codex_overrides();

    // 1. Upgrade outdated User-Agent
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::USER_AGENT,
        http::HeaderValue::from_static("codex-cli/0.144.0 (Windows 10.0.26200; x64)"),
    );
    headers.insert(
        http::header::HeaderName::from_static("version"),
        http::HeaderValue::from_static("0.144.0"),
    );

    CodexSpoofer.apply_to_header_map(&mut headers);

    assert_eq!(
        headers
            .get(http::header::USER_AGENT)
            .unwrap()
            .to_str()
            .unwrap(),
        "codex-cli/0.156.1 (Windows 10.0.26200; x64)"
    );
    assert_eq!(headers.get("version").unwrap().to_str().unwrap(), "0.156.1");
    assert_eq!(
        headers.get("origin").unwrap().to_str().unwrap(),
        "https://chatgpt.com"
    );
    assert_eq!(
        headers.get("originator").unwrap().to_str().unwrap(),
        "codex_cli_rs"
    );

    // 2. Preserve modern User-Agent >= 0.156.0
    let mut headers_modern = http::HeaderMap::new();
    headers_modern.insert(
        http::header::USER_AGENT,
        http::HeaderValue::from_static("codex-cli/0.158.0 (Linux x86_64)"),
    );
    headers_modern.insert(
        http::header::HeaderName::from_static("version"),
        http::HeaderValue::from_static("0.158.0"),
    );

    CodexSpoofer.apply_to_header_map(&mut headers_modern);

    assert_eq!(
        headers_modern
            .get(http::header::USER_AGENT)
            .unwrap()
            .to_str()
            .unwrap(),
        "codex-cli/0.158.0 (Linux x86_64)"
    );
    assert_eq!(
        headers_modern.get("version").unwrap().to_str().unwrap(),
        "0.158.0"
    );
}

#[test]
fn test_merge_codex_models_preserves_base_and_adds_backend() {
    let base = codex_static_models();
    assert_eq!(base.len(), 10);
    assert!(base.iter().any(|m| m.model_id.as_str() == "gpt-6-astra"));
    assert!(!base.iter().any(|m| m.model_id.as_str() == "gpt-reserve"));

    let backend = vec![
        openproxy_types::DiscoveredModel {
            model_id: ModelId::new("gpt-reserve"),
            display_name: Some("GPT-Reserve".into()),
            target_format: TargetFormat::Responses,
            context_length: Some(272_000),
            max_output_tokens: Some(32_768),
            input_modalities: None,
            output_modalities: None,
            model_type: Some("chat".into()),
            family: Some("gpt".into()),
            capabilities: None,
        },
        openproxy_types::DiscoveredModel {
            model_id: ModelId::new("gpt-5.6-luna"),
            display_name: Some("GPT-5.6 Luna Updated".into()),
            target_format: TargetFormat::Responses,
            context_length: Some(300_000),
            max_output_tokens: Some(32_768),
            input_modalities: None,
            output_modalities: None,
            model_type: Some("chat".into()),
            family: Some("gpt".into()),
            capabilities: None,
        },
    ];

    let merged = models::merge_codex_models(base, backend);
    assert_eq!(merged.len(), 11);
    assert!(merged.iter().any(|m| m.model_id.as_str() == "gpt-6-astra"));
    assert!(merged.iter().any(|m| m.model_id.as_str() == "gpt-5.4"));
    assert!(merged.iter().any(|m| m.model_id.as_str() == "gpt-reserve"));

    let updated_luna = merged
        .iter()
        .find(|m| m.model_id.as_str() == "gpt-5.6-luna")
        .unwrap();
    assert_eq!(
        updated_luna.display_name.as_deref(),
        Some("GPT-5.6 Luna Updated")
    );
    assert_eq!(updated_luna.context_length, Some(300_000));
}
