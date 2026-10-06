use super::*;
use serde_json::json;

mod adversarial_tests;

#[test]
fn test_map_antigravity_physical_model() {
    assert_eq!(
        map_antigravity_physical_model("gemini-3.1-pro-high"),
        "gemini-pro-agent"
    );
    assert_eq!(
        map_antigravity_physical_model("gemini-3.1-pro-medium"),
        "gemini-pro-agent"
    );
    assert_eq!(
        map_antigravity_physical_model("gemini-3.5-flash-high"),
        "gemini-3-flash-agent"
    );
    assert_eq!(
        map_antigravity_physical_model("gemini-1.5-pro"),
        "gemini-1.5-pro"
    );
    assert_eq!(
        map_antigravity_physical_model("custom-model"),
        "custom-model"
    );
}

#[test]
fn test_parse_models_response_valid() {
    let body = json!({
        "models": {
            "gemini-1.5-pro": {
                "displayName": "Gemini 1.5 Pro",
                "maxTokens": 1000000,
                "maxOutputTokens": 8192,
                "supportsThinking": true,
                "supportsImages": true,
                "supportsCodeGeneration": true
            },
            "gemini-1.5-flash": {
                "displayName": "Gemini 1.5 Flash",
                "contextLength": 200000,
                "supportsThinking": false
            }
        }
    });

    let models = AntigravityAdapter::parse_models_response(&body).expect("should parse");
    assert_eq!(models.len(), 2);

    let pro_model = models
        .iter()
        .find(|m| m.model_id.as_str() == "gemini-1.5-pro")
        .unwrap();
    assert_eq!(pro_model.display_name.as_deref(), Some("Gemini 1.5 Pro"));
    assert_eq!(pro_model.context_length, Some(1000000));
    assert_eq!(pro_model.max_output_tokens, Some(8192));
    let caps = pro_model.capabilities.as_ref().unwrap();
    assert_eq!(caps.thinking, Some(true));
    assert_eq!(caps.vision, Some(true));

    let flash_model = models
        .iter()
        .find(|m| m.model_id.as_str() == "gemini-1.5-flash")
        .unwrap();
    assert_eq!(
        flash_model.display_name.as_deref(),
        Some("Gemini 1.5 Flash")
    );
    assert_eq!(flash_model.context_length, Some(200000));
    assert_eq!(flash_model.max_output_tokens, Some(8192));
    let flash_caps = flash_model.capabilities.as_ref().unwrap();
    assert_eq!(flash_caps.thinking, Some(false));
}

#[test]
fn test_parse_models_response_invalid() {
    let body = json!({});
    assert!(AntigravityAdapter::parse_models_response(&body).is_none());
}

#[test]
fn test_parse_antigravity_models_response_missing_models() {
    let body = json!({});
    let result = parse_antigravity_models_response(&body);
    assert!(result.is_err());
}

#[test]
fn test_parse_antigravity_models_response_valid() {
    let body = json!({
        "models": {
            "model-a": {
                "quotaInfo": {
                    "resetTime": "2023-01-01T00:00:00Z",
                    "remainingFraction": 0.5
                }
            },
            "model-b": {
                "quotaInfo": {
                    "resetTime": "2023-01-01T00:00:00Z",
                    "remainingFraction": 0.2
                }
            }
        }
    });

    let result = parse_antigravity_models_response(&body).expect("should parse");
    assert_eq!(result.plan_name.unwrap(), "Antigravity");
    assert_eq!(result.session_limit.unwrap(), 1000);
    assert_eq!(result.session_used.unwrap(), 800);

    let details = result.model_details.unwrap();
    assert_eq!(details.len(), 2);
}

#[test]
fn test_inject_sentinel_thought_signatures_flash() {
    let mut contents = json!([
        {
            "role": "model",
            "parts": [
                {
                    "functionCall": {
                        "name": "get_weather",
                        "args": {}
                    }
                }
            ]
        }
    ]);

    tokens::inject_sentinel_thought_signatures(&mut contents, "gemini-3.7-flash-high");
    let parts = contents[0]["parts"].as_array().expect("parts array");
    assert_eq!(
        parts.len(),
        2,
        "must prepend placeholder thought block before functionCall"
    );

    // Part 0: prepended placeholder thought
    assert_eq!(parts[0]["thought"], true);
    assert_eq!(parts[0]["text"], "...");
    assert_eq!(
        parts[0]["thoughtSignature"],
        "skip_thought_signature_validator"
    );

    // Part 1: functionCall with sentinel signature and cleaned snake_case
    assert_eq!(
        parts[1]["thoughtSignature"],
        "skip_thought_signature_validator"
    );
    assert!(
        parts[1].get("thought_signature").is_none(),
        "snake_case thought_signature must be purged for Google Cloud Code API"
    );
}

