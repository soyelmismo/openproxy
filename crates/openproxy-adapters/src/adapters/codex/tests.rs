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
fn test_parse_codex_usage_quota_with_additional_rate_limits() {
    let body = json!({
        "rate_limit": {
            "primary_window": {
                "used_percent": 10.0,
                "reset_at": 1_700_000_000.0
            }
        },
        "additional_rate_limits": [
            {
                "limit_name": "gpt-reserve",
                "metered_feature": "codex_reserve",
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 25.0,
                        "reset_at": 1_700_050_000.0
                    }
                }
            }
        ]
    });

    let quota = quota::parse_codex_usage_quota(&body).expect("should parse");
    assert_eq!(quota.session_used, Some(10));
    let details = quota.model_details.expect("model_details must be present");
    assert_eq!(details.len(), 1);
    assert_eq!(details[0].model_id, "gpt-reserve");
    assert_eq!(details[0].session_used, 25);
    assert_eq!(details[0].session_limit, 100);
    assert_eq!(details[0].session_reset_at, Some("1700050000".to_string()));
    assert!((details[0].remaining_fraction - 0.75).abs() < 1e-4);
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
        "max_completion_tokens": 4096,
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
    assert!(
        val.get("max_completion_tokens").is_none(),
        "Codex must strip max_completion_tokens"
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
        "x-codex-routing-hint",
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
    assert_eq!(find("x-codex-routing-hint"), Some("model=gpt-5.6-luna"));
}

