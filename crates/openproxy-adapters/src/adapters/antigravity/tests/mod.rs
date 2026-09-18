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
    assert_eq!(parts.len(), 2, "must prepend placeholder thought block before functionCall");
    
    // Part 0: prepended placeholder thought
    assert_eq!(parts[0]["thought"], true);
    assert_eq!(parts[0]["text"], "...");
    assert_eq!(parts[0]["thoughtSignature"], "skip_thought_signature_validator");

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
        reset_dynamic_overrides, set_dynamic_extra_header, set_dynamic_version,
        ANTIGRAVITY_TEST_LOCK,
    };

    let _guard = ANTIGRAVITY_TEST_LOCK.lock().unwrap();
    reset_dynamic_overrides();
    let mut a = AntigravityAdapter::new();
    let cfg = a.config_mut().expect("config_mut");
    cfg.extra_headers.push(("x-admin-injected".into(), "true".into()));
    cfg.extra_headers.push(("x-goog-user-project".into(), "override-proj".into()));

    set_dynamic_version("4.9.0");
    set_dynamic_extra_header("x-antigravity-custom-edge", "edge-val");

    let headers = a.build_headers("ya29.test", TargetFormat::Gemini, &ModelId::new("gemini-1.5-pro"));
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
    use crate::antigravity_headers::{reset_dynamic_overrides, ANTIGRAVITY_TEST_LOCK};

    let _guard = ANTIGRAVITY_TEST_LOCK.lock().unwrap();
    reset_dynamic_overrides();
    let a = AntigravityAdapter::new();
    let headers = a.build_headers("token-xyz", TargetFormat::Gemini, &ModelId::new("gemini-1.5-pro"));

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
    assert!(get("x-machine-id").is_some_and(|id| id.len() == 32 && id.chars().all(|c| c.is_ascii_hexdigit())));
    assert!(get("x-vscode-sessionid").is_some_and(|s| !s.is_empty()));
}