#[test]
fn test_parse_antigravity_user_quota_summary_missing_groups() {
    let body = json!({});
    let result = parse_antigravity_user_quota_summary(&body);
    assert!(result.is_err());
}

#[test]
fn test_parse_antigravity_user_quota_summary_valid() {
    let body = json!({
        "groups": [{
            "displayName": "Pro",
            "buckets": [{
                "window": "WEEK",
                "resetTime": "2023-01-01T00:00:00Z",
                "remainingFraction": 0.5
            }, {
                "window": "DAY",
                "resetTime": "2023-01-01T00:00:00Z",
                "remainingFraction": 0.8
            }]
        }]
    });

    let result = parse_antigravity_user_quota_summary(&body).expect("should parse");
    assert_eq!(result.plan_name.unwrap(), "Pro");
    assert_eq!(result.weekly_limit.unwrap(), 1000);
    assert_eq!(result.weekly_used.unwrap(), 500);
    assert_eq!(result.session_limit.unwrap(), 1000);
    assert_eq!(result.session_used.unwrap(), 200);
}

#[test]
fn test_parse_antigravity_user_quota_summary_gemini_and_claude() {
    let body = json!({
        "groups": [
            {
                "displayName": "Gemini Models",
                "buckets": [
                    {
                        "bucketId": "gemini-weekly",
                        "window": "weekly",
                        "resetTime": "2026-10-07T03:24:40Z",
                        "remainingFraction": 1.0
                    },
                    {
                        "bucketId": "gemini-5h",
                        "window": "5h",
                        "resetTime": "2026-09-30T08:24:40Z",
                        "remainingFraction": 1.0
                    }
                ]
            },
            {
                "displayName": "Claude and GPT models",
                "buckets": [
                    {
                        "bucketId": "3p-weekly",
                        "window": "weekly",
                        "resetTime": "2026-10-06T23:16:00Z",
                        "remainingFraction": 0.95
                    },
                    {
                        "bucketId": "3p-5h",
                        "window": "5h",
                        "resetTime": "2026-09-30T06:42:34Z",
                        "remainingFraction": 0.8
                    }
                ]
            }
        ]
    });

    let result = parse_antigravity_user_quota_summary(&body).expect("should parse");
    assert_eq!(
        result.weekly_reset_at.as_deref(),
        Some("2026-10-07T03:24:40Z")
    );
    assert_eq!(
        result.session_reset_at.as_deref(),
        Some("2026-09-30T08:24:40Z")
    );
    assert_eq!(result.weekly_used, Some(0));
    assert_eq!(result.session_used, Some(0));

    let details = result.model_details.expect("should have claude details");
    assert_eq!(details.len(), 2);
    let claude_weekly = details
        .iter()
        .find(|d| d.model_id == "Claude (Weekly)")
        .unwrap();
    assert_eq!(
        claude_weekly.session_reset_at.as_deref(),
        Some("2026-10-06T23:16:00Z")
    );
    assert_eq!(claude_weekly.session_used, 50); // 1.0 - 0.95 = 5% of 1000

    let claude_5h = details
        .iter()
        .find(|d| d.model_id == "Claude (5h)")
        .unwrap();
    assert_eq!(
        claude_5h.session_reset_at.as_deref(),
        Some("2026-09-30T06:42:34Z")
    );
    assert_eq!(claude_5h.session_used, 200); // 1.0 - 0.8 = 20% of 1000
}

#[test]
fn normalize_quota_fraction_unlimited() {
    assert_eq!(normalize_quota_fraction(None, Some(1.0)), (0, true));
}

#[test]
fn normalize_quota_fraction_with_reset_no_fraction() {
    assert_eq!(
        normalize_quota_fraction(Some("2023-01-01T00:00:00Z"), None),
        (1000, false)
    );
}

#[test]
fn normalize_quota_fraction_no_reset_with_fraction() {
    assert_eq!(normalize_quota_fraction(None, Some(0.5)), (500, false));
}

#[test]
fn normalize_quota_fraction_with_reset_with_fraction() {
    assert_eq!(
        normalize_quota_fraction(Some("2023-01-01T00:00:00Z"), Some(0.3)),
        (700, false)
    );
}