#[test]
fn test_codex_hardcoded_models_parity() {
    let models = codex_static_models();
    assert_eq!(models.len(), 11);
    let ids: Vec<&str> = models.iter().map(|m| m.model_id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "gpt-6-astra",
            "gpt-6.1-sol",
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
        "codex-cli/0.159.0 (Windows 10.0.26200; x64)"
    );
    assert_eq!(headers.get("version").unwrap().to_str().unwrap(), "0.159.0");
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
    assert_eq!(base.len(), 11);
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
    assert_eq!(merged.len(), 12);
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

#[test]
fn test_codex_reset_credits_request_builders() {
    let req_list = quota::build_codex_reset_credits_request("test_token_abc", Some("ws_xyz"));
    assert_eq!(
        req_list.url,
        "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits"
    );
    assert_eq!(req_list.method, http::Method::GET);
    assert_eq!(
        req_list.headers.get("authorization").unwrap(),
        "Bearer test_token_abc"
    );
    assert_eq!(
        req_list.headers.get("chatgpt-account-id").unwrap(),
        "ws_xyz"
    );

    let req_consume = quota::build_codex_consume_reset_request(
        "test_token_abc",
        Some("ws_xyz"),
        "req-uuid-1",
        "credit-uuid-99",
    );
    assert_eq!(
        req_consume.url,
        "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume"
    );
    let payload: serde_json::Value =
        serde_json::from_slice(req_consume.body.as_ref().unwrap()).unwrap();
    assert_eq!(payload["redeem_request_id"], "req-uuid-1");
    assert_eq!(payload["credit_id"], "credit-uuid-99");
}

#[test]
fn test_parse_codex_reset_credits_list() {
    let raw = serde_json::json!({
        "credits": [
            {
                "id": "c1",
                "status": "available",
                "expires_at": "2099-01-01T00:00:00Z",
                "title": "Plus reset credit"
            },
            {
                "credit_id": "c2",
                "status": "consumed",
                "expires_at": "2099-01-01T00:00:00Z"
            },
            {
                "credit_id": "c0",
                "status": "available",
                "expires_at": "2090-01-01T00:00:00Z"
            }
        ],
        "available_count": 2
    });

    let (credits, count) = quota::parse_codex_reset_credits(&raw).unwrap();
    assert_eq!(count, 2);
    assert_eq!(credits.len(), 2);
    // Soonest expiring (2090) should be first
    assert_eq!(credits[0].id, "c0");
    assert_eq!(credits[1].id, "c1");
}

#[test]
fn test_parse_codex_consume_response() {
    let ok_reset = serde_json::json!({ "code": "reset" });
    assert_eq!(
        quota::parse_codex_consume_response(200, &ok_reset).unwrap(),
        quota::CodexResetOutcome::Reset
    );

    let ok_already = serde_json::json!({ "code": "alreadyRedeemed" });
    assert_eq!(
        quota::parse_codex_consume_response(200, &ok_already).unwrap(),
        quota::CodexResetOutcome::AlreadyRedeemed
    );

    let err_nothing = serde_json::json!({ "error": { "code": "nothing_to_reset" } });
    assert_eq!(
        quota::parse_codex_consume_response(409, &err_nothing).unwrap(),
        quota::CodexResetOutcome::NothingToReset
    );

    let err_nocredit = serde_json::json!({ "error": { "code": "no_credit" } });
    assert_eq!(
        quota::parse_codex_consume_response(409, &err_nocredit).unwrap(),
        quota::CodexResetOutcome::NoCredit
    );
}

#[test]
fn test_parse_codex_reset_credits_count() {
    let usage = serde_json::json!({
        "rate_limit_reset_credits": {
            "available_count": 3
        }
    });
    assert_eq!(quota::parse_codex_reset_credits_count(&usage), Some(3));
}

#[test]
fn test_patch_codex_request_object_sanitizes_input_image_parameters() {
    let mut obj = serde_json::json!({
        "model": "gpt-6.1-sol",
        "input": [
            {
                "role": "user",
                "content": [
                    {
                        "type": "input_image",
                        "image": "aGVsbG8=",
                        "mime_type": "image/png"
                    },
                    {
                        "type": "input_text",
                        "text": "what is this?"
                    }
                ]
            }
        ]
    })
    .as_object()
    .unwrap()
    .clone();

    patch_codex_request_object(&mut obj);

    let input = obj["input"].as_array().unwrap();
    let content = input[0]["content"].as_array().unwrap();
    let img_part = &content[0];

    assert_eq!(img_part["type"], "input_image");
    assert_eq!(img_part["image_url"], "data:image/png;base64,aGVsbG8=");
    assert!(
        img_part.get("image").is_none(),
        "image parameter must be stripped for Codex"
    );
    assert!(
        img_part.get("mime_type").is_none(),
        "mime_type parameter must be stripped for Codex"
    );
}

/// Un snapshot congelado del contrato fija headers y body de la MISMA
/// generación: la versión y el user-agent salen del snapshot, y los defaults
/// verificados por el pin (`store: false`, `tool_choice: "auto"`) se aplican
/// sin pisar una decisión explícita del cliente.
#[test]
fn test_pinned_contract_drives_headers_and_body_of_one_generation() {
    use crate::client_contract::providers::codex_runtime::{
        CODEX_RUNTIME_EXTRACTOR_VERSION, CODEX_RUNTIME_RULE_SET_ID, CodexProfile,
        CodexRequestDefaults,
    };
    use crate::client_contract::{
        Candidate, ClientIdentity, ContractId, IDENTITY_ORIGIN_OFFICIAL, Snapshot,
    };

    let profile = CodexProfile {
        version: "0.163.0".to_string(),
        commit: "0000000000000000000000000000000000000000".to_string(),
        release_tag: "rust-v0.163.0".to_string(),
        originator: "codex_cli_rs".to_string(),
        base_url: "https://chatgpt.com/backend-api/codex".to_string(),
        residency_header: "x-openai-internal-codex-residency".to_string(),
        routing_hint_header: "x-codex-routing-hint".to_string(),
        models_endpoint_path: "models".to_string(),
        client_version_query: "client_version".to_string(),
        defaults: CodexRequestDefaults {
            tool_choice: "auto".to_string(),
            store: false,
            stream: true,
        },
        rule_set_id: CODEX_RUNTIME_RULE_SET_ID.to_string(),
        extractor_version: CODEX_RUNTIME_EXTRACTOR_VERSION,
        catalog_models: 11,
    };
    let identity = ClientIdentity::versioned("0.163.0", IDENTITY_ORIGIN_OFFICIAL);
    let candidate = Candidate::without_catalog(
        ContractId::Codex,
        identity,
        CODEX_RUNTIME_RULE_SET_ID,
        CODEX_RUNTIME_EXTRACTOR_VERSION,
        Vec::new(),
        profile,
    );
    let snapshot = Snapshot::from_candidate(candidate).expect("snapshot de prueba");

    let adapter = CodexAdapter::new().with_pinned_contract(Arc::new(snapshot));
    assert!(adapter.pinned_contract().is_some());

    let headers = adapter.build_headers(
        "tok",
        TargetFormat::Responses,
        &ModelId::new("gpt-5.6-luna"),
    );
    let find = |k: &str| {
        headers
            .iter()
            .find(|(hk, _)| hk.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(
        find("version"),
        Some("0.163.0"),
        "el header version sale del snapshot congelado"
    );
    assert_eq!(
        find("user-agent"),
        Some("codex-cli/0.163.0 (Windows 10.0.26200; x64)")
    );

    let body = bytes::Bytes::from(
        serde_json::to_vec(&serde_json::json!({"model": "gpt-5.6-luna", "input": []})).unwrap(),
    );
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
            body,
            TargetFormat::Responses,
            &ModelId::new("gpt-5.6-luna"),
            &resolved_target,
        )
        .expect("wrap should succeed");
    let val: serde_json::Value = serde_json::from_slice(&wrapped).unwrap();
    assert_eq!(val["stream"], true);
    assert_eq!(
        val["store"], false,
        "el default verificado por el pin viaja en el body"
    );
    assert_eq!(val["tool_choice"], "auto");
}

/// Sin snapshot congelado el comportamiento es el histórico: la versión la
/// anuncia el spoofer y el body no lleva los defaults del contrato.
#[test]
fn test_without_a_pinned_contract_the_body_keeps_its_shape() {
    let _guard = CODEX_TEST_LOCK.lock().unwrap();
    reset_dynamic_codex_overrides();

    let adapter = CodexAdapter::new();
    assert!(adapter.pinned_contract().is_none());
    let body = bytes::Bytes::from(
        serde_json::to_vec(&serde_json::json!({"model": "gpt-5.6-luna", "input": []})).unwrap(),
    );
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
            body,
            TargetFormat::Responses,
            &ModelId::new("gpt-5.6-luna"),
            &resolved_target,
        )
        .expect("wrap should succeed");
    let val: serde_json::Value = serde_json::from_slice(&wrapped).unwrap();
    assert_eq!(val["stream"], true);
    assert!(
        val.get("store").is_none() && val.get("tool_choice").is_none(),
        "sin contrato congelado no se inyectan defaults"
    );
}

/// El catálogo del snapshot se filtra por lo que el pin publica de verdad:
/// modelos ocultos, no soportados por API o que exigen un cliente más nuevo
/// no se listan.
#[test]
fn test_snapshot_catalog_filters_by_visibility_support_and_minimal_client() {
    let catalog = json!({"models": [
        {"slug": "m-list", "display_name": "L", "priority": 1, "visibility": "list", "supported_in_api": true, "minimal_client_version": "0.150.0"},
        {"slug": "m-hidden", "display_name": "H", "priority": 2, "visibility": "hide", "supported_in_api": true},
        {"slug": "m-old-client", "display_name": "O", "priority": 3, "visibility": "list", "supported_in_api": true, "minimal_client_version": "0.170.0"},
        {"slug": "m-no-api", "display_name": "N", "priority": 4, "visibility": "list", "supported_in_api": false},
        {"slug": "codex-auto-review", "display_name": "R", "priority": 5, "visibility": "list", "supported_in_api": true}
    ]});
    let bytes = serde_json::to_vec(&catalog).unwrap();

    let visible = models::snapshot_catalog_models(&bytes, "0.162.1").unwrap();
    let ids: Vec<&str> = visible.iter().map(|m| m.model_id.as_str()).collect();
    assert_eq!(ids, vec!["m-list"]);

    // El camino sin contrato mantiene su comportamiento histórico: solo
    // excluye el modelo interno de review.
    let unfiltered = models::parse_codex_models_json(&bytes).unwrap();
    assert_eq!(unfiltered.len(), 4);
}

/// El catálogo real del pin, filtrado por la versión del snapshot.
#[test]
fn test_snapshot_catalog_of_the_pinned_fixture_filters_hidden_models() {
    let catalog = include_str!(
        "../../../../../fixtures/codex-rust-v0.162.1/codex-rs/models-manager/models.json"
    );
    let all = models::parse_codex_models_json(catalog.as_bytes()).unwrap();
    let visible = models::snapshot_catalog_models(catalog.as_bytes(), "0.162.1").unwrap();
    assert_eq!(all.len(), 10, "11 modelos menos el interno de review");
    assert!(
        visible.len() < all.len(),
        "el filtrado real debe descartar modelos ocultos o no publicados"
    );
    assert!(
        !visible
            .iter()
            .any(|m| m.model_id.as_str() == "codex-auto-review")
    );
}