#[test]
fn count_tokens_wraps_request_only_no_project() {
    let inner = serde_json::json!({
        "contents": [{"role": "user", "parts": [{"text": "hi"}]}]
    });
    let wrapped = serde_json::json!({ "request": inner });

    assert_eq!(wrapped.get("project"), None);
    assert_eq!(wrapped.get("model"), None);
    assert_eq!(wrapped.get("requestType"), None);
    assert_eq!(wrapped.get("enabledCreditTypes"), None);
    assert_eq!(
        wrapped["request"]["contents"].as_array().map(Vec::len),
        Some(1)
    );
}

#[test]
fn parse_total_tokens_flat() {
    let body = json!({"totalTokens": 42});
    assert_eq!(parse_total_tokens(&body), Some(42));
}

#[test]
fn parse_total_tokens_nested() {
    let body = json!({"response": {"totalTokens": 7}});
    assert_eq!(parse_total_tokens(&body), Some(7));
}

#[test]
fn parse_total_tokens_missing_returns_none() {
    assert_eq!(parse_total_tokens(&json!({})), None);
    assert_eq!(parse_total_tokens(&json!({"response": {}})), None);
    assert_eq!(parse_total_tokens(&json!({"totalTokens": "42"})), None);
}

#[cfg(feature = "upstream-hyper")]
#[tokio::test]
async fn count_tokens_propagates_4xx() {
    use crate::upstream::tests_helper as mock_helper;
    use std::sync::Arc;

    let upstream: Arc<crate::upstream::UpstreamClient> =
        mock_helper::build_mock_upstream_returning_status(401, "auth required").await;
    let res = count_tokens(
        &upstream,
        "fake-token",
        &serde_json::json!({"contents": []}),
    )
    .await;
    let err = res.expect_err("must error on 4xx");
    assert!(
        err.contains("401"),
        "error msg should mention status: {err}"
    );
    assert!(
        err.contains("auth required"),
        "body should be in msg: {err}"
    );
}

#[test]
fn test_antigravity_adapter_build_headers_with_extra_and_dynamic_overrides() {
    use crate::adapters::ProviderAdapter;
    use crate::antigravity_headers::{
        ANTIGRAVITY_TEST_LOCK, reset_dynamic_overrides, set_dynamic_extra_header,
        set_dynamic_version,
    };

    let _guard = ANTIGRAVITY_TEST_LOCK.lock().unwrap();
    reset_dynamic_overrides();
    let mut a = AntigravityAdapter::new();
    let cfg = a.config_mut().expect("config_mut");
    cfg.extra_headers
        .push(("x-admin-injected".into(), "true".into()));
    cfg.extra_headers
        .push(("x-goog-user-project".into(), "override-proj".into()));

    set_dynamic_version("4.9.0");
    set_dynamic_extra_header("x-antigravity-custom-edge", "edge-val");

    let headers = a.build_headers(
        "ya29.test",
        TargetFormat::Gemini,
        &ModelId::new("gemini-1.5-pro"),
    );
    let get = |k: &str| {
        headers
            .iter()
            .find(|(hk, _)| hk.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
    };

    assert_eq!(get("authorization"), Some("Bearer ya29.test"));
    assert_eq!(get("x-admin-injected"), Some("true"));
    assert_eq!(get("x-goog-user-project"), Some("override-proj"));
    assert_eq!(get("x-client-version"), Some("4.9.0"));
    assert_eq!(get("x-antigravity-custom-edge"), Some("edge-val"));

    reset_dynamic_overrides();
}

#[test]
fn test_antigravity_default_headers_contract() {
    use crate::adapters::ProviderAdapter;
    use crate::antigravity_headers::{ANTIGRAVITY_TEST_LOCK, reset_dynamic_overrides};

    let _guard = ANTIGRAVITY_TEST_LOCK.lock().unwrap();
    reset_dynamic_overrides();
    let a = AntigravityAdapter::new();
    let headers = a.build_headers(
        "token-xyz",
        TargetFormat::Gemini,
        &ModelId::new("gemini-1.5-pro"),
    );

    let get = |k: &str| {
        headers
            .iter()
            .find(|(hk, _)| hk.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
    };

    // Strict bijective baseline contract: exactly the expected set of keys, no more, no less
    let actual_keys: std::collections::BTreeSet<String> = headers
        .iter()
        .map(|(k, _)| k.to_ascii_lowercase())
        .collect();
    let expected_keys: std::collections::BTreeSet<String> = [
        "authorization",
        "content-type",
        "user-agent",
        "x-client-name",
        "x-client-version",
        "x-machine-id",
        "x-vscode-sessionid",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(
        actual_keys, expected_keys,
        "Antigravity contract breach: header added or removed"
    );

    assert_eq!(get("authorization"), Some("Bearer token-xyz"));
    assert_eq!(get("content-type"), Some("application/json"));
    assert_eq!(get("x-client-name"), Some("antigravity"));
    assert!(get("x-client-version").is_some_and(|v| !v.is_empty()));
    assert!(get("user-agent").is_some_and(|ua| ua.starts_with("Antigravity/")));
    assert!(
        get("x-machine-id")
            .is_some_and(|id| id.len() == 32 && id.chars().all(|c| c.is_ascii_hexdigit()))
    );
    assert!(get("x-vscode-sessionid").is_some_and(|s| !s.is_empty()));
}

#[test]
fn test_claude_opus_4_6_thinking_no_sentinel_thought_signatures() {
    let mut contents = json!([
        {
            "role": "model",
            "parts": [
                {
                    "thought": true,
                    "text": "reasoning...",
                    "thought_signature": "fake-sig"
                },
                {
                    "functionCall": {
                        "name": "get_file",
                        "args": {}
                    }
                }
            ]
        }
    ]);

    tokens::inject_sentinel_thought_signatures(&mut contents, "claude-opus-4-6-thinking");
    let parts = contents[0]["parts"].as_array().expect("parts array");

    // Must NOT have thoughtSignature injected for Claude
    for part in parts {
        assert!(
            part.get("thoughtSignature").is_none(),
            "Claude must never receive Gemini thoughtSignature"
        );
        assert!(
            part.get("thought_signature").is_none(),
            "snake_case thought_signature must be purged"
        );
    }
}

#[test]
fn test_wrap_request_body_claude_sanitizes_role_function_and_correlates_ids() {
    let adapter = AntigravityAdapter::new();
    let body_json = json!({
        "contents": [
            {
                "role": "user",
                "parts": [{"text": "call"}]
            },
            {
                "role": "model",
                "parts": [
                    {
                        "functionCall": {
                            "name": "calc",
                            "args": {}
                        }
                    }
                ]
            },
            {
                "role": "function",
                "parts": [
                    {
                        "functionResponse": {
                            "name": "calc",
                            "response": {"result": 42}
                        }
                    }
                ]
            }
        ],
        "generationConfig": {
            "maxOutputTokens": 2048,
            "thinkingConfig": {
                "thinkingBudget": 500
            }
        }
    });

    let raw_bytes = bytes::Bytes::from(serde_json::to_vec(&body_json).unwrap());
    let target = openproxy_types::context::ResolvedTarget {
        target: openproxy_types::combos::ComboTarget {
            id: openproxy_types::ComboTargetId(1),
            combo_id: openproxy_types::ComboId(1),
            provider_id: openproxy_types::ProviderId::new("antigravity"),
            account_id: None,
            model_row_id: None,
            sub_combo_id: None,
            priority_order: 1,
            weight: 1,
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
            provider_id: openproxy_types::ProviderId::new("antigravity"),
            model_id: openproxy_types::ModelId::new("claude-opus-4-6-thinking"),
            target_format: openproxy_types::TargetFormat::Gemini,
            discovered_at: openproxy_types::now_unix_secs_str().into_boxed_str(),
            ..Default::default()
        },
        api_key: "k".to_string(),
        api_key_label: None,
        custom_meta: None,
    };

    let wrapped = adapter
        .wrap_request_body(
            raw_bytes,
            TargetFormat::Gemini,
            &ModelId::new("claude-opus-4-6-thinking"),
            &target,
        )
        .expect("wrap_request_body should succeed");

    let val: serde_json::Value = serde_json::from_slice(&wrapped).unwrap();
    let req = &val["request"];
    let contents = req["contents"].as_array().unwrap();

    // 1. Role "function" must be converted to "user"
    assert_eq!(contents[2]["role"], "user");

    // 2. Both functionCall and functionResponse must have matching IDs
    let fc_id = contents[1]["parts"][0]["functionCall"]["id"]
        .as_str()
        .unwrap();
    let fr_id = contents[2]["parts"][0]["functionResponse"]["id"]
        .as_str()
        .unwrap();
    assert!(!fc_id.is_empty());
    assert_eq!(fc_id, fr_id);

    // 3. thinkingBudget clamped to at least 1024, and maxOutputTokens has headroom (> 1024)
    let gen_cfg = &req["generationConfig"];
    assert_eq!(
        gen_cfg["thinkingConfig"]["thinkingBudget"].as_i64(),
        Some(1024)
    );
    assert!(gen_cfg["maxOutputTokens"].as_i64().unwrap() > 1024);

    // 4. functionResponse.response strictly normalized to { "output": 42 }
    let fr_resp = &contents[2]["parts"][0]["functionResponse"]["response"];
    assert_eq!(fr_resp["output"], 42);

    // 5. Envelope has no enabledCreditTypes
    assert_eq!(val.get("enabledCreditTypes"), None);
}

#[test]
fn test_wrap_request_body_gemini_sanitizes_role_function_to_model_and_formats_output() {
    let adapter = AntigravityAdapter::new();
    let body_json = json!({
        "contents": [
            {
                "role": "user",
                "parts": [{"text": "run"}]
            },
            {
                "role": "model",
                "parts": [
                    {
                        "functionCall": {
                            "name": "shell",
                            "args": {"cmd": "echo hi"}
                        }
                    }
                ]
            },
            {
                "role": "function",
                "parts": [
                    {
                        "functionResponse": {
                            "name": "shell",
                            "response": {
                                "name": "shell",
                                "content": "hello world"
                            }
                        }
                    }
                ]
            }
        ],
        "tools": [
            {
                "functionDeclarations": [
                    {
                        "name": "shell",
                        "description": "run shell",
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "cmd": {"type": "string"}
                            }
                        }
                    },
                    {
                        "name": "read_file",
                        "description": "read a file",
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "path": {"type": "string"}
                            }
                        }
                    }
                ]
            }
        ]
    });

    let raw_bytes = bytes::Bytes::from(serde_json::to_vec(&body_json).unwrap());
    let target = openproxy_types::context::ResolvedTarget {
        target: openproxy_types::combos::ComboTarget {
            id: openproxy_types::ComboTargetId(1),
            combo_id: openproxy_types::ComboId(1),
            provider_id: openproxy_types::ProviderId::new("antigravity"),
            account_id: None,
            model_row_id: None,
            sub_combo_id: None,
            priority_order: 1,
            weight: 1,
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
            provider_id: openproxy_types::ProviderId::new("antigravity"),
            model_id: openproxy_types::ModelId::new("gemini-3.8-flash-high"),
            target_format: openproxy_types::TargetFormat::Gemini,
            discovered_at: openproxy_types::now_unix_secs_str().into_boxed_str(),
            ..Default::default()
        },
        api_key: "k".to_string(),
        api_key_label: None,
        custom_meta: None,
    };

    let wrapped = adapter
        .wrap_request_body(
            raw_bytes,
            TargetFormat::Gemini,
            &ModelId::new("gemini-3.8-flash-high"),
            &target,
        )
        .expect("wrap_request_body should succeed");

    let val: serde_json::Value = serde_json::from_slice(&wrapped).unwrap();
    let req = &val["request"];
    let contents = req["contents"].as_array().unwrap();

    // 1. Role "function" must be converted to "model" for Gemini
    assert_eq!(contents[2]["role"], "model");

    // 2. Both functionCall and functionResponse must have matching call_... IDs
    let fc_part = contents[1]["parts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p.get("functionCall").is_some())
        .expect("functionCall part must exist");
    let fc_id = fc_part["functionCall"]["id"].as_str().unwrap();
    let fr = &contents[2]["parts"][0]["functionResponse"];
    let fr_id = fr["id"].as_str().unwrap();
    assert!(fc_id.starts_with("call_"));
    assert_eq!(fc_id, fr_id);

    // 3. functionResponse.response strictly formatted to { "output": "hello world" }
    assert_eq!(fr["response"]["output"], "hello world");

    // 4. Tools expanded to individual declarations with uppercase types
    let tools = req["tools"].as_array().unwrap();
    assert_eq!(
        tools.len(),
        2,
        "tools must be split into single-declaration objects"
    );
    let t0_params = &tools[0]["functionDeclarations"][0]["parameters"];
    assert_eq!(t0_params["type"], "OBJECT");
    assert_eq!(t0_params["properties"]["cmd"]["type"], "STRING");

    // 5. Envelope has no enabledCreditTypes
    assert_eq!(val.get("enabledCreditTypes"), None);
}
